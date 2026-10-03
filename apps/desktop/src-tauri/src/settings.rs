//! Desktop settings: `<datadir>/desktop.json`, applied to btcw-core's config as [`Overrides`].
//!
//! ```json
//! { "network": "regtest", "rpc_url": null, "rpc_cookie": null,
//!   "auto_lock_minutes": 5, "mainnet_opt_in": false }
//! ```
//! A `null` (or missing) value means "not set here", so `BTCW_*` environment variables,
//! `<datadir>/btcw.toml` and the built-in defaults still apply underneath, exactly as for the CLI.
//! The file is written only by [`save`] (mode 0600, temp file + rename) and read once at start-up.
//! `rpc_url` may carry `user:pass@`, so it is never logged or shown in `Debug`, and the file is
//! private to the user.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use btcw_core::bitcoin::Network;
use btcw_core::config::{self, Overrides};
use btcw_core::{Result, WalletError};
use serde::{Deserialize, Serialize};

pub const SETTINGS_FILE: &str = "desktop.json";
pub const DEFAULT_AUTO_LOCK_MINUTES: u32 = 5;
pub const AUTO_LOCK_MINUTES: RangeInclusive<u32> = 1..=60;
/// Long enough for any real URL or path; anything longer is a paste accident.
const MAX_TEXT_LEN: usize = 2048;

/// What `desktop.json` holds.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredSettings {
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub rpc_url: Option<String>,
    #[serde(default)]
    pub rpc_cookie: Option<String>,
    #[serde(default = "default_auto_lock")]
    pub auto_lock_minutes: u32,
    #[serde(default)]
    pub mainnet_opt_in: bool,
}

fn default_auto_lock() -> u32 {
    DEFAULT_AUTO_LOCK_MINUTES
}

impl Default for StoredSettings {
    fn default() -> Self {
        Self {
            network: None,
            rpc_url: None,
            rpc_cookie: None,
            auto_lock_minutes: DEFAULT_AUTO_LOCK_MINUTES,
            mainnet_opt_in: false,
        }
    }
}

/// The URL can hold RPC credentials (`http://user:pass@host`), so `Debug` only says whether it
/// is set.
impl std::fmt::Debug for StoredSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredSettings")
            .field("network", &self.network)
            .field("rpc_url", &self.rpc_url.as_ref().map(|_| "<set>"))
            .field("rpc_cookie", &self.rpc_cookie)
            .field("auto_lock_minutes", &self.auto_lock_minutes)
            .field("mainnet_opt_in", &self.mainnet_opt_in)
            .finish()
    }
}

/// `Settings` in `types.ts`: what `get_settings` returns and `set_settings` takes.
/// `network` is the *effective* network (after env and `btcw.toml`), never null.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub network: String,
    pub rpc_url: Option<String>,
    pub rpc_cookie: Option<String>,
    pub auto_lock_minutes: u32,
    /// The user typed the mainnet confirmation in Settings (PLAN §4.1, second gate).
    #[serde(default)]
    pub mainnet_opt_in: bool,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("network", &self.network)
            .field("rpc_url", &self.rpc_url.as_ref().map(|_| "<set>"))
            .field("rpc_cookie", &self.rpc_cookie)
            .field("auto_lock_minutes", &self.auto_lock_minutes)
            .field("mainnet_opt_in", &self.mainnet_opt_in)
            .finish()
    }
}

impl StoredSettings {
    /// The view for `get_settings`, given the network the config actually resolved to.
    pub fn to_settings(&self, effective: Network) -> Settings {
        Settings {
            network: effective.to_string(),
            rpc_url: self.rpc_url.clone(),
            rpc_cookie: self.rpc_cookie.clone(),
            auto_lock_minutes: self.auto_lock_minutes,
            mainnet_opt_in: self.mainnet_opt_in,
        }
    }

    /// As btcw-core overrides for `datadir`. Unset fields stay `None` so env/file/defaults apply.
    pub fn to_overrides(&self, datadir: &Path) -> Overrides {
        Overrides {
            network: self.network.clone(),
            datadir: Some(datadir.to_path_buf()),
            rpc_url: self.rpc_url.clone(),
            rpc_user: None,
            rpc_pass: None,
            rpc_cookie: self.rpc_cookie.as_deref().map(expand_home),
            mainnet_opt_in: self.mainnet_opt_in,
        }
    }

    /// Drop values that can't be used any more, so a stale or hand-edited file never stops the
    /// app from starting (e.g. `"bitcoin"` saved by a mainnet build, then opened by a build
    /// without mainnet). Each fix is logged; the file itself is left alone until the next save.
    fn sanitized(mut self) -> Self {
        if let Some(name) = &self.network {
            let usable = config::parse_network(name).is_ok_and(|network| {
                config::selectable_networks().contains(&network)
                    && config::check_network_allowed(network, self.mainnet_opt_in).is_ok()
            });
            if !usable {
                tracing::warn!(
                    network = %name,
                    "{SETTINGS_FILE}: this network is not available in this build; using the default"
                );
                self.network = None;
            }
        }
        if !AUTO_LOCK_MINUTES.contains(&self.auto_lock_minutes) {
            tracing::warn!(
                minutes = self.auto_lock_minutes,
                "{SETTINGS_FILE}: auto-lock is outside 1..=60 minutes; using {DEFAULT_AUTO_LOCK_MINUTES}"
            );
            self.auto_lock_minutes = DEFAULT_AUTO_LOCK_MINUTES;
        }
        self.rpc_url = self.rpc_url.and_then(non_empty);
        self.rpc_cookie = self.rpc_cookie.and_then(non_empty);
        self
    }
}

/// Check a `set_settings` request and turn it into what gets stored.
/// Returns the stored form and the network it selects.
pub fn validate(next: &Settings) -> Result<(StoredSettings, Network)> {
    let network = config::parse_network(&next.network)?;
    if !config::selectable_networks().contains(&network) {
        return Err(match network {
            Network::Bitcoin => WalletError::MainnetDisabled,
            other => WalletError::Config(format!("network {other} is not available")),
        });
    }
    // Mainnet needs the opt-in on top of the build feature.
    config::check_network_allowed(network, next.mainnet_opt_in)?;

    if !AUTO_LOCK_MINUTES.contains(&next.auto_lock_minutes) {
        return Err(WalletError::Config(format!(
            "auto-lock must be between {} and {} minutes",
            AUTO_LOCK_MINUTES.start(),
            AUTO_LOCK_MINUTES.end()
        )));
    }

    let rpc_url = next.rpc_url.clone().and_then(non_empty);
    if let Some(url) = &rpc_url {
        // The URL is not echoed back: it may contain RPC credentials.
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://"))
            || url.len() > MAX_TEXT_LEN
            || url.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(WalletError::Config(
                "the node RPC URL must be a full http:// URL without spaces, like http://127.0.0.1:48332"
                    .into(),
            ));
        }
    }
    let rpc_cookie = next.rpc_cookie.clone().and_then(non_empty);
    if let Some(path) = &rpc_cookie
        && (path.len() > MAX_TEXT_LEN || path.chars().any(char::is_control))
    {
        return Err(WalletError::Config(
            "the cookie file path is not a valid path".into(),
        ));
    }

    Ok((
        StoredSettings {
            network: Some(network.to_string()),
            rpc_url,
            rpc_cookie,
            auto_lock_minutes: next.auto_lock_minutes,
            mainnet_opt_in: next.mainnet_opt_in,
        },
        network,
    ))
}

/// Read `desktop.json`. A missing file means defaults; an unreadable or invalid one is logged
/// (never its contents) and also gives defaults, so a damaged file can't keep the app from
/// opening. The next save replaces it.
pub fn load(path: &Path) -> StoredSettings {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return StoredSettings::default(),
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, "cannot read the desktop settings; using defaults");
            return StoredSettings::default();
        }
    };
    match serde_json::from_str::<StoredSettings>(&text) {
        Ok(stored) => stored.sanitized(),
        Err(e) => {
            // serde_json's message gives line, column and the problem, not the file's contents.
            tracing::warn!(file = %path.display(), error = %e, "invalid desktop settings; using defaults");
            StoredSettings::default()
        }
    }
}

/// Write `desktop.json` atomically (temp file in the same directory, fsync, rename), mode 0600.
pub fn save(path: &Path, settings: &StoredSettings) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        WalletError::Config(format!("{} has no parent directory", path.display()))
    })?;
    fs::create_dir_all(dir).map_err(|e| io_context("creating", dir, e))?;
    let json = serde_json::to_vec_pretty(settings)
        .map_err(|e| WalletError::Config(format!("cannot encode the settings: {e}")))?;

    let tmp = path.with_extension("json.tmp");
    // A temp file left by a crash would make `create_new` fail forever.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_context("removing", &tmp, e)),
    }
    let written = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&json)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(io_context("writing", path, e));
    }
    Ok(())
}

fn io_context(action: &str, path: &Path, e: std::io::Error) -> WalletError {
    WalletError::Io(std::io::Error::new(
        e.kind(),
        format!("{action} {}: {e}", path.display()),
    ))
}

fn non_empty(text: String) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else if trimmed.len() == text.len() {
        Some(text)
    } else {
        Some(trimmed.to_owned())
    }
}

/// `~/x` → `$HOME/x`, as a shell would (the Settings screen suggests `~/.bitcoin/...`).
fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(network: &str) -> Settings {
        Settings {
            network: network.into(),
            rpc_url: None,
            rpc_cookie: None,
            auto_lock_minutes: 5,
            mainnet_opt_in: false,
        }
    }

    #[test]
    fn validates_network_auto_lock_and_urls() {
        let (stored, network) = validate(&request("regtest")).unwrap();
        assert_eq!(network, Network::Regtest);
        assert_eq!(stored.network.as_deref(), Some("regtest"));

        assert_eq!(validate(&request("testnet")).unwrap_err().code(), "config");
        assert_eq!(validate(&request("dogecoin")).unwrap_err().code(), "config");
        // Without the opt-in mainnet is refused in every build; with it only in mainnet builds.
        assert_eq!(
            validate(&request("bitcoin")).unwrap_err().code(),
            "mainnet_disabled"
        );
        let opted_in = Settings {
            mainnet_opt_in: true,
            ..request("bitcoin")
        };
        // (Asked of btcw-core rather than a cfg here: the feature lives there.)
        if config::selectable_networks().contains(&Network::Bitcoin) {
            assert_eq!(validate(&opted_in).unwrap().1, Network::Bitcoin);
        } else {
            assert_eq!(validate(&opted_in).unwrap_err().code(), "mainnet_disabled");
        }

        for minutes in [0, 61, 1440] {
            let bad = Settings {
                auto_lock_minutes: minutes,
                ..request("signet")
            };
            assert_eq!(validate(&bad).unwrap_err().code(), "config", "{minutes}");
        }
        for minutes in [1, 60] {
            let ok = Settings {
                auto_lock_minutes: minutes,
                ..request("signet")
            };
            assert!(validate(&ok).is_ok(), "{minutes}");
        }

        let with_url = |url: &str| Settings {
            rpc_url: Some(url.into()),
            ..request("regtest")
        };
        let (stored, _) = validate(&with_url("  http://127.0.0.1:18443 ")).unwrap();
        assert_eq!(stored.rpc_url.as_deref(), Some("http://127.0.0.1:18443"));
        assert_eq!(validate(&with_url("   ")).unwrap().0.rpc_url, None);
        for bad in [
            "127.0.0.1:18443",
            "ftp://host",
            "http://a b",
            "http://x\u{7}y",
        ] {
            let err = validate(&with_url(bad)).unwrap_err();
            assert_eq!(err.code(), "config", "{bad}");
        }
        // Credentials in a rejected URL are not echoed back.
        let err = validate(&with_url("htp://alice:hunter2@host")).unwrap_err();
        assert!(!err.to_string().contains("hunter2"));
    }

    #[test]
    fn saves_atomically_with_private_permissions_and_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(SETTINGS_FILE);
        let stored = StoredSettings {
            network: Some("signet".into()),
            rpc_url: Some("http://u:p@127.0.0.1:38332".into()),
            rpc_cookie: None,
            auto_lock_minutes: 7,
            mainnet_opt_in: false,
        };
        save(&path, &stored).unwrap();
        assert_eq!(load(&path), stored);
        assert!(!path.with_extension("json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // Overwrite (and a stale temp file from a crash doesn't block it).
        fs::write(path.with_extension("json.tmp"), b"junk").unwrap();
        let next = StoredSettings {
            auto_lock_minutes: 9,
            ..stored
        };
        save(&path, &next).unwrap();
        assert_eq!(load(&path).auto_lock_minutes, 9);
        // Debug never shows the URL (it can hold credentials).
        assert!(!format!("{next:?}").contains("u:p@"));
    }

    #[test]
    fn a_missing_or_damaged_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        assert_eq!(load(&path), StoredSettings::default());
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(&path), StoredSettings::default());
        fs::write(&path, r#"{ "network": "regtest", "colour": "blue" }"#).unwrap();
        assert_eq!(load(&path), StoredSettings::default());
        // Unusable values are dropped one by one; the rest is kept.
        fs::write(
            &path,
            r#"{ "network": "bitcoin", "auto_lock_minutes": 999, "rpc_url": " ", "rpc_cookie": "/c" }"#,
        )
        .unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.network, None);
        assert_eq!(loaded.auto_lock_minutes, DEFAULT_AUTO_LOCK_MINUTES);
        assert_eq!(loaded.rpc_url, None);
        assert_eq!(loaded.rpc_cookie.as_deref(), Some("/c"));
    }

    #[test]
    fn overrides_leave_unset_values_to_env_and_file() {
        let dir = Path::new("/data/btcw");
        let o = StoredSettings::default().to_overrides(dir);
        assert_eq!(o.network, None);
        assert_eq!(o.rpc_url, None);
        assert_eq!(o.rpc_cookie, None);
        assert_eq!(o.datadir.as_deref(), Some(dir));
        assert!(!o.mainnet_opt_in);

        let o = StoredSettings {
            rpc_cookie: Some("~/.bitcoin/regtest/.cookie".into()),
            ..Default::default()
        }
        .to_overrides(dir);
        let cookie = o.rpc_cookie.unwrap();
        if let Some(home) = dirs::home_dir() {
            assert_eq!(cookie, home.join(".bitcoin/regtest/.cookie"));
        }
    }
}
