//! What the app remembers between commands; the wallet itself is opened per command.
//!
//! - [`Session`]: the [`Signer`] while unlocked, the last user activity, and at most one
//!   prepared payment.
//! - The settings from `desktop.json`.
//! - Each network's last synced height, so `app_info` can answer during a long sync.
//! - One gate per network that serializes wallet access inside this process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use btcw_core::bitcoin::{Network, Psbt, Txid};
use btcw_core::config::{self, Config};
use btcw_core::keys::Signer;

use crate::error::ApiResult;
use crate::settings::{self, SETTINGS_FILE, StoredSettings};

/// How long a prepared payment can wait for "Send" before it has to be reviewed again.
pub const PENDING_SEND_TTL: Duration = Duration::from_secs(10 * 60);

/// The Rust-side auto-lock backs up the UI's timer. The UI reports activity at most every 30 s,
/// so this margin lets the UI's timer fire first and tell the user why.
pub const AUTO_LOCK_GRACE: Duration = Duration::from_secs(60);

/// Source of "now", injectable so tests can move time forward.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Reads an environment variable (`BTCW_*`); injectable so tests don't see the developer's shell.
pub type EnvLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

/// Whether a command counts as user activity. Reads are also issued by timers, so they must not
/// keep the signing key alive; they still lock if the deadline has passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    User,
    Background,
}

pub struct AppState {
    datadir: PathBuf,
    env: Box<EnvLookup>,
    clock: Arc<dyn Clock>,
    settings: Mutex<StoredSettings>,
    session: Mutex<Session>,
    gates: Mutex<HashMap<Network, Arc<Mutex<()>>>>,
    synced: Mutex<HashMap<Network, u32>>,
    assistant: crate::assistant::Assistant,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("datadir", &self.datadir)
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// State for `datadir`, reading `desktop.json` from it.
    pub fn new(datadir: PathBuf, env: Box<EnvLookup>, clock: Arc<dyn Clock>) -> Self {
        let stored = settings::load(&datadir.join(SETTINGS_FILE));
        let now = clock.now();
        Self {
            datadir,
            env,
            clock,
            settings: Mutex::new(stored),
            session: Mutex::new(Session::new(now)),
            gates: Mutex::new(HashMap::new()),
            synced: Mutex::new(HashMap::new()),
            assistant: crate::assistant::Assistant::default(),
        }
    }

    /// The real app: `BTCW_DATADIR` if set, else btcw's default data directory, so the CLI and
    /// the app share wallets.
    pub fn from_process_env() -> Self {
        let env = |key: &str| std::env::var(key).ok();
        let datadir = env("BTCW_DATADIR")
            .filter(|dir| !dir.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(config::default_datadir);
        Self::new(datadir, Box::new(env), Arc::new(SystemClock))
    }

    /// An environment variable, through the same (test-injectable) lookup as the config.
    pub fn env_var(&self, key: &str) -> Option<String> {
        (self.env)(key)
    }

    pub fn datadir(&self) -> &Path {
        &self.datadir
    }

    pub fn settings_path(&self) -> PathBuf {
        self.datadir.join(SETTINGS_FILE)
    }

    pub fn now(&self) -> Instant {
        self.clock.now()
    }

    pub fn stored_settings(&self) -> StoredSettings {
        lock(&self.settings).clone()
    }

    pub(crate) fn settings_guard(&self) -> MutexGuard<'_, StoredSettings> {
        lock(&self.settings)
    }

    /// The core config: these settings as overrides, then `BTCW_*`, then `btcw.toml`.
    pub fn config_with(&self, stored: &StoredSettings) -> btcw_core::Result<Config> {
        Config::load_with(stored.to_overrides(&self.datadir), |key| (self.env)(key))
    }

    pub fn config(&self) -> btcw_core::Result<Config> {
        let stored = self.stored_settings();
        self.config_with(&stored)
    }

    /// Start of every command except `lock`: enforce the auto-lock deadline, record user
    /// activity, then resolve the config and drop a signer that belongs to another network.
    pub fn begin(&self, activity: Activity) -> ApiResult<Config> {
        self.touch(activity);
        let cfg = self.config()?;
        let mut session = self.session();
        if session
            .unlocked
            .as_ref()
            .is_some_and(|u| u.network != cfg.network)
        {
            tracing::info!("locking: the active network changed");
            session.lock();
        }
        Ok(cfg)
    }

    /// The auto-lock check (and, for user activity, pushing the deadline back) without
    /// resolving the config.
    pub fn touch(&self, activity: Activity) {
        let limit = self.idle_limit();
        let now = self.now();
        let mut session = self.session();
        if session.unlocked.is_some()
            && now.saturating_duration_since(session.last_activity) > limit
        {
            tracing::info!(
                "auto-lock: no activity for the configured time; the signing key was wiped"
            );
            session.lock();
        }
        if activity == Activity::User {
            session.last_activity = now;
        }
    }

    /// `auto_lock_minutes` plus [`AUTO_LOCK_GRACE`].
    pub fn idle_limit(&self) -> Duration {
        let minutes = u64::from(lock(&self.settings).auto_lock_minutes);
        Duration::from_secs(minutes.saturating_mul(60)) + AUTO_LOCK_GRACE
    }

    pub(crate) fn session(&self) -> MutexGuard<'_, Session> {
        lock(&self.session)
    }

    /// One gate per network: the wallet's file lock is per open file, so two commands of this
    /// process opening it at once would refuse each other (`wallet_in_use`).
    pub(crate) fn gate(&self, network: Network) -> Arc<Mutex<()>> {
        Arc::clone(lock(&self.gates).entry(network).or_default())
    }

    pub(crate) fn assistant(&self) -> &crate::assistant::Assistant {
        &self.assistant
    }

    pub(crate) fn remember_synced(&self, network: Network, height: u32) {
        lock(&self.synced).insert(network, height);
    }

    pub(crate) fn cached_synced(&self, network: Network) -> Option<u32> {
        lock(&self.synced).get(&network).copied()
    }
}

/// Ignore poisoning: every update is a plain assignment, so the data stays consistent.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The signing session. Never printed: `Debug` says only what is there.
pub(crate) struct Session {
    unlocked: Option<UnlockedSigner>,
    last_activity: Instant,
    pending: Option<PendingSend>,
    /// Bumped on every lock, so state tied to the session (the assistant's chat) can tell.
    epoch: u64,
}

struct UnlockedSigner {
    network: Network,
    /// Wiped on drop (`Signer`'s `Drop` erases the master key).
    signer: Signer,
}

/// A payment the user is looking at. The UI only has `id` and the preview.
pub(crate) struct PendingSend {
    pub id: String,
    pub network: Network,
    pub psbt: Psbt,
    pub created: Instant,
    /// For a fee bump: the unconfirmed payment this one replaces.
    pub replaces: Option<Txid>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field(
                "unlocked",
                &self.unlocked.as_ref().map(|u| u.network.to_string()),
            )
            .field("pending_send", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}

impl Session {
    fn new(now: Instant) -> Self {
        Self {
            unlocked: None,
            last_activity: now,
            pending: None,
            epoch: 0,
        }
    }

    /// Drop the signer (wiping the key) and any prepared payment.
    pub fn lock(&mut self) {
        self.unlocked = None;
        self.pending = None;
        self.epoch = self.epoch.wrapping_add(1);
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Keep `signer` for `network`, replacing (and wiping) any previous one. A prepared payment
    /// for another network is dropped.
    pub fn unlock(&mut self, network: Network, signer: Signer, now: Instant) {
        if self.pending.as_ref().is_some_and(|p| p.network != network) {
            self.pending = None;
        }
        self.unlocked = Some(UnlockedSigner { network, signer });
        self.last_activity = now;
    }

    pub fn is_unlocked_for(&self, network: Network) -> bool {
        self.signer_for(network).is_some()
    }

    pub fn signer_for(&self, network: Network) -> Option<&Signer> {
        self.unlocked
            .as_ref()
            .filter(|u| u.network == network)
            .map(|u| &u.signer)
    }

    /// Remove and return the prepared payment, but only if its id is `id`.
    pub fn take_pending(&mut self, id: &str) -> Option<PendingSend> {
        if self.pending.as_ref().is_some_and(|p| p.id == id) {
            self.pending.take()
        } else {
            None
        }
    }

    /// Remove and return whatever payment was prepared.
    pub fn take_any_pending(&mut self) -> Option<PendingSend> {
        self.pending.take()
    }

    pub fn set_pending(&mut self, pending: PendingSend) {
        self.pending = Some(pending);
    }

    #[cfg(test)]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[cfg(test)]
pub(crate) mod test_clock {
    use super::*;

    /// A clock that only moves when told to.
    pub struct ManualClock(Mutex<Instant>);

    impl ManualClock {
        pub fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(Instant::now())))
        }

        pub fn advance(&self, by: Duration) {
            let mut now = lock(&self.0);
            *now += by;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            *lock(&self.0)
        }
    }
}
