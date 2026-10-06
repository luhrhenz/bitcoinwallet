//! Every desktop command as a plain function over [`AppState`], so it can be tested without a
//! webview; `ipc.rs` wraps each one in a `#[tauri::command]`. Names and JSON shapes match
//! `apps/desktop/src/lib/api.ts` and `types.ts`; every failure is an [`ApiError`].
//!
//! - The wallet is opened per command ([`with_wallet`]) and closed before returning, so `btcw`
//!   in a terminal keeps working next to the app.
//! - Secrets stay in Rust: only [`create_wallet`] and [`reveal_phrase`] return recovery words,
//!   and a prepared payment's PSBT stays in the session behind a random id.
//! - Cheap checks (address, dust) run before the wallet is opened, the node is contacted or
//!   Argon2 runs.

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

/// The recovery phrase as a JSON array, serialized straight from the zeroizing buffer (no
/// `Vec<String>` copy). `Debug` shows only the word count.
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

/// The wallet's synced height, or the last one seen if the wallet is busy (a long sync, or
/// `btcw` has it open).
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

/// New random wallet; the app stays unlocked. Returns the recovery words for the backup screen.
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
    // The core checks again under the lock; this just avoids a node round trip.
    if api::wallet_exists(&cfg) {
        return Err(WalletError::WalletExists(cfg.network_dir()).into());
    }
    api::check_new_password(password)?;

    // A new wallet has no older transactions, so its first sync starts at today's tip. Without
    // a node it scans from genesis.
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

/// Password → keep only the [`btcw_core::keys::Signer`], after checking the seed matches the
/// wallet.
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

/// User activity while unlocked: push the auto-lock deadline back.
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
    // Wallet first: "no wallet" is a better first error than "no node".
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
/// Needs the wallet unlocked; a new preview replaces any earlier one. `to` is an address or a
/// contact's name.
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
    // `build_psbt`'s checks, before the wallet lock, the node and the sync.
    let amount = Amount::from_sat(amount_sat);
    let typed_address = match tx::parse_address(to, cfg.network) {
        Ok(address) => Some(address),
        // Not address-like: maybe a contact's name. A mistyped address must never resolve to
        // a contact, so anything address-like keeps its address error.
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
        // For a name, report an unknown contact or dust before contacting the node.
        if typed_address.is_none() {
            let recipient = wallet.resolve_recipient(to)?;
            tx::check_amount(&recipient.address, amount)?;
        }
        let node = Node::connect(&cfg.rpc, cfg.network)?;
        node.sync(wallet, &mut |_| {})?;
        let (psbt, preview) = tx::prepare_send(wallet, &node, to, amount_sat, fee_rate)?;
        // "Send" reopens the wallet, so the revealed change address must be saved now.
        if let Err(e) = wallet.persist() {
            tx::cancel(wallet, &psbt);
            return Err(e.into());
        }
        Ok((psbt, preview))
    })?;
    drop(previous);

    let id = random_id()?;
    let mut session = state.session();
    // Locked while this was being built: don't keep it.
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

/// "Send": sign the prepared PSBT and broadcast it. The PSBT is removed either way, so a failed
/// send is always reviewed again before another try.
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
        // The wallet was closed while the preview was on screen; another process may have spent
        // these coins or used this change address since.
        let checked = match replaces {
            None => tx::check_prepared(wallet, &psbt)
                .map_err(ApiError::from)
                .and_then(|()| Ok(Node::connect(&cfg.rpc, cfg.network)?)),
            // Sync first: the original may have confirmed or been replaced since the preview.
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
        // `tx::complete_send` in two halves: sign while holding the session (so `lock` can't
        // wipe the key mid-signature), then release it so "Lock" never waits on the node.
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
    // Releasing the change address is best effort: BDK keeps "used" marks in memory only, so
    // a reopened wallet has nothing to release. A busy wallet doesn't fail the cancel.
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

/// The lowest fee rate (sat/vB, rounded up to the hundredth) that can replace `txid`, or why it
/// can't be sped up. Watch-only; answers from the last sync.
pub fn min_fee_bump_rate(state: &AppState, txid: &str) -> ApiResult<f64> {
    let cfg = state.begin(Activity::User)?;
    let txid = parse_txid(txid)?;
    let rate = with_wallet(state, &cfg, |wallet| {
        Ok(tx::min_fee_bump_rate(wallet, txid)?)
    })?;
    Ok(sat_vb_rounded_up(rate))
}

/// "Speed up" → review: like [`prepare_send`], but builds a replacement for `txid`.
/// `fee_rate_sat_vb: None` means the node's estimate, raised to the minimum if lower.
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

    // One prepared payment at a time: this one replaces (cancels) the last.
    let previous = state.session().take_any_pending();
    let (psbt, preview) = with_wallet(state, &cfg, |wallet| {
        if let Some(previous) = &previous
            && previous.network == cfg.network
        {
            tx::cancel(wallet, &previous.psbt);
        }
        // Refuse unknown, confirmed or incoming transactions before contacting the node.
        tx::min_fee_bump_rate(wallet, original)?;
        let node = Node::connect(&cfg.rpc, cfg.network)?;
        node.sync(wallet, &mut |_| {})?;
        let (psbt, preview) = tx::prepare_fee_bump(wallet, &node, original, fee_rate)?;
        // As in `prepare_send`: a fresh change address must be saved for the signing wallet.
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

/// Save a contact. The address must be valid for the active network.
pub fn add_contact(
    state: &AppState,
    name: &str,
    address: &str,
    note: Option<&str>,
) -> ApiResult<Contact> {
    let cfg = state.begin(Activity::User)?;
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

/// Label one of the wallet's transactions, replacing any earlier label. Returns it trimmed.
pub fn set_label(state: &AppState, txid: &str, label: &str) -> ApiResult<String> {
    let cfg = state.begin(Activity::User)?;
    let txid = parse_txid(txid)?;
    with_wallet(state, &cfg, |wallet| Ok(wallet.set_label(txid, label)?))
}

/// Remove a transaction's label. Fine if it had none.
pub fn clear_label(state: &AppState, txid: &str) -> ApiResult<()> {
    let cfg = state.begin(Activity::User)?;
    let txid = parse_txid(txid)?;
    with_wallet(state, &cfg, |wallet| {
        wallet.clear_label(txid)?;
        Ok(())
    })
}

/// Status of one of the wallet's transactions, synced first if the node answers. `null` if
/// unknown.
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

/// Validate, save `desktop.json`, apply. Switching networks locks the app: the signer belongs
/// to the old network's wallet.
pub fn set_settings(state: &AppState, next: &Settings) -> ApiResult<()> {
    state.touch(Activity::User);
    let (mut stored, network) = settings::validate(next)?;
    // The whole configuration must still resolve, or every later command would fail.
    let cfg = state.config_with(&stored)?;
    if cfg.network != network {
        return Err(ApiError::internal(
            "the saved network did not take effect; check BTCW_NETWORK and btcw.toml",
        ));
    }
    let mut current = state.settings_guard();
    // The assistant has its own command; keep what it saved.
    stored.assistant = current.assistant.clone();
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
/// Needs the password because only the encrypted keystore records the phrase length; it also
/// reports a wrong password before the user types any words.
pub fn backup_challenge(state: &AppState, password: &SecretString) -> ApiResult<Vec<usize>> {
    let cfg = state.begin(Activity::User)?;
    let word_count = api::reveal_phrase(&cfg, password)?.word_count();
    Ok(api::backup_challenge(word_count)?)
}

/// Check the user's words against the encrypted phrase and mark the wallet verified.
/// `backup_mismatch` names the wrong positions, never words. The words are wiped afterwards.
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

/// The recovery phrase again, given the password.
pub fn reveal_phrase(state: &AppState, password: &SecretString) -> ApiResult<MnemonicReply> {
    let cfg = state.begin(Activity::User)?;
    let mnemonic = api::reveal_phrase(&cfg, password)?;
    Ok(MnemonicReply {
        mnemonic: PhraseWords::of(&mnemonic),
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────────────────────────

/// Open the wallet (watch-only) for one command, after this process's other commands on the
/// same network, and close it before returning.
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

/// A txid from the UI; `tx_not_found` if it isn't one.
fn parse_txid(txid: &str) -> ApiResult<Txid> {
    Txid::from_str(txid.trim()).map_err(|_| {
        let shown: String = txid.chars().take(MAX_ECHOED_INPUT).collect();
        WalletError::TxNotFound(format!("`{shown}` is not a transaction id")).into()
    })
}

/// sat/vB for display, rounded *up* to the hundredth (751 sat/kWU → 3.01), so the minimum shown
/// and typed back is always accepted.
fn sat_vb_rounded_up(rate: FeeRate) -> f64 {
    // 1 sat/vB = 250 sat/kWU, so hundredths of a sat/vB = sat/kWU × 2 / 5, rounded up.
    let hundredths = rate.to_sat_per_kwu().saturating_mul(2).div_ceil(5);
    hundredths as f64 / 100.0
}

/// sat/vB as typed (may have decimals) → sat/kWU, rounded up so the rate paid is never below
/// the one asked for.
fn fee_rate_from_sat_vb(sat_vb: f64) -> ApiResult<FeeRate> {
    // Far above the 25 000 sat/vB limit, but small enough for the cast below to be exact.
    const CAST_CEILING: f64 = 1.0e9;
    if !sat_vb.is_finite() || sat_vb <= 0.0 || sat_vb > CAST_CEILING {
        return Err(WalletError::TxBuild(
            "the fee rate must be a positive number of sat/vB".into(),
        )
        .into());
    }
    // The epsilon absorbs float noise (2.3 × 250 = 575.0000000000001).
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
