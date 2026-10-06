//! [`WalletService`]: a BDK wallet persisted to SQLite, plus read-only views of its state.
//!
//! - One wallet per `<datadir>/<network>/wallet.sqlite`.
//! - While open, holds an exclusive lock on `cfg.lock_path()` so the CLI and the desktop app
//!   can't write the same wallet at once (`WalletInUse`).
//! - Public descriptors only; signing lives in `keys::Signer`, so `open` needs no password.
//! - A birthday height in our own `btcw_meta` table tells `Node::sync` where to start scanning.
//! - Confirmations count from the wallet's synced tip, so the views work offline.
//! - Contacts and labels (`book.rs`) are two more tables in the same file; labels are cached in
//!   memory so `history()` stays infallible.

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use bdk_wallet::chain::ChainPosition;
use bdk_wallet::rusqlite::{Connection, OpenFlags, OptionalExtension};
use bdk_wallet::{
    CreateWithPersistError, KeychainKind, LoadError, LoadMismatch, LoadWithPersistError,
    PersistedWallet, Wallet,
};

use crate::bitcoin::{Address, Network, Script, Txid};
use crate::book;
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
/// `"1"` once the user has verified the recovery phrase. A missing row means not verified, so a
/// crash can only cause an extra reminder, never a missing one.
const BACKUP_KEY: &str = "backup_verified";

pub struct WalletService {
    wallet: PersistedWallet<Connection>,
    db: Connection,
    network: Network,
    /// `<datadir>/<network>/`, for files that live next to the database (the mempool cache).
    network_dir: PathBuf,
    /// Read once at open time; it never changes.
    birthday: u32,
    /// Cached `backup_verified` row.
    backup_verified: bool,
    /// Copy of `btcw_labels`. Only this process writes while it holds the lock, so it can't go
    /// stale.
    labels: HashMap<Txid, String>,
    /// Declared last so the lock is released only after the database connection closes.
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
        // Checked under the lock, so another process can't create the file in between.
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
                network_dir: cfg.network_dir(),
                birthday,
                // No row yet: a new wallet starts unverified (`api` marks restores verified).
                backup_verified: false,
                labels: HashMap::new(),
                _lock: lock,
            }),
            Err(e) => {
                // Remove the partial file, or every retry would fail with `WalletExists`.
                remove_partial_db(&db_path);
                Err(e)
            }
        }
    }

    /// Open an existing wallet database. `WalletNotFound` if missing; `NetworkMismatch` if it
    /// belongs to another network; `Persist` if `expected` descriptors don't match.
    pub fn open(cfg: &Config, expected: Option<&Descriptors>) -> Result<Self> {
        let db_path = cfg.wallet_db_path();
        // Fail before creating the directory or lock file, so a missing wallet leaves no trace.
        if !db_path.exists() {
            return Err(WalletError::WalletNotFound(cfg.network_dir()));
        }
        let lock = acquire_lock(cfg)?;

        // No SQLITE_OPEN_CREATE: if the file vanished since the check, fail rather than create it.
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
        let mut wallet = match params.load_wallet(&mut db) {
            Ok(Some(wallet)) => wallet,
            // The file exists but holds no wallet (e.g. an empty SQLite file).
            Ok(None) => return Err(WalletError::WalletNotFound(cfg.network_dir())),
            Err(e) => return Err(load_error(cfg.network, &db_path, e)),
        };

        restore_first_seen(&mut wallet, &db)?;
        let birthday = read_birthday(&db)?;
        let backup_verified = read_meta(&db, BACKUP_KEY)?.as_deref() == Some("1");
        // Older wallets have no address-book tables yet.
        book::create_tables(&db)?;
        let labels = book::load_labels(&db)?;
        Ok(Self {
            wallet,
            db,
            network: cfg.network,
            network_dir: cfg.network_dir(),
            birthday,
            backup_verified,
            labels,
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

    /// Whether the user has proved they hold a correct copy of the recovery phrase.
    pub fn backup_verified(&self) -> bool {
        self.backup_verified
    }

    /// Record the backup state; written straight to the database (not staged with BDK's data).
    pub fn set_backup_verified(&mut self, verified: bool) -> Result<()> {
        self.db
            .execute(CREATE_META_TABLE, [])
            .and_then(|_| {
                self.db.execute(
                    "INSERT OR REPLACE INTO btcw_meta (key, value) VALUES (?1, ?2)",
                    (BACKUP_KEY, if verified { "1" } else { "0" }),
                )
            })
            .map_err(|e| persist_err("storing the backup state", e))?;
        self.backup_verified = verified;
        Ok(())
    }

    /// The backup flag without opening the wallet (no lock, read-only), so it works while
    /// another process has it open. `None` if there is no wallet.
    pub fn read_backup_verified(cfg: &Config) -> Result<Option<bool>> {
        let db_path = cfg.wallet_db_path();
        if !db_path.exists() {
            return Ok(None);
        }
        let db = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| persist_err(format!("opening {}", db_path.display()), e))?;
        let has_table: bool = db
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'btcw_meta')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| persist_err("reading the backup state", e))?;
        if !has_table {
            return Ok(Some(false));
        }
        let value: Option<String> = db
            .query_row(
                "SELECT value FROM btcw_meta WHERE key = ?1",
                [BACKUP_KEY],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| persist_err("reading the backup state", e))?;
        Ok(Some(value.as_deref() == Some("1")))
    }

    /// Height of the wallet's latest checkpoint (0 before the first sync).
    pub fn synced_height(&self) -> u32 {
        self.wallet.latest_checkpoint().height()
    }

    /// Next unused receive address (reuses a revealed-but-unused one). Persists the reveal.
    pub fn new_address(&mut self) -> Result<AddressRow> {
        let info = self.wallet.next_unused_address(KeychainKind::External);
        // Persist before handing it out (as BDK requires), or after a restart the revealed
        // index could go backwards and forget an address the user already shared.
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
                let txid = wtx.tx_node.txid;
                TxRow {
                    txid: txid.to_string(),
                    received_sat,
                    sent_sat,
                    net_sat: net_sat(received_sat, sent_sat),
                    // Unknown for incoming payments: the sender's inputs aren't ours.
                    fee_sat: self.wallet.calculate_fee(tx).ok().map(|fee| fee.to_sat()),
                    status: tx_status(&wtx.chain_position, tip),
                    label: self.labels.get(&txid).cloned(),
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

    /// True if `descriptors` are this wallet's (both keychains).
    pub fn matches(&self, descriptors: &Descriptors) -> bool {
        self.wallet
            .public_descriptor(KeychainKind::External)
            .to_string()
            == descriptors.external()
            && self
                .wallet
                .public_descriptor(KeychainKind::Internal)
                .to_string()
                == descriptors.internal()
    }

    /// Where `chain::Node::sync` keeps the mempool transactions it has already downloaded.
    pub(crate) fn mempool_cache_path(&self) -> PathBuf {
        self.network_dir.join("mempool-cache.txt")
    }

    /// Escape hatches for `chain` and `tx`. Not public API.
    pub(crate) fn bdk(&self) -> &PersistedWallet<Connection> {
        &self.wallet
    }

    pub(crate) fn bdk_mut(&mut self) -> &mut PersistedWallet<Connection> {
        &mut self.wallet
    }

    /// For `book.rs`: our own tables live in the same SQLite file.
    pub(crate) fn db(&self) -> &Connection {
        &self.db
    }

    pub(crate) fn labels(&self) -> &HashMap<Txid, String> {
        &self.labels
    }

    pub(crate) fn labels_mut(&mut self) -> &mut HashMap<Txid, String> {
        &mut self.labels
    }

    fn address_of(&self, spk: &Script) -> Option<String> {
        Address::from_script(spk, self.network)
            .ok()
            .map(|a| a.to_string())
    }
}

/// Take the per-network lock, creating the directory and lock file if needed.
///
/// The lock file is never deleted: that would let two processes lock two different inodes.
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

/// Everything in `create` that touches the database file; the caller deletes it on error.
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
    // Written after the wallet: a crash in between leaves no birthday, which reads as 0
    // (scan from genesis). Slow, but it can't miss funds.
    write_birthday(&db, birthday)?;
    book::create_tables(&db)?;
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
    match read_meta(db, BIRTHDAY_KEY)? {
        None => Ok(0),
        Some(v) => v.parse().map_err(|_| {
            WalletError::Persist(format!(
                "stored wallet birthday `{v}` is not a block height"
            ))
        }),
    }
}

/// Workaround for bdk_chain 0.23.3: `TxGraph::apply_changeset` (used by `Wallet::load`) drops
/// the stored `first_seen`, so after a reopen a transaction looks first seen at its latest
/// sighting. Feed the on-disk value back in: BDK only ever lowers `first_seen` and never moves
/// `last_seen` backwards, so nothing else changes.
fn restore_first_seen(wallet: &mut PersistedWallet<Connection>, db: &Connection) -> Result<()> {
    let mut statement = db
        .prepare("SELECT txid, first_seen FROM bdk_txs WHERE first_seen IS NOT NULL")
        .map_err(|e| persist_err("reading first-seen times", e))?;
    let stored: Vec<(String, u64)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .and_then(|rows| rows.collect())
        .map_err(|e| persist_err("reading first-seen times", e))?;

    let mut sightings = Vec::new();
    for (txid, first_seen) in stored {
        let Ok(txid) = txid.parse::<crate::bitcoin::Txid>() else {
            continue; // Not something BDK wrote; nothing to repair.
        };
        if let Some(wtx) = wallet.get_tx(txid)
            && wtx
                .tx_node
                .first_seen
                .is_none_or(|current| first_seen < current)
        {
            sightings.push((wtx.tx_node.tx.clone(), first_seen));
        }
    }
    if !sightings.is_empty() {
        wallet.apply_unconfirmed_txs(sightings);
    }
    Ok(())
}

/// One `btcw_meta` value (`None` if the row is missing).
fn read_meta(db: &Connection, key: &str) -> Result<Option<String>> {
    // A database from a crashed `create` may lack the table; an empty one answers the same.
    db.execute(CREATE_META_TABLE, [])
        .and_then(|_| {
            db.query_row("SELECT value FROM btcw_meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
        })
        .map_err(|e| persist_err(format!("reading `{key}` from the wallet database"), e))
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

pub(crate) fn persist_err(
    context: impl std::fmt::Display,
    e: impl std::fmt::Display,
) -> WalletError {
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

/// `received - sent` without `as` casts; 21M BTC is about 2^51 sat, so saturation never shows.
fn net_sat(received: u64, sent: u64) -> i64 {
    let net = i128::from(received) - i128::from(sent);
    i64::try_from(net).unwrap_or(if net < 0 { i64::MIN } else { i64::MAX })
}

/// Shared with `tx::tx_status`, so history rows and `btcw status` always agree.
pub(crate) fn tx_status(
    pos: &ChainPosition<bdk_wallet::chain::ConfirmationBlockTime>,
    tip: u32,
) -> TxStatus {
    match pos {
        // If confirmed `transitively` (via a descendant), the count is a lower bound: safe.
        ChainPosition::Confirmed { anchor, .. } => {
            let height = anchor.block_id.height;
            TxStatus::Confirmed {
                height,
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
/// descending; ties break on txid so the order is deterministic.
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

    /// A made-up unconfirmed transaction paying `sat` to `to` (BDK doesn't need its parent).
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
            label: None,
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
            label: None,
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

    /// Without `restore_first_seen`, bdk_chain 0.23.3 reports `first_seen` as the latest
    /// sighting after a reopen.
    #[test]
    fn first_seen_survives_reopen_after_later_sightings() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cfg = regtest_config(dir.path())?;
        let descriptors = descriptors()?;
        let mut wallet = WalletService::create(&cfg, &descriptors, Some(0))?;
        let address = wallet.new_address()?;
        let funding = funding_tx(&address.address, 50_000)?;

        let (first, later) = (1_700_000_000, 1_700_000_900);
        wallet
            .bdk_mut()
            .apply_unconfirmed_txs([(funding.clone(), first)]);
        // Seen again in the mempool on a later sync.
        wallet.bdk_mut().apply_unconfirmed_txs([(funding, later)]);
        let unconfirmed = |w: &WalletService| w.history().first().map(|row| row.status.clone());
        let expected = Some(TxStatus::Unconfirmed {
            first_seen: Some(first),
        });
        assert_eq!(unconfirmed(&wallet), expected);
        wallet.persist()?;
        drop(wallet);

        let mut reopened = WalletService::open(&cfg, Some(&descriptors))?;
        assert_eq!(unconfirmed(&reopened), expected);
        // The repair only stages values already on disk; persisting is harmless.
        reopened.persist()?;
        drop(reopened);
        assert_eq!(unconfirmed(&WalletService::open(&cfg, None)?), expected);
        Ok(())
    }

    /// Against a real regtest node: receive, confirm, count confirmations, reopen.
    /// Skipped when no `bitcoind` is available.
    #[cfg(feature = "test-utils")]
    mod node {
        use bdk_bitcoind_rpc::bitcoincore_rpc::{Auth, Client};
        use bdk_bitcoind_rpc::{Emitter, NO_EXPECTED_MEMPOOL_TXS};

        use super::*;
        use crate::config::RpcAuth;
        use crate::testnode::TestNode;

        /// Minimal stand-in for `Node::sync`: blocks from the birthday, then mempool.
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
