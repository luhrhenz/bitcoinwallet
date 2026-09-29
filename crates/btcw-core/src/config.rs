//! Network policy, data directories and node RPC settings.
//!
//! Precedence, highest first: explicit [`Overrides`] (CLI flags / desktop settings) →
//! environment (`BTCW_*`) → `<datadir>/btcw.toml` → built-in defaults.
//! The library never loads `.env` itself; binaries call `dotenvy::dotenv()` if they want it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use secrecy::SecretString;
use serde::Deserialize;

use crate::bitcoin::Network;
use crate::error::{Result, WalletError};

/// Networks offered in pickers, in display order. Mainnet is appended only when compiled in.
pub fn selectable_networks() -> Vec<Network> {
    let mut nets = vec![Network::Testnet4, Network::Signet, Network::Regtest];
    if cfg!(feature = "mainnet") {
        nets.push(Network::Bitcoin);
    }
    nets
}

/// Parse a user-supplied network name. Accepts `mainnet` as an alias for `bitcoin`.
pub fn parse_network(s: &str) -> Result<Network> {
    match s.trim().to_ascii_lowercase().as_str() {
        "testnet4" => Ok(Network::Testnet4),
        "signet" => Ok(Network::Signet),
        "regtest" => Ok(Network::Regtest),
        "bitcoin" | "mainnet" => Ok(Network::Bitcoin),
        "testnet" | "testnet3" => Err(WalletError::Config(
            "testnet3 is not supported; use testnet4".into(),
        )),
        other => Err(WalletError::Config(format!(
            "unknown network `{other}` (expected testnet4, signet or regtest)"
        ))),
    }
}

/// Mainnet needs both the `mainnet` cargo feature *and* a runtime opt-in (PLAN §4.1).
pub fn check_network_allowed(network: Network, mainnet_opt_in: bool) -> Result<()> {
    match network {
        Network::Testnet4 | Network::Signet | Network::Regtest => Ok(()),
        Network::Bitcoin if cfg!(feature = "mainnet") && mainnet_opt_in => Ok(()),
        Network::Bitcoin => Err(WalletError::MainnetDisabled),
        other => Err(WalletError::Config(format!(
            "network {other} is not supported"
        ))),
    }
}

/// BIP44 coin type used in `m/84'/coin'/...`: 0 for mainnet, 1 for every test network.
pub fn coin_type(network: Network) -> u32 {
    match network {
        Network::Bitcoin => 0,
        _ => 1,
    }
}

/// Bitcoin Core's default RPC port for a network.
pub fn default_rpc_port(network: Network) -> u16 {
    match network {
        Network::Bitcoin => 8332,
        Network::Testnet => 18332,
        Network::Testnet4 => 48332,
        Network::Signet => 38332,
        _ => 18443, // regtest
    }
}

/// Sub-directory Bitcoin Core uses inside its datadir for this network ("" = mainnet).
fn core_subdir(network: Network) -> &'static str {
    match network {
        Network::Bitcoin => "",
        Network::Testnet => "testnet3",
        Network::Testnet4 => "testnet4",
        Network::Signet => "signet",
        _ => "regtest",
    }
}

#[derive(Debug, Clone)]
pub enum RpcAuth {
    /// Path to Bitcoin Core's `.cookie` file (default; no password to configure).
    Cookie(PathBuf),
    UserPass {
        user: String,
        pass: SecretString,
    },
}

#[derive(Debug, Clone)]
pub struct RpcConfig {
    pub url: String,
    pub auth: RpcAuth,
}

impl RpcConfig {
    /// A node on this machine using Bitcoin Core's default port and cookie location.
    pub fn local_default(network: Network) -> Self {
        let core_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".bitcoin")
            .join(core_subdir(network));
        Self {
            url: format!("http://127.0.0.1:{}", default_rpc_port(network)),
            auth: RpcAuth::Cookie(core_dir.join(".cookie")),
        }
    }
}

/// Values set explicitly by a frontend. `None` means "fall through to env / file / default".
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub network: Option<String>,
    pub datadir: Option<PathBuf>,
    pub rpc_url: Option<String>,
    pub rpc_user: Option<String>,
    pub rpc_pass: Option<SecretString>,
    pub rpc_cookie: Option<PathBuf>,
    pub mainnet_opt_in: bool,
}

/// `<datadir>/btcw.toml`
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    network: Option<String>,
    #[serde(default)]
    mainnet_opt_in: bool,
    /// Per-network node settings: `[rpc.testnet4] url = "..."`.
    #[serde(default)]
    rpc: HashMap<String, FileRpc>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRpc {
    url: Option<String>,
    user: Option<String>,
    pass: Option<String>,
    cookie: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub network: Network,
    pub datadir: PathBuf,
    pub rpc: RpcConfig,
    pub mainnet_opt_in: bool,
}

impl Config {
    pub const DEFAULT_NETWORK: Network = Network::Testnet4;

    /// Resolve configuration from overrides, the process environment and the config file.
    pub fn load(overrides: Overrides) -> Result<Self> {
        Self::load_with(overrides, |k| std::env::var(k).ok())
    }

    /// Same as [`Config::load`] with an injectable environment (for tests).
    pub fn load_with(overrides: Overrides, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let datadir = overrides
            .datadir
            .or_else(|| env("BTCW_DATADIR").map(PathBuf::from))
            .unwrap_or_else(default_datadir);

        let file = read_file_config(&datadir.join("btcw.toml"))?;

        let network = match overrides
            .network
            .or_else(|| env("BTCW_NETWORK"))
            .or(file.network)
        {
            Some(s) => parse_network(&s)?,
            None => Self::DEFAULT_NETWORK,
        };
        let mainnet_opt_in = overrides.mainnet_opt_in || file.mainnet_opt_in;
        check_network_allowed(network, mainnet_opt_in)?;

        let file_rpc = file.rpc.get(&network.to_string());
        let mut rpc = RpcConfig::local_default(network);
        if let Some(url) = overrides
            .rpc_url
            .or_else(|| env("BTCW_RPC_URL"))
            .or_else(|| file_rpc.and_then(|r| r.url.clone()))
        {
            rpc.url = url;
        }
        let user = overrides
            .rpc_user
            .or_else(|| env("BTCW_RPC_USER"))
            .or_else(|| file_rpc.and_then(|r| r.user.clone()));
        let pass = overrides
            .rpc_pass
            .or_else(|| env("BTCW_RPC_PASS").map(SecretString::from))
            .or_else(|| {
                file_rpc
                    .and_then(|r| r.pass.clone())
                    .map(SecretString::from)
            });
        let cookie = overrides
            .rpc_cookie
            .or_else(|| env("BTCW_RPC_COOKIE").map(PathBuf::from))
            .or_else(|| file_rpc.and_then(|r| r.cookie.clone()));
        match (user, pass, cookie) {
            (Some(user), Some(pass), _) => rpc.auth = RpcAuth::UserPass { user, pass },
            (Some(_), None, _) | (None, Some(_), _) => {
                return Err(WalletError::Config(
                    "RPC user and password must be set together".into(),
                ));
            }
            (None, None, Some(cookie)) => rpc.auth = RpcAuth::Cookie(cookie),
            (None, None, None) => {}
        }

        Ok(Self {
            network,
            datadir,
            rpc,
            mainnet_opt_in,
        })
    }

    /// `<datadir>/<network>/`. Each network's wallet lives apart so they can never mix.
    pub fn network_dir(&self) -> PathBuf {
        self.datadir.join(self.network.to_string())
    }

    pub fn wallet_db_path(&self) -> PathBuf {
        self.network_dir().join("wallet.sqlite")
    }

    pub fn keystore_path(&self) -> PathBuf {
        self.network_dir().join("seed.enc")
    }

    /// Held (via `File::try_lock`) while a process has the wallet open.
    pub fn lock_path(&self) -> PathBuf {
        self.network_dir().join("wallet.lock")
    }

    pub fn coin_type(&self) -> u32 {
        coin_type(self.network)
    }
}

/// `$XDG_DATA_HOME/btcw` on Linux (e.g. `~/.local/share/btcw`).
pub fn default_datadir() -> PathBuf {
    dirs::data_dir()
        .map(|d| d.join("btcw"))
        .unwrap_or_else(|| PathBuf::from(".btcw"))
}

fn read_file_config(path: &Path) -> Result<FileConfig> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            toml::from_str(&s).map_err(|e| WalletError::Config(format!("{}: {e}", path.display())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn overrides_in(dir: &Path) -> Overrides {
        Overrides {
            datadir: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn defaults_to_testnet4() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cfg = Config::load_with(overrides_in(dir.path()), no_env)?;
        assert_eq!(cfg.network, Network::Testnet4);
        assert_eq!(cfg.rpc.url, "http://127.0.0.1:48332");
        assert_eq!(cfg.coin_type(), 1);
        assert_eq!(
            cfg.keystore_path(),
            dir.path().join("testnet4").join("seed.enc")
        );
        Ok(())
    }

    #[test]
    fn mainnet_is_locked_without_feature_and_opt_in() {
        assert!(matches!(
            check_network_allowed(Network::Bitcoin, false),
            Err(WalletError::MainnetDisabled)
        ));
        #[cfg(not(feature = "mainnet"))]
        assert!(matches!(
            check_network_allowed(Network::Bitcoin, true),
            Err(WalletError::MainnetDisabled)
        ));
        #[cfg(feature = "mainnet")]
        assert!(check_network_allowed(Network::Bitcoin, true).is_ok());
    }

    #[test]
    fn rejects_testnet3_and_unknown_names() {
        assert!(parse_network("testnet").is_err());
        assert!(parse_network("dogecoin").is_err());
        assert!(matches!(parse_network(" Regtest "), Ok(Network::Regtest)));
    }

    #[test]
    fn precedence_override_then_env_then_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("btcw.toml"),
            "network = \"signet\"\n[rpc.regtest]\nurl = \"http://file:1\"\nuser = \"u\"\npass = \"p\"\n",
        )?;

        let from_file = Config::load_with(overrides_in(dir.path()), no_env)?;
        assert_eq!(from_file.network, Network::Signet);

        let env = |k: &str| (k == "BTCW_NETWORK").then(|| "regtest".to_string());
        let from_env = Config::load_with(overrides_in(dir.path()), env)?;
        assert_eq!(from_env.network, Network::Regtest);
        assert_eq!(from_env.rpc.url, "http://file:1");
        match &from_env.rpc.auth {
            RpcAuth::UserPass { user, pass } => {
                assert_eq!(user, "u");
                assert_eq!(pass.expose_secret(), "p");
            }
            RpcAuth::Cookie(_) => panic!("expected user/pass auth from file"),
        }

        let mut o = overrides_in(dir.path());
        o.network = Some("testnet4".into());
        o.rpc_url = Some("http://flag:2".into());
        let from_flag = Config::load_with(o, env)?;
        assert_eq!(from_flag.network, Network::Testnet4);
        assert_eq!(from_flag.rpc.url, "http://flag:2");
        Ok(())
    }

    #[test]
    fn half_configured_userpass_is_an_error() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut o = overrides_in(dir.path());
        o.rpc_user = Some("alice".into());
        assert!(matches!(
            Config::load_with(o, no_env),
            Err(WalletError::Config(_))
        ));
        Ok(())
    }
}
