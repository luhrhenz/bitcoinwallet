//! Every desktop command as a plain function over [`AppState`]. Nothing here knows about Tauri,
//! so all of it is unit-tested without a webview; `ipc.rs` wraps each function in a thin
//! `#[tauri::command]` that runs it on a blocking thread.
//!
//! The contract is `apps/desktop/src/lib/api.ts`: command names, argument names and the JSON
//! shapes of `types.ts`. Every failure is an [`ApiError`] `{ code, message }`.
//!
//! Rules every function follows:
//! - **The wallet is opened per command** ([`with_wallet`]) and dropped before returning, so its
//!   lock file is held only briefly and `btcw` in a terminal keeps working next to the app.
//! - **Secrets stay in Rust.** The only values that carry recovery words into JS are the
//!   replies of [`create_wallet`] and [`reveal_phrase`]. Passwords arrive as `SecretString`,
//!   are used once and dropped. The PSBT of a prepared payment stays in the session
//!   (`state.rs`) behind a random id; the UI gets the preview only.
//! - **Cheap checks first**: an address typo or a dust amount is reported before the wallet is
//!   opened, the node is contacted or Argon2 runs, as the CLI does. A contact name (which only
//!   the wallet file can resolve) is looked up right after opening the wallet, still before the
//!   node.
//! - **Wallet edits are watch-only**: contacts and labels need no password, and count as user
//!   activity for the auto-lock. Sending and speeding up a payment need the signer.

use std::str::FromStr as _;
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Duration;

use btcw_core::WalletError;
use btcw_core::api::{self, CreatedWallet, Unlocked};
use btcw_core::bitcoin::{Amount, FeeRate, Psbt, Txid};
use btcw_core::book;
use btcw_core::chain::Node;
use btcw_core::config::{self, Config};
use btcw_core::keys::{Mnemonic, WordCount};
use btcw_core::tx;
use btcw_core::types::{
    AddressRow, BalanceView, Contact, SendPreview, SyncProgress, SyncReport, TxRow, TxStatus,
    UtxoRow,
};
use btcw_core::wallet::WalletService;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Serialize;
use serde::ser::SerializeSeq as _;
use zeroize::{Zeroize as _, Zeroizing};

use crate::error::{ApiError, ApiResult};
use crate::settings::{self, Settings};
use crate::state::{Activity, AppState, PENDING_SEND_TTL, PendingSend};

/// At most this many `sync-progress` events per second reach the webview (20/s).
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// Longest piece of a malformed txid echoed back in an error.
const MAX_ECHOED_INPUT: usize = 80;

// ── Reply shapes (`types.ts`) ───────────────────────────────────────────────────────────────

/// `AppInfo` in `types.ts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppInfo {
    pub network: String,
    /// Networks the user may pick (mainnet only in `--features mainnet` builds).
    pub networks: Vec<String>,
    pub wallet_exists: bool,
    /// A signer is loaded for this network.
    pub unlocked: bool,
    /// `WalletService::synced_height`; `null` when there is no wallet.
    pub synced_height: Option<u32>,
    /// `null` when there is no wallet.
    pub backup_verified: Option<bool>,
}

/// The recovery phrase as a JSON array of words, written straight from the wiped-on-drop buffer
/// (no `Vec<String>` copy). `Debug` shows only the word count.
pub struct PhraseWords(Zeroizing<String>);

impl PhraseWords {
    fn of(mnemonic: &Mnemonic) -> Self {
        Self(mnemonic.phrase())
    }

    pub fn len(&self) -> usize {
        self.0.split_whitespace().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Serialize for PhraseWords {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.len()))?;
        for word in self.0.split_whitespace() {
            seq.serialize_element(word)?;
        }
        seq.end()
    }
}

impl std::fmt::Debug for PhraseWords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PhraseWords(<{} words redacted>)", self.len())
    }
}

/// `{ mnemonic: string[] }`: the reply of `create_wallet` and `reveal_phrase`.
#[derive(Debug, Serialize)]
pub struct MnemonicReply {
    pub mnemonic: PhraseWords,
}

/// `PreparedSend` in `types.ts`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreparedSend {
    pub id: String,
    pub preview: SendPreview,
}

/// `{ txid: string }`
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SentTx {
    pub txid: String,
}

// ── App and session ─────────────────────────────────────────────────────────────────────────

pub fn app_info(state: &AppState) -> ApiResult<AppInfo> {
    let cfg = state.begin(Activity::Background)?;
    let wallet_exists = api::wallet_exists(&cfg);
    let (synced_height, backup_verified) = if cfg.wallet_db_path().exists() {
        (
            Some(synced_height(state, &cfg)?),
            // Lock-free, read-only: works even while another process has the wallet open.
            WalletService::read_backup_verified(&cfg)?,
        )
    } else {
        (None, None)
    };
    Ok(AppInfo {
        network: cfg.network.to_string(),
        networks: config::selectable_networks()
            .iter()
            .map(ToString::to_string)
            .collect(),
        wallet_exists,
        unlocked: state.session().is_unlocked_for(cfg.network),
        synced_height,
        backup_verified,
    })
}

/// The wallet's synced height. If this app is busy with the wallet (a long sync), or `btcw`
/// has it open, answer from the last height seen instead of waiting.
fn synced_height(state: &AppState, cfg: &Config) -> ApiResult<u32> {
    let gate = state.gate(cfg.network);
    let turn = match gate.try_lock() {
        Ok(turn) => Some(turn),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    };
    let cached = state.cached_synced(cfg.network);
    match (turn, cached) {
        (None, Some(height)) => Ok(height),
        // Busy and nothing cached yet: wait for the turn like any other command.
        (None, None) => with_wallet(state, cfg, |wallet| Ok(wallet.synced_height())),
        (Some(_turn), cached) => match api::open_watch_only(cfg) {
            Ok(wallet) => {
                let height = wallet.synced_height();
                state.remember_synced(cfg.network, height);
                Ok(height)
            }
            Err(WalletError::WalletInUse) => cached.ok_or_else(|| WalletError::WalletInUse.into()),
            Err(e) => Err(e.into()),
        },
    }
}

/// New random wallet; the app stays unlocked. The reply is one of the two places where
/// recovery words leave Rust (the backup screen needs them).
pub fn create_wallet(
    state: &AppState,
    words: u32,
    password: &SecretString,
) -> ApiResult<MnemonicReply> {
    let cfg = state.begin(Activity::User)?;
    let words = match words {
        12 => WordCount::Words12,
        24 => WordCount::Words24,
        other => {
            return Err(WalletError::Config(format!(
                "a new recovery phrase has 12 or 24 words, not {other}"
            ))
            .into());
        }
    };
    // Checked again (under the wallet lock) by the core; these just avoid a node round trip.
    if api::wallet_exists(&cfg) {
        return Err(WalletError::WalletExists(cfg.network_dir()).into());
    }
    api::check_new_password(password)?;

    // Like `btcw create`: a new wallet can't have older transactions, so its first sync starts
    // at today's tip. Without a node it scans from genesis (slow on testnet4, but complete).
    let birthday = current_tip(&cfg);
    let CreatedWallet {
        mnemonic,
        unlocked: Unlocked { wallet, signer },
    } = with_gate(state, &cfg, || {
        api::create_wallet(&cfg, words, password, birthday)
    })?;
    drop(wallet);
    state.session().unlock(cfg.network, signer, state.now());
    Ok(MnemonicReply {
        mnemonic: PhraseWords::of(&mnemonic),
    })
}

/// Wallet from a typed phrase; the app stays unlocked. The UI syncs next (with progress).
pub fn restore_wallet(
    state: &AppState,
    phrase: &SecretString,
    password: &SecretString,
    birthday: Option<u32>,
) -> ApiResult<()> {
    let cfg = state.begin(Activity::User)?;
    if api::wallet_exists(&cfg) {
        return Err(WalletError::WalletExists(cfg.network_dir()).into());
    }
    let Unlocked { wallet, signer } = with_gate(state, &cfg, || {
        api::restore_wallet(&cfg, phrase.expose_secret(), password, birthday)
    })?;
    drop(wallet);
    state.session().unlock(cfg.network, signer, state.now());
    Ok(())
}

/// Password → keep only the [`btcw_core::keys::Signer`]. The wallet is opened just to check
/// that the decrypted seed belongs to it, then closed again.
pub fn unlock(state: &AppState, password: &SecretString) -> ApiResult<()> {
    let cfg = state.begin(Activity::User)?;
    let signer = with_wallet(state, &cfg, |wallet| {
        Ok(api::load_signer(&cfg, wallet, password)?)
    })?;
    state.session().unlock(cfg.network, signer, state.now());
    Ok(())
}

/// Drop the signer (wiping the master key) and any prepared payment. Never fails.
pub fn lock(state: &AppState) -> ApiResult<()> {
    state.session().lock();
    Ok(())
}

/// The UI saw the user (a key press, a click) while unlocked: push the auto-lock deadline back.
pub fn keep_alive(state: &AppState) -> ApiResult<()> {
    state.touch(Activity::User);
    Ok(())
}

// ── Watch-only views ────────────────────────────────────────────────────────────────────────

pub fn new_address(state: &AppState) -> ApiResult<AddressRow> {
    let cfg = state.begin(Activity::User)?;
    with_wallet(state, &cfg, |wallet| Ok(wallet.new_address()?))
}

pub fn list_addresses(state: &AppState) -> ApiResult<Vec<AddressRow>> {
    let cfg = state.begin(Activity::Background)?;
    with_wallet(state, &cfg, |wallet| Ok(wallet.addresses()))
}

pub fn balance(state: &AppState) -> ApiResult<BalanceView> {
    let cfg = state.begin(Activity::Background)?;
    with_wallet(state, &cfg, |wallet| Ok(wallet.balance()))
}

pub fn history(state: &AppState) -> ApiResult<Vec<TxRow>> {
    let cfg = state.begin(Activity::Background)?;
    with_wallet(state, &cfg, |wallet| Ok(wallet.history()))
}

pub fn utxos(state: &AppState) -> ApiResult<Vec<UtxoRow>> {
    let cfg = state.begin(Activity::Background)?;
    with_wallet(state, &cfg, |wallet| Ok(wallet.utxos()))
}

/// Pull new blocks and the mempool from the node. `on_progress` gets at most one event per
/// [`PROGRESS_INTERVAL`], plus the last one.
pub fn sync(state: &AppState, on_progress: &mut dyn FnMut(SyncProgress)) -> ApiResult<SyncReport> {
    let cfg = state.begin(Activity::Background)?;
    // Wallet first: "no wallet here" is a better first error than "no node" (as in `btcw sync`).
    with_wallet(state, &cfg, |wallet| {
        let node = Node::connect(&cfg.rpc, cfg.network)?;
        let mut throttle = ProgressThrottle::new(PROGRESS_INTERVAL);
        let report = node.sync(wallet, &mut |progress| {
            // `app_info` answers from this while the sync holds the wallet.
            state.remember_synced(cfg.network, progress.height);
            if let Some(progress) = throttle.offer(state.now(), progress) {
                on_progress(progress);
            }
        })?;
        if let Some(progress) = throttle.finish() {
            on_progress(progress);
        }
        Ok(report)
    })
}

// ── Sending ─────────────────────────────────────────────────────────────────────────────────

/// "Review": sync, build the unsigned PSBT, keep it here, return its preview and an id.
/// Needs the wallet unlocked (`locked` otherwise). A new preview replaces any earlier one.
///
/// `to` is an address or a contact's name (`WalletService::resolve_recipient`, as in `btcw send`).
/// The preview's `to` is always the full address; `contact` is the name, if one was typed.
pub fn prepare_send(
    state: &AppState,
    to: &str,
    amount_sat: u64,
    fee_rate_sat_vb: Option<f64>,
) -> ApiResult<PreparedSend> {
    let cfg = state.begin(Activity::User)?;
    if !state.session().is_unlocked_for(cfg.network) {
        return Err(ApiError::locked());
    }
    // The checks `build_psbt` makes, before the wallet lock, the node and the sync.
    let amount = Amount::from_sat(amount_sat);
    let typed_address = match tx::parse_address(to, cfg.network) {
        Ok(address) => Some(address),
        // Not an address and doesn't look like one: maybe a contact's name, which needs the
        // wallet file. Anything address-like keeps its address error (a mistyped address must
        // never resolve to a contact).
        Err(WalletError::InvalidAddress(_))
            if !to.trim().is_empty() && !book::looks_like_address(to) =>
        {
            None
        }
        Err(e) => return Err(e.into()),
    };
    if let Some(address) = &typed_address {
        tx::check_amount(address, amount)?;
    }
    let fee_rate = fee_rate_sat_vb.map(fee_rate_from_sat_vb).transpose()?;
    if let Some(rate) = fee_rate {
        tx::check_fee_rate(rate)?;
    }

    // At most one prepared payment: this one replaces (cancels) the last.
    let previous = state.session().take_any_pending();
    let (psbt, preview) = with_wallet(state, &cfg, |wallet| {
        if let Some(previous) = &previous
            && previous.network == cfg.network
        {
            tx::cancel(wallet, &previous.psbt);
        }
        // A name: an unknown one (`contact`) or a dust amount for its address is reported
        // before the node is contacted, like a typo in an address.
        if typed_address.is_none() {
            let recipient = wallet.resolve_recipient(to)?;
            tx::check_amount(&recipient.address, amount)?;
        }
        let node = Node::connect(&cfg.rpc, cfg.network)?;
        // Coin selection must see the coins as they are now.
        node.sync(wallet, &mut |_| {})?;
        let (psbt, preview) = tx::prepare_send(wallet, &node, to, amount_sat, fee_rate)?;
        // The wallet is reopened for "Send", so the change address revealed here must be saved
        // now, or the reopened wallet would not know it.
        if let Err(e) = wallet.persist() {
            tx::cancel(wallet, &psbt);
            return Err(e.into());
        }
        Ok((psbt, preview))
    })?;
    drop(previous);

    let id = random_id()?;
    let mut session = state.session();
    // Locked (by the user or the auto-lock) while this was being built: don't keep it.
    if !session.is_unlocked_for(cfg.network) {
        return Err(ApiError::locked());
    }
    session.set_pending(PendingSend {
        id: id.clone(),
        network: cfg.network,
        psbt,
        created: state.now(),
        replaces: None,
    });
    Ok(PreparedSend { id, preview })
}

/// "Send": sign the prepared PSBT and broadcast it. `locked` without a signer; `tx_build` for
/// an id that is unknown, already used, cancelled or expired. The PSBT is removed either way,
/// so a failed send is always reviewed again before another try.
pub fn confirm_send(state: &AppState, id: &str) -> ApiResult<SentTx> {
    let cfg = state.begin(Activity::User)?;
    let pending = {
        let mut session = state.session();
        if !session.is_unlocked_for(cfg.network) {
            return Err(ApiError::locked());
        }
        session.take_pending(id)
    };
    let Some(pending) = pending else {
        return Err(WalletError::TxBuild(
            "this payment is no longer waiting to be sent (it was sent, cancelled or replaced); \
             review it again"
                .into(),
        )
        .into());
    };
    if pending.network != cfg.network
        || state.now().saturating_duration_since(pending.created) > PENDING_SEND_TTL
    {
        return Err(WalletError::TxBuild(format!(
            "this payment preview expired after {} minutes; review it again",
            PENDING_SEND_TTL.as_secs() / 60
        ))
        .into());
    }

    let mut psbt = pending.psbt;
    let replaces = pending.replaces;
    let txid = with_wallet(state, &cfg, |wallet| {
        // The wallet was closed while the preview was on screen: `btcw send` may have spent
        // these coins or used this change address since. Don't sign a stale payment.
        let checked = match replaces {
            None => tx::check_prepared(wallet, &psbt)
                .map_err(ApiError::from)
                .and_then(|()| Ok(Node::connect(&cfg.rpc, cfg.network)?)),
            // A fee bump spends coins the original already spends, by design, so the plain
            // check would refuse it. Sync first: the original may have confirmed (or been
            // replaced) since the preview, and then the replacement must not be signed.
            Some(original) => Node::connect(&cfg.rpc, cfg.network)
                .and_then(|node| {
                    node.sync(wallet, &mut |_| {})?;
                    tx::check_prepared_bump(wallet, &psbt, original)?;
                    Ok(node)
                })
                .map_err(ApiError::from),
        };
        let node = match checked {
            Ok(node) => node,
            Err(e) => {
                release(wallet, &psbt);
                return Err(e);
            }
        };
        // This is `tx::complete_send` in its two halves: sign while holding the session (so a
        // `lock` can't wipe the key halfway through a signature), then let go of it before
        // anything waits on the network, so "Lock" never waits for a slow node.
        let signed = match state.session().signer_for(cfg.network) {
            Some(signer) => tx::sign_psbt(wallet, signer, &mut psbt).map_err(ApiError::from),
            None => Err(ApiError::locked()),
        };
        if let Err(e) = signed {
            release(wallet, &psbt);
            return Err(e);
        }
        // Releases the change address itself if extracting or broadcasting fails.
        Ok(tx::broadcast_signed(wallet, &node, psbt)?)
    })?;
    Ok(SentTx {
        txid: txid.to_string(),
    })
}

/// "Cancel": forget the prepared payment. Unknown ids are fine (already sent or dropped).
pub fn cancel_send(state: &AppState, id: &str) -> ApiResult<()> {
    state.touch(Activity::User);
    let Some(pending) = state.session().take_pending(id) else {
        return Ok(());
    };
    // The payment is cancelled now: its PSBT is gone. Giving the change address back is
    // best effort. BDK keeps "used" marks in memory only, so the wallet reopened here has
    // nothing left to release (the next payment reuses that address anyway); `tx::cancel` is
    // still run so this stays right if that ever changes. A busy wallet is no reason to tell
    // the user the cancel failed.
    let released = state.config().map_err(ApiError::from).and_then(|cfg| {
        if cfg.network != pending.network {
            return Ok(());
        }
        with_wallet(state, &cfg, |wallet| {
            release(wallet, &pending.psbt);
            Ok(())
        })
    });
    if let Err(e) = released {
        tracing::warn!(code = e.code, error = %e.message, "cancel_send: the change address was not released");
    }
    Ok(())
}

// ── Speed up (fee bump, RBF) ────────────────────────────────────────────────────────────────

/// The lowest fee rate, in sat/vB, that can replace the unconfirmed payment `txid`, rounded up
/// to the next hundredth so that the number shown (and typed back) is always enough. Fails, with
/// the reason, when `txid` can't be sped up at all (`tx_not_found`, `tx_build`). Watch-only; no
/// node: it answers from the last sync (`tx_status` keeps that fresh on the transaction screen).
pub fn min_fee_bump_rate(state: &AppState, txid: &str) -> ApiResult<f64> {
    let cfg = state.begin(Activity::User)?;
    let txid = parse_txid(txid)?;
    let rate = with_wallet(state, &cfg, |wallet| {
        Ok(tx::min_fee_bump_rate(wallet, txid)?)
    })?;
    Ok(sat_vb_rounded_up(rate))
}

/// "Speed up" → review: sync, build the unsigned replacement for `txid`, keep it here like a
/// prepared payment (same single slot, same 10-minute expiry, same `confirm_send` /
/// `cancel_send`), and return its preview, whose `replaces` is `txid`. Needs the wallet unlocked.
/// `fee_rate_sat_vb: None` means the node's estimate, raised to the minimum when it is lower.
pub fn prepare_fee_bump(
    state: &AppState,
    txid: &str,
    fee_rate_sat_vb: Option<f64>,
) -> ApiResult<PreparedSend> {
    let cfg = state.begin(Activity::User)?;
    if !state.session().is_unlocked_for(cfg.network) {
        return Err(ApiError::locked());
    }
    let original = parse_txid(txid)?;
    let fee_rate = fee_rate_sat_vb.map(fee_rate_from_sat_vb).transpose()?;
    if let Some(rate) = fee_rate {
        tx::check_fee_rate(rate)?;
    }

    // One prepared payment at a time, bump or not: this one replaces (cancels) the last.
    let previous = state.session().take_any_pending();
    let (psbt, preview) = with_wallet(state, &cfg, |wallet| {
        if let Some(previous) = &previous
            && previous.network == cfg.network
        {
            tx::cancel(wallet, &previous.psbt);
        }
        // An unknown, confirmed or incoming transaction is refused before the node is asked.
        tx::min_fee_bump_rate(wallet, original)?;
        let node = Node::connect(&cfg.rpc, cfg.network)?;
        // A payment that confirmed meanwhile can't be replaced; coin selection (if the change
        // can't cover the extra fee) must see the coins as they are now.
        node.sync(wallet, &mut |_| {})?;
        let (psbt, preview) = tx::prepare_fee_bump(wallet, &node, original, fee_rate)?;
        // Saved for the reopened wallet that signs, as in `prepare_send` (a replacement of a
        // payment without change gets a fresh change address).
        if let Err(e) = wallet.persist() {
            tx::cancel(wallet, &psbt);
            return Err(e.into());
        }
        Ok((psbt, preview))
    })?;
    drop(previous);

    let id = random_id()?;
    let mut session = state.session();
    if !session.is_unlocked_for(cfg.network) {
        return Err(ApiError::locked());
    }
    session.set_pending(PendingSend {
        id: id.clone(),
        network: cfg.network,
        psbt,
        created: state.now(),
        replaces: Some(original),
    });
    Ok(PreparedSend { id, preview })
}

// ── Address book and labels (watch-only) ────────────────────────────────────────────────────

/// Every contact, sorted by name (ignoring case).
pub fn list_contacts(state: &AppState) -> ApiResult<Vec<Contact>> {
    let cfg = state.begin(Activity::Background)?;
    with_wallet(state, &cfg, |wallet| Ok(wallet.contacts()?))
}

/// Save a contact. The address must be valid for the active network (`invalid_address`,
/// `network_mismatch`); the name and note follow the core's rules (`contact`).
pub fn add_contact(
    state: &AppState,
    name: &str,
    address: &str,
    note: Option<&str>,
) -> ApiResult<Contact> {
    let cfg = state.begin(Activity::User)?;
    // An address typo is reported without opening the wallet.
    tx::parse_address(address, cfg.network)?;
    with_wallet(state, &cfg, |wallet| {
        Ok(wallet.add_contact(name, address, note)?)
    })
}

/// Delete a contact (name matched ignoring case); returns what was removed.
pub fn remove_contact(state: &AppState, name: &str) -> ApiResult<Contact> {
    let cfg = state.begin(Activity::User)?;
    with_wallet(state, &cfg, |wallet| Ok(wallet.remove_contact(name)?))
}

/// Rename a contact (`old` matched ignoring case); returns it under its new name.
pub fn rename_contact(state: &AppState, old: &str, new: &str) -> ApiResult<Contact> {
    let cfg = state.begin(Activity::User)?;
    with_wallet(state, &cfg, |wallet| Ok(wallet.rename_contact(old, new)?))
}

/// Label one of the wallet's transactions (`tx_not_found` for any other txid), replacing any
/// earlier label. Returns the label as stored (trimmed).
pub fn set_label(state: &AppState, txid: &str, label: &str) -> ApiResult<String> {
    let cfg = state.begin(Activity::User)?;
    let txid = parse_txid(txid)?;
    with_wallet(state, &cfg, |wallet| Ok(wallet.set_label(txid, label)?))
}

/// Remove a transaction's label. Fine if it had none; `tx_not_found` for a txid the wallet
/// doesn't know.
pub fn clear_label(state: &AppState, txid: &str) -> ApiResult<()> {
    let cfg = state.begin(Activity::User)?;
    let txid = parse_txid(txid)?;
    with_wallet(state, &cfg, |wallet| {
        wallet.clear_label(txid)?;
        Ok(())
    })
}

/// Status of one of the wallet's transactions: syncs first when the node answers, otherwise
/// answers from the last sync. `null` if the wallet doesn't know the transaction.
pub fn tx_status(state: &AppState, txid: &str) -> ApiResult<Option<TxStatus>> {
    let cfg = state.begin(Activity::Background)?;
    let txid = parse_txid(txid)?;
    with_wallet(state, &cfg, |wallet| {
        let synced =
            Node::connect(&cfg.rpc, cfg.network).and_then(|node| node.sync(wallet, &mut |_| {}));
        if let Err(e) = synced {
            tracing::info!(code = e.code(), error = %e, "tx_status: could not sync; answering from the last sync");
        }
        Ok(tx::tx_status(wallet, txid))
    })
}

// ── Settings ────────────────────────────────────────────────────────────────────────────────

pub fn get_settings(state: &AppState) -> ApiResult<Settings> {
    let cfg = state.begin(Activity::Background)?;
    Ok(state.stored_settings().to_settings(cfg.network))
}

/// Validate, save `desktop.json`, apply. Switching networks locks the app (and drops any
/// prepared payment): the signer belongs to the old network's wallet.
pub fn set_settings(state: &AppState, next: &Settings) -> ApiResult<()> {
    state.touch(Activity::User);
    let (stored, network) = settings::validate(next)?;
    // The whole configuration must still resolve (btcw.toml, BTCW_*), or every command after
    // this one would fail.
    let cfg = state.config_with(&stored)?;
    if cfg.network != network {
        return Err(ApiError::internal(
            "the saved network did not take effect; check BTCW_NETWORK and btcw.toml",
        ));
    }
    let mut current = state.settings_guard();
    let previous = state.config_with(&current).map(|c| c.network).ok();
    settings::save(&state.settings_path(), &stored)?;
    *current = stored;
    drop(current);
    if previous != Some(network) {
        state.session().lock();
    }
    Ok(())
}

// ── Backup ──────────────────────────────────────────────────────────────────────────────────

/// Three random word positions (1-based, ascending) to ask for.
///
/// Takes the password: the only place the phrase's length is recorded is the encrypted keystore
/// (12- and 24-word phrases overlap in byte length, and restored phrases can have 15, 18 or 21
/// words), so it is decrypted to count the words and dropped at once. Asking for the password
/// first also tells the user it is wrong *before* they type any words, as `btcw backup verify`
/// does.
pub fn backup_challenge(state: &AppState, password: &SecretString) -> ApiResult<Vec<usize>> {
    let cfg = state.begin(Activity::User)?;
    let word_count = api::reveal_phrase(&cfg, password)?.word_count();
    Ok(api::backup_challenge(word_count)?)
}

/// Check the user's words against the encrypted phrase; on success the wallet is marked
/// verified. `backup_mismatch` names the wrong positions, never words. The words are wiped
/// afterwards whatever the outcome.
pub fn verify_backup(
    state: &AppState,
    password: &SecretString,
    mut answers: Vec<(usize, String)>,
) -> ApiResult<()> {
    let result = state.begin(Activity::User).and_then(|cfg| {
        with_wallet(state, &cfg, |wallet| {
            Ok(api::verify_backup(&cfg, wallet, password, &answers)?)
        })
    });
    // The typed words are as secret as the phrase.
    for (_, word) in &mut answers {
        word.zeroize();
    }
    result
}

/// The recovery phrase again, with the password, at any time (the owner's choice, PLAN §4.2).
pub fn reveal_phrase(state: &AppState, password: &SecretString) -> ApiResult<MnemonicReply> {
    let cfg = state.begin(Activity::User)?;
    let mnemonic = api::reveal_phrase(&cfg, password)?;
    Ok(MnemonicReply {
        mnemonic: PhraseWords::of(&mnemonic),
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────────────────────────

/// Open the wallet (watch-only) for one command, waiting for this process's other commands on
/// the same network first, and close it before returning.
fn with_wallet<T>(
    state: &AppState,
    cfg: &Config,
    f: impl FnOnce(&mut WalletService) -> ApiResult<T>,
) -> ApiResult<T> {
    let gate = state.gate(cfg.network);
    let _turn = crate::state::lock(&gate);
    let mut wallet = api::open_watch_only(cfg)?;
    state.remember_synced(cfg.network, wallet.synced_height());
    let result = f(&mut wallet);
    state.remember_synced(cfg.network, wallet.synced_height());
    // Closes the database and releases the wallet's lock file before the gate opens.
    drop(wallet);
    result
}

/// Run `f` (which creates the wallet) on this network's gate.
fn with_gate<T>(
    state: &AppState,
    cfg: &Config,
    f: impl FnOnce() -> btcw_core::Result<T>,
) -> ApiResult<T> {
    let gate: Arc<Mutex<()>> = state.gate(cfg.network);
    let _turn = crate::state::lock(&gate);
    Ok(f()?)
}

/// `tx::cancel` + persist, for every way out of `confirm_send` before the broadcast.
fn release(wallet: &mut WalletService, psbt: &Psbt) {
    tx::cancel(wallet, psbt);
    if let Err(e) = wallet.persist() {
        tracing::warn!(error = %e, "could not save the wallet after releasing a payment");
    }
}

/// The node's tip height, or `None` (logged) when it can't be reached.
fn current_tip(cfg: &Config) -> Option<u32> {
    match Node::connect(&cfg.rpc, cfg.network).and_then(|node| node.tip_height()) {
        Ok(height) => Some(height),
        Err(e) => {
            tracing::warn!(
                code = e.code(),
                error = %e,
                "could not read the current block height; the new wallet's first sync will \
                 scan from the genesis block"
            );
            None
        }
    }
}

/// A txid as typed or passed by the UI; `tx_not_found` if it isn't one (as `btcw status` says).
fn parse_txid(txid: &str) -> ApiResult<Txid> {
    Txid::from_str(txid.trim()).map_err(|_| {
        let shown: String = txid.chars().take(MAX_ECHOED_INPUT).collect();
        WalletError::TxNotFound(format!("`{shown}` is not a transaction id")).into()
    })
}

/// A fee rate as sat/vB for display, rounded *up* to the hundredth (751 sat/kWU = 3.004 sat/vB
/// → 3.01), like the core's own messages. [`fee_rate_from_sat_vb`] turns it back into at least
/// the same rate, so the minimum shown is always accepted.
fn sat_vb_rounded_up(rate: FeeRate) -> f64 {
    // 1 sat/vB = 250 sat/kWU, so hundredths of a sat/vB = sat/kWU × 2 / 5, rounded up.
    let hundredths = rate.to_sat_per_kwu().saturating_mul(2).div_ceil(5);
    hundredths as f64 / 100.0
}

/// sat/vB as typed (it may have decimals, e.g. 2.5) → BDK's sat per 1000 weight units, rounded
/// up so the rate paid is never below the one asked for. `check_fee_rate` then applies the
/// 1 sat/vB minimum and the 25 000 sat/vB limit.
fn fee_rate_from_sat_vb(sat_vb: f64) -> ApiResult<FeeRate> {
    // Far above the 25 000 sat/vB limit, but small enough for the cast below to be exact.
    const CAST_CEILING: f64 = 1.0e9;
    if !sat_vb.is_finite() || sat_vb <= 0.0 || sat_vb > CAST_CEILING {
        return Err(WalletError::TxBuild(
            "the fee rate must be a positive number of sat/vB".into(),
        )
        .into());
    }
    // 1 vB = 4 WU, so 1 sat/vB = 250 sat/kWU. The epsilon absorbs float noise
    // (2.3 × 250 = 575.0000000000001) so an exact rate isn't rounded up a whole step.
    let sat_per_kwu = (sat_vb * 250.0 - 1e-6).ceil();
    Ok(FeeRate::from_sat_per_kwu(sat_per_kwu as u64))
}

/// 16 bytes from the OS RNG, hex: the id of a prepared payment.
fn random_id() -> ApiResult<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| ApiError::internal(format!("the OS random number generator failed: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Passes on at most one progress event per `every`, always the one that reaches the tip, and
/// keeps the last suppressed one for [`ProgressThrottle::finish`].
pub(crate) struct ProgressThrottle {
    every: Duration,
    last_sent: Option<std::time::Instant>,
    held: Option<SyncProgress>,
}

impl ProgressThrottle {
    pub fn new(every: Duration) -> Self {
        Self {
            every,
            last_sent: None,
            held: None,
        }
    }

    pub fn offer(
        &mut self,
        now: std::time::Instant,
        progress: SyncProgress,
    ) -> Option<SyncProgress> {
        let due = self
            .last_sent
            .is_none_or(|last| now.saturating_duration_since(last) >= self.every);
        if due || progress.height >= progress.tip_height {
            self.last_sent = Some(now);
            self.held = None;
            Some(progress)
        } else {
            self.held = Some(progress);
            None
        }
    }

    pub fn finish(&mut self) -> Option<SyncProgress> {
        self.held.take()
    }
}

#[cfg(test)]
#[path = "commands_tests.rs"]
mod tests;
