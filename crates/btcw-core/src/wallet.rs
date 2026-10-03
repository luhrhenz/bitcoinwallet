//! [`WalletService`]: a BDK wallet persisted to SQLite, plus read-only views of its state.
//!
//! OWNER: Agent C (Phase 1). Contract: PLAN §4, §5.3–5.7.
//!
//! - One wallet per `<datadir>/<network>/wallet.sqlite`.
//! - While open, holds an exclusive `File::try_lock` on `cfg.lock_path()` so the CLI and the
//!   desktop app can't write the same wallet at once (`WalletInUse`).
//! - The wallet holds **public descriptors only**; signing lives in `keys::Signer`. So `open`
//!   needs no password: it loads descriptors from the database, checks the network
//!   (`.check_network(..)`), and, if `expected` is given, checks they match
//!   (`.descriptor(keychain, Some(..))`).
//! - Stores a *birthday height* in its own SQLite table (`btcw_meta`, same file). `Node::sync`
//!   starts scanning there, so a new wallet never rescans the whole chain.
//! - Confirmation counts come from the wallet's own latest checkpoint (the synced tip),
//!   so the views work offline.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

use bdk_wallet::chain::ChainPosition;
use bdk_wallet::rusqlite::{Connection, OpenFlags, OptionalExtension};
use bdk_wallet::{
    CreateWithPersistError, KeychainKind, LoadError, LoadMismatch, LoadWithPersistError,
    PersistedWallet, Wallet,
};

use crate::bitcoin::{Address, Network, Script};
use crate::config::Config;
use crate::error::{Result, WalletError};
use crate::keys::Descriptors;
use crate::types::{AddressRow, BalanceView, Keychain, TxRow, TxStatus, UtxoRow};

/// Addresses scanned beyond the last used one (BIP44 gap limit is 20).
pub const LOOKAHEAD: u32 = 25;

/// Our own key/value table, created next to BDK's tables in the same SQLite file.
const CREATE_META_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS btcw_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)";
const BIRTHDAY_KEY: &str = "birthday_height";

pub struct WalletService {
    wallet: PersistedWallet<Connection>,
    db: Connection,
    network: Network,
    /// Read once at open time: `birthday_height()` is infallible, and the value never changes.
    birthday: u32,
    /// Declared last so it is dropped last: the lock is released only after the database
    /// connection has been closed.
    _lock: File,
}

impl std::fmt::Debug for WalletService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletService")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl WalletService {
    /// Create a brand-new wallet database. `WalletExists` if one is already there.
    /// `birthday`: first block height that can contain our transactions (`None` = genesis).
    pub fn create(cfg: &Config, descriptors: &Descriptors, birthday: Option<u32>) -> Result<Self> {
        let lock = acquire_lock(cfg)?;
        // Checked under the lock, so another btcw process can't create the file in between.
        let db_path = cfg.wallet_db_path();
        if db_path.exists() {
            return Err(WalletError::WalletExists(cfg.network_dir()));
        }
        let birthday = birthday.unwrap_or(0);

        match create_db(cfg, &db_path, descriptors, birthday) {
            Ok((wallet, db)) => Ok(Self {
                wallet,
                db,
                network: cfg.network,
                birthday,
                _lock: lock,
            }),
            Err(e) => {
                // `create_db` owned the connection, so it is closed by now. Remove the partial
                // file, otherwise every retry would fail with `WalletExists`.
                remove_partial_db(&db_path);
                Err(e)
            }
        }
    }

    /// Open an existing wallet database. `WalletNotFound` if missing; `NetworkMismatch` if it
    /// belongs to another network; `Persist` if `expected` descriptors don't match.
    pub fn open(cfg: &Config, expected: Option<&Descriptors>) -> Result<Self> {
        let db_path = cfg.wallet_db_path();
        // Fail before creating the network directory or lock file: opening a wallet that isn't
        // there should leave no trace (e.g. `btcw balance --network signet` by mistake).
        if !db_path.exists() {
            return Err(WalletError::WalletNotFound(cfg.network_dir()));
        }
        let lock = acquire_lock(cfg)?;

        // No SQLITE_OPEN_CREATE: if the file vanished after the check above, SQLite must fail
        // rather than silently create an empty database.
        let mut db = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| match e.sqlite_error_code() {
            Some(bdk_wallet::rusqlite::ErrorCode::CannotOpen) => {
                WalletError::WalletNotFound(cfg.network_dir())
            }
            _ => persist_err(format!("opening {}", db_path.display()), e),
        })?;

        let mut params = Wallet::load()
            .check_network(cfg.network)
            .lookahead(LOOKAHEAD);
        if let Some(expected) = expected {
            params = params
                .descriptor(KeychainKind::External, Some(expected.external().to_owned()))
                .descriptor(KeychainKind::Internal, Some(expected.internal().to_owned()));
        }
        let wallet = match params.load_wallet(&mut db) {
            Ok(Some(wallet)) => wallet,
            // The file exists but holds no wallet (e.g. an empty SQLite file).
            Ok(None) => return Err(WalletError::WalletNotFound(cfg.network_dir())),
            Err(e) => return Err(load_error(cfg.network, &db_path, e)),
        };

        let birthday = read_birthday(&db)?;
        Ok(Self {
            wallet,
            db,
            network: cfg.network,
            birthday,
            _lock: lock,
        })
    }

    pub fn network(&self) -> Network {
        self.network
    }

    /// Height sync starts from on a fresh wallet (0 = genesis).
    pub fn birthday_height(&self) -> u32 {
        self.birthday
    }

    /// Height of the wallet's latest checkpoint (0 before the first sync).
    pub fn synced_height(&self) -> u32 {
        self.wallet.latest_checkpoint().height()
    }

    /// Next unused receive address (reuses a revealed-but-unused one). Persists the reveal.
    pub fn new_address(&mut self) -> Result<AddressRow> {
        let info = self.wallet.next_unused_address(KeychainKind::External);
        // Persist before handing the address out, as BDK's docs require: otherwise, after a
        // restart the revealed index would go backwards and `addresses()` would no longer list
        // an address the user may already have shared.
        self.persist()?;
        Ok(AddressRow {
            index: info.index,
            address: info.address.to_string(),
            keychain: Keychain::External,
            used: false,
        })
    }

    /// All revealed receive addresses, ascending index, with `used` flags.
    pub fn addresses(&self) -> Vec<AddressRow> {
        let index = self.wallet.spk_index();
        index
            .revealed_keychain_spks(KeychainKind::External)
            .filter_map(|(i, spk)| {
                let Some(address) = self.address_of(&spk) else {
                    // Unreachable for our wpkh descriptors; skip rather than panic.
                    tracing::warn!(index = i, "revealed script has no address form");
                    return None;
                };
                Some(AddressRow {
                    index: i,
                    address,
                    keychain: Keychain::External,
                    used: index.is_used(KeychainKind::External, i),
                })
            })
            .collect()
    }

    pub fn balance(&self) -> BalanceView {
        let b = self.wallet.balance();
        let confirmed_sat = b.confirmed.to_sat();
        let unconfirmed_sat = b
            .trusted_pending
            .to_sat()
            .saturating_add(b.untrusted_pending.to_sat());
        let immature_sat = b.immature.to_sat();
        BalanceView {
            confirmed_sat,
            unconfirmed_sat,
            immature_sat,
            total_sat: confirmed_sat
                .saturating_add(unconfirmed_sat)
                .saturating_add(immature_sat),
        }
    }

    /// Wallet transactions: unconfirmed first, then confirmed by height descending.
    pub fn history(&self) -> Vec<TxRow> {
        let tip = self.synced_height();
        let mut rows: Vec<TxRow> = self
            .wallet
            .transactions()
            .map(|wtx| {
                let tx = &wtx.tx_node.tx;
                let (sent, received) = self.wallet.sent_and_received(tx);
                let (sent_sat, received_sat) = (sent.to_sat(), received.to_sat());
                TxRow {
                    txid: wtx.tx_node.txid.to_string(),
                    received_sat,
                    sent_sat,
                    net_sat: net_sat(received_sat, sent_sat),
                    // Errors when an input's previous output is unknown to the wallet, which is
                    // normal for incoming payments: the sender's coins aren't ours.
                    fee_sat: self.wallet.calculate_fee(tx).ok().map(|fee| fee.to_sat()),
                    status: tx_status(&wtx.chain_position, tip),
                }
            })
            .collect();
        sort_history(&mut rows);
        rows
    }

    pub fn utxos(&self) -> Vec<UtxoRow> {
        self.wallet
            .list_unspent()
            .map(|utxo| UtxoRow {
                outpoint: utxo.outpoint.to_string(),
                value_sat: utxo.txout.value.to_sat(),
                address: self.address_of(&utxo.txout.script_pubkey),
                keychain: keychain_view(utxo.keychain),
                derivation_index: utxo.derivation_index,
                confirmed: utxo.chain_position.is_confirmed(),
            })
            .collect()
    }

    /// Write staged changes to SQLite.
    pub fn persist(&mut self) -> Result<()> {
        self.wallet
            .persist(&mut self.db)
            .map(|_changed| ())
            .map_err(|e| persist_err("saving wallet changes", e))
    }

    /// Escape hatches for `chain` and `tx`. Not public API.
    pub(crate) fn bdk(&self) -> &PersistedWallet<Connection> {
        &self.wallet
    }

    pub(crate) fn bdk_mut(&mut self) -> &mut PersistedWallet<Connection> {
        &mut self.wallet
    }

    fn address_of(&self, spk: &Script) -> Option<String> {
        Address::from_script(spk, self.network)
            .ok()
            .map(|a| a.to_string())
    }
}

/// Take the per-network lock, creating the directory and lock file if needed.
///
/// The lock file is never deleted: removing it while another process waits on it would let
/// two processes hold "the" lock on two different inodes.
fn acquire_lock(cfg: &Config) -> Result<File> {
    let dir = cfg.network_dir();
    std::fs::create_dir_all(&dir).map_err(|e| io_err("creating", &dir, e))?;
    let path = cfg.lock_path();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // The file's contents are irrelevant; never truncate it under another holder.
        .truncate(false)
        .open(&path)
        .map_err(|e| io_err("opening lock file", &path, e))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(WalletError::WalletInUse),
        Err(TryLockError::Error(e)) => Err(io_err("locking", &path, e)),
    }
}

/// Everything in `create` that touches the database file. On error the caller deletes the
/// file, so nothing here needs its own cleanup.
fn create_db(
    cfg: &Config,
    db_path: &Path,
    descriptors: &Descriptors,
    birthday: u32,
) -> Result<(PersistedWallet<Connection>, Connection)> {
    let mut db = Connection::open(db_path)
        .map_err(|e| persist_err(format!("creating {}", db_path.display()), e))?;
    let wallet = Wallet::create(
        descriptors.external().to_owned(),
        descriptors.internal().to_owned(),
    )
    .network(cfg.network)
    .lookahead(LOOKAHEAD)
    .create_wallet(&mut db)
    .map_err(|e| match e {
        CreateWithPersistError::Persist(e) => persist_err("writing the new wallet", e),
        CreateWithPersistError::DataAlreadyExists(_) => {
            WalletError::WalletExists(cfg.network_dir())
        }
        // Descriptor errors describe the problem (checksum, key type, ...) without echoing keys.
        CreateWithPersistError::Descriptor(e) => {
            WalletError::Config(format!("invalid wallet descriptors: {e}"))
        }
    })?;
    // Written after the wallet on purpose: if we crash between the two, the wallet opens with a
    // missing birthday, which reads as 0 (scan from genesis). Slow, but it can't miss funds.
    write_birthday(&db, birthday)?;
    Ok((wallet, db))
}

fn write_birthday(db: &Connection, birthday: u32) -> Result<()> {
    db.execute(CREATE_META_TABLE, [])
        .and_then(|_| {
            db.execute(
                "INSERT INTO btcw_meta (key, value) VALUES (?1, ?2)",
                (BIRTHDAY_KEY, birthday.to_string()),
            )
        })
        .map(|_rows| ())
        .map_err(|e| persist_err("storing the wallet birthday", e))
}

/// A missing table or row means "no birthday recorded" and reads as 0 (genesis).
fn read_birthday(db: &Connection) -> Result<u32> {
    // Create-if-missing instead of querying `sqlite_master`: databases from a crashed `create`
    // (see `create_db`) may lack the table, and an empty table answers the query the same way.
    let value: Option<String> = db
        .execute(CREATE_META_TABLE, [])
        .and_then(|_| {
            db.query_row(
                "SELECT value FROM btcw_meta WHERE key = ?1",
                [BIRTHDAY_KEY],
                |row| row.get(0),
            )
            .optional()
        })
        .map_err(|e| persist_err("reading the wallet birthday", e))?;
    match value {
        None => Ok(0),
        Some(v) => v.parse().map_err(|_| {
            WalletError::Persist(format!(
                "stored wallet birthday `{v}` is not a block height"
            ))
        }),
    }
}

/// Best effort: the original error matters more than a failed cleanup, so this only logs.
fn remove_partial_db(db_path: &Path) {
    let mut journal = db_path.as_os_str().to_owned();
    journal.push("-journal");
    for path in [db_path, Path::new(&journal)] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not remove partial wallet database")
            }
        }
    }
}

fn load_error(
    expected: Network,
    db_path: &Path,
    e: LoadWithPersistError<bdk_wallet::rusqlite::Error>,
) -> WalletError {
    match e {
        LoadWithPersistError::Persist(e) => {
            persist_err(format!("reading {}", db_path.display()), e)
        }
        LoadWithPersistError::InvalidChangeSet(LoadError::Mismatch(LoadMismatch::Network {
            loaded,
            ..
        })) => WalletError::NetworkMismatch {
            expected,
            found: loaded.to_string(),
        },
        // Only reachable with `expected` descriptors: the seed that was unlocked is not the one
        // this database was created from.
        LoadWithPersistError::InvalidChangeSet(LoadError::Mismatch(LoadMismatch::Descriptor {
            keychain,
            ..
        })) => WalletError::Persist(format!(
            "wallet database does not belong to this seed ({keychain} descriptor differs)"
        )),
        LoadWithPersistError::InvalidChangeSet(e) => WalletError::Persist(format!(
            "wallet database {} is incomplete or corrupt: {e}",
            db_path.display()
        )),
    }
}

fn persist_err(context: impl std::fmt::Display, e: impl std::fmt::Display) -> WalletError {
    WalletError::Persist(format!("{context}: {e}"))
}

/// Keep `WalletError::Io` (and its `io` code) but say which file failed.
fn io_err(action: &str, path: &Path, e: io::Error) -> WalletError {
    WalletError::Io(io::Error::new(
        e.kind(),
        format!("{action} {}: {e}", path.display()),
    ))
}

fn keychain_view(k: KeychainKind) -> Keychain {
    match k {
        KeychainKind::External => Keychain::External,
        KeychainKind::Internal => Keychain::Internal,
    }
}

/// `received - sent` without `as` casts. Real amounts are far below `i64::MAX` (21M BTC is
/// about 2^51 sat), so saturation is a guard, never a visible result.
fn net_sat(received: u64, sent: u64) -> i64 {
    let net = i128::from(received) - i128::from(sent);
    i64::try_from(net).unwrap_or(if net < 0 { i64::MIN } else { i64::MAX })
}

fn tx_status(pos: &ChainPosition<bdk_wallet::chain::ConfirmationBlockTime>, tip: u32) -> TxStatus {
    match pos {
        // `transitively` confirmed means a descendant confirmed at this anchor, so our tx is at
        // or below it; the count is then a lower bound, which is the safe direction.
        ChainPosition::Confirmed { anchor, .. } => {
            let height = anchor.block_id.height;
            TxStatus::Confirmed {
                height,
                // The anchor is in our chain, so tip >= height; saturating keeps it >= 1 anyway.
                confirmations: tip.saturating_sub(height).saturating_add(1),
                block_time: anchor.confirmation_time,
            }
        }
        ChainPosition::Unconfirmed { first_seen, .. } => TxStatus::Unconfirmed {
            first_seen: *first_seen,
        },
    }
}

/// Unconfirmed first (newest `first_seen` first, unknown last), then confirmed by height
/// descending. Ties break on txid so the output never depends on BDK's iteration order.
fn sort_history(rows: &mut [TxRow]) {
    fn rank(s: &TxStatus) -> (u8, std::cmp::Reverse<Option<u64>>) {
        match s {
            // `Reverse(Some(..))` sorts before `Reverse(None)`, and larger times first.
            TxStatus::Unconfirmed { first_seen } => (0, std::cmp::Reverse(*first_seen)),
            TxStatus::Confirmed { height, .. } => (1, std::cmp::Reverse(Some(u64::from(*height)))),
        }
    }
    rows.sort_by(|a, b| {
        rank(&a.status)
            .cmp(&rank(&b.status))
            .then_with(|| a.txid.cmp(&b.txid))
    });
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::path::Path;

    use bdk_wallet::KeychainKind;

    use super::*;
    use crate::bitcoin::absolute::LockTime;
    use crate::bitcoin::hashes::Hash;
    use crate::bitcoin::transaction::Version;
    use crate::bitcoin::{Amount, OutPoint, Transaction, TxIn, TxOut, Txid};
    use crate::config::Overrides;
    use crate::keys;

    type TestResult = std::result::Result<(), Box<dyn Error>>;

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn regtest_config(datadir: &Path) -> Result<Config> {
        Config::load_with(
            Overrides {
                datadir: Some(datadir.to_path_buf()),
                network: Some("regtest".into()),
                ..Default::default()
            },
            |_| None,
        )
    }

    fn descriptors() -> Result<Descriptors> {
        let (descriptors, _signer) =
            keys::derive_account(&keys::parse_mnemonic(ABANDON)?, "", Network::Regtest, 0)?;
        Ok(descriptors)
    }

    /// A made-up unconfirmed transaction paying `sat` to `to`. BDK doesn't need the parent of
    /// the dummy outpoint to track the output that pays us.
    fn funding_tx(to: &str, sat: u64) -> std::result::Result<Transaction, Box<dyn Error>> {
        let address = to
            .parse::<Address<crate::bitcoin::address::NetworkUnchecked>>()?
            .require_network(Network::Regtest)?;
        Ok(Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([7; 32]), 0),
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(sat),
                script_pubkey: address.script_pubkey(),
            }],
        })
    }

    fn row(txid: char, status: TxStatus) -> TxRow {
        TxRow {
            txid: txid.to_string().repeat(64),
            received_sat: 0,
            sent_sat: 0,
            net_sat: 0,
            fee_sat: None,
            status,
        }
    }

    fn confirmed(height: u32) -> TxStatus {
        TxStatus::Confirmed {
            height,
            confirmations: 1,
            block_time: 0,
        }
    }

    #[test]
    fn history_sort_is_deterministic() {
        let mut rows = vec![
            row('a', confirmed(5)),
            row('b', TxStatus::Unconfirmed { first_seen: None }),
            row('c', confirmed(9)),
            row(
                'd',
                TxStatus::Unconfirmed {
                    first_seen: Some(100),
                },
            ),
            row('e', confirmed(5)),
            row(
                'f',
                TxStatus::Unconfirmed {
                    first_seen: Some(200),
                },
            ),
        ];
        sort_history(&mut rows);
        let order: String = rows.iter().filter_map(|r| r.txid.chars().next()).collect();
        // Unconfirmed newest-first with unknown time last, then height 9, then height 5 by txid.
        assert_eq!(order, "fdbcae");
    }

    #[test]
    fn net_amount_is_signed_and_saturates() {
        assert_eq!(net_sat(100, 30), 70);
        assert_eq!(net_sat(30, 100), -70);
        assert_eq!(net_sat(u64::MAX, 0), i64::MAX);
        assert_eq!(net_sat(0, u64::MAX), i64::MIN);
    }

    #[test]
    fn receive_offline_then_survive_reopen() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cfg = regtest_config(dir.path())?;
        let descriptors = descriptors()?;
        let mut wallet = WalletService::create(&cfg, &descriptors, Some(0))?;

        // Handing out an address twice without a payment must not skip ahead (no gaps).
        let first = wallet.new_address()?;
        assert_eq!(first.index, 0);
        assert!(first.address.starts_with("bcrt1q"), "{}", first.address);
        assert!(!first.used);
        assert_eq!(wallet.new_address()?, first);
        assert_eq!(wallet.addresses(), vec![first.clone()]);

        let funding = funding_tx(&first.address, 100_000)?;
        let txid = funding.compute_txid();
        let seen_at = 1_700_000_000;
        wallet.bdk_mut().apply_unconfirmed_txs([(funding, seen_at)]);

        let addresses = wallet.addresses();
        assert_eq!(addresses.len(), 1);
        assert!(addresses[0].used);
        let second = wallet.new_address()?;
        assert_eq!(second.index, 1);
        assert_ne!(second.address, first.address);

        let expected_balance = BalanceView {
            confirmed_sat: 0,
            unconfirmed_sat: 100_000,
            immature_sat: 0,
            total_sat: 100_000,
        };
        assert_eq!(wallet.balance(), expected_balance);

        let expected_history = vec![TxRow {
            txid: txid.to_string(),
            received_sat: 100_000,
            sent_sat: 0,
            net_sat: 100_000,
            fee_sat: None,
            status: TxStatus::Unconfirmed {
                first_seen: Some(seen_at),
            },
        }];
        assert_eq!(wallet.history(), expected_history);

        let expected_utxos = vec![UtxoRow {
            outpoint: format!("{txid}:0"),
            value_sat: 100_000,
            address: Some(first.address.clone()),
            keychain: Keychain::External,
            derivation_index: 0,
            confirmed: false,
        }];
        assert_eq!(wallet.utxos(), expected_utxos);

        // `new_address` persisted the reveal; the applied tx is still only staged.
        wallet.persist()?;
        drop(wallet);

        let mut reopened = WalletService::open(&cfg, Some(&descriptors))?;
        let addresses = reopened.addresses();
        assert_eq!(addresses.len(), 2);
        assert!(addresses[0].used && !addresses[1].used);
        assert_eq!(reopened.balance(), expected_balance);
        assert_eq!(reopened.history(), expected_history);
        assert_eq!(reopened.utxos(), expected_utxos);
        // Index 1 was revealed but never paid, so it's handed out again, not index 2.
        assert_eq!(reopened.new_address()?, second);
        assert_eq!(
            reopened
                .bdk()
                .spk_index()
                .last_revealed_index(KeychainKind::External),
            Some(1)
        );
        Ok(())
    }

    /// Against a real regtest node: receive, confirm, count confirmations, reopen.
    /// Skipped (not failed) when no `bitcoind` is available.
    #[cfg(feature = "test-utils")]
    mod node {
        use bdk_bitcoind_rpc::bitcoincore_rpc::{Auth, Client};
        use bdk_bitcoind_rpc::{Emitter, NO_EXPECTED_MEMPOOL_TXS};

        use super::*;
        use crate::config::RpcAuth;
        use crate::testnode::TestNode;

        /// Minimal stand-in for Agent B's `Node::sync`: blocks from the birthday, then mempool.
        fn sync(wallet: &mut WalletService, client: &Client) -> TestResult {
            let start_height = wallet.birthday_height();
            let bdk = wallet.bdk_mut();
            let mut emitter = Emitter::new(
                client,
                bdk.latest_checkpoint(),
                start_height,
                NO_EXPECTED_MEMPOOL_TXS,
            );
            while let Some(ev) = emitter.next_block()? {
                bdk.apply_block_connected_to(&ev.block, ev.block_height(), ev.connected_to())?;
            }
            bdk.apply_unconfirmed_txs(emitter.mempool()?.update);
            wallet.persist()?;
            Ok(())
        }

        fn confirmations(wallet: &WalletService) -> std::result::Result<u32, Box<dyn Error>> {
            match wallet.history().as_slice() {
                [
                    TxRow {
                        status: TxStatus::Confirmed { confirmations, .. },
                        ..
                    },
                ] => Ok(*confirmations),
                other => Err(format!("expected one confirmed tx, got {other:?}").into()),
            }
        }

        #[test]
        fn confirmations_follow_the_synced_tip() -> TestResult {
            if !TestNode::available() {
                eprintln!("skipping: no bitcoind (set BITCOIND_EXE)");
                return Ok(());
            }
            let node = TestNode::start()?;
            let rpc = node.rpc_config();
            let RpcAuth::Cookie(cookie) = rpc.auth else {
                return Err("test node uses cookie auth".into());
            };
            let client = Client::new(&rpc.url, Auth::CookieFile(cookie))?;

            let dir = tempfile::tempdir()?;
            let cfg = regtest_config(dir.path())?;
            let birthday = node.tip_height()?;
            let mut wallet = WalletService::create(&cfg, &descriptors()?, Some(birthday))?;
            assert_eq!(wallet.birthday_height(), birthday);
            let address = wallet.new_address()?;
            let to = address
                .address
                .parse::<Address<crate::bitcoin::address::NetworkUnchecked>>()?
                .require_network(Network::Regtest)?;

            let txid = node.fund(&to, Amount::from_sat(1_000_000))?;
            sync(&mut wallet, &client)?;
            assert_eq!(wallet.balance().unconfirmed_sat, 1_000_000);
            assert!(matches!(
                wallet.history()[..],
                [TxRow {
                    status: TxStatus::Unconfirmed {
                        first_seen: Some(_)
                    },
                    ..
                }]
            ));

            node.mine(1)?;
            sync(&mut wallet, &client)?;
            let tip = node.tip_height()?;
            assert_eq!(wallet.synced_height(), tip);
            assert_eq!(
                wallet.balance(),
                BalanceView {
                    confirmed_sat: 1_000_000,
                    unconfirmed_sat: 0,
                    immature_sat: 0,
                    total_sat: 1_000_000,
                }
            );
            let history = wallet.history();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].txid, txid.to_string());
            assert_eq!(history[0].net_sat, 1_000_000);
            match history[0].status {
                TxStatus::Confirmed {
                    height,
                    confirmations,
                    block_time,
                } => {
                    assert_eq!(height, tip);
                    assert_eq!(confirmations, 1);
                    assert!(block_time > 0);
                }
                ref other => return Err(format!("expected confirmed, got {other:?}").into()),
            }
            let utxos = wallet.utxos();
            assert_eq!(utxos.len(), 1);
            assert!(utxos[0].confirmed);
            assert!(wallet.addresses()[0].used);

            node.mine(2)?;
            sync(&mut wallet, &client)?;
            assert_eq!(confirmations(&wallet)?, 3);
            let balance = wallet.balance();
            let history = wallet.history();
            drop(wallet);

            // Everything above came from SQLite: no node needed to see it again.
            let reopened = WalletService::open(&cfg, None)?;
            assert_eq!(reopened.synced_height(), tip + 2);
            assert_eq!(reopened.birthday_height(), birthday);
            assert_eq!(reopened.balance(), balance);
            assert_eq!(reopened.history(), history);
            assert_eq!(confirmations(&reopened)?, 3);
            Ok(())
        }
    }
}
