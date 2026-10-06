//! A throwaway regtest `bitcoind` for integration tests (feature `test-utils`).
//!
//! Binary: `$BITCOIND_EXE`, else `bitcoind` on `PATH`. Each node gets a temp datadir and free
//! ports and stops on drop. Tests skip when [`TestNode::available`] is false.
//!
//! RPCs use raw `call::<Value>` so they don't depend on typed response structs.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bdk_bitcoind_rpc::bitcoincore_rpc::jsonrpc::{self, simple_http};
use bdk_bitcoind_rpc::bitcoincore_rpc::{Client, RpcApi};
use serde_json::{Value, json};

use crate::bitcoin::address::NetworkUnchecked;
use crate::bitcoin::{Address, Amount, BlockHash, Network, Txid};
use crate::config::{RpcAuth, RpcConfig};
use crate::error::{Result, WalletError};

pub struct TestNode {
    child: Child,
    _dir: tempfile::TempDir,
    rpc: RpcConfig,
    client: Client,
}

fn rpc_err(e: impl std::fmt::Display) -> WalletError {
    WalletError::Rpc(e.to_string())
}

fn bitcoind_exe() -> PathBuf {
    std::env::var_os("BITCOIND_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("bitcoind"))
}

/// Same 120 s timeout as `chain::Node`: with several nodes mining in parallel, a
/// `generatetoaddress` batch can exceed the default 15 s ("Resource temporarily unavailable").
fn client(url: &str, cookie: &std::path::Path) -> Result<Client> {
    let contents = std::fs::read_to_string(cookie)?;
    let (user, pass) = contents
        .trim_end()
        .split_once(':')
        .ok_or_else(|| rpc_err("malformed cookie file"))?;
    let transport = simple_http::Builder::new()
        .timeout(Duration::from_secs(120))
        .url(url)
        .map_err(rpc_err)?
        .auth(user, Some(pass))
        .build();
    Ok(Client::from_jsonrpc(jsonrpc::Client::with_transport(
        transport,
    )))
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

impl TestNode {
    /// True if a `bitcoind` binary can be executed.
    ///
    /// `-nosettings`: Core v31 rewrites `settings.json` even for `-version`, and concurrent
    /// probes can collide on it.
    ///
    /// # Panics
    /// When `BITCOIND_EXE` is set but doesn't run, so a broken setup can't pass silently.
    pub fn available() -> bool {
        let runs = Command::new(bitcoind_exe())
            .args(["-nosettings", "-version"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !runs && let Some(exe) = std::env::var_os("BITCOIND_EXE") {
            panic!(
                "BITCOIND_EXE={} is set but `-version` failed; refusing to skip node tests",
                PathBuf::from(exe).display()
            );
        }
        runs
    }

    /// Start a node, create a Core wallet named `faucet`, and mine 101 blocks to it so it has
    /// one spendable 50-BTC coinbase.
    pub fn start() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let rpc_port = free_port()?;
        let p2p_port = free_port()?;
        let child = Command::new(bitcoind_exe())
            .arg("-regtest")
            .arg(format!("-datadir={}", dir.path().display()))
            .arg(format!("-rpcport={rpc_port}"))
            .arg(format!("-port={p2p_port}"))
            .args([
                "-listen=0",
                "-fallbackfee=0.0002",
                "-printtoconsole=0",
                "-txindex=0",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        let cookie = dir.path().join("regtest").join(".cookie");
        let url = format!("http://127.0.0.1:{rpc_port}");

        let deadline = Instant::now() + Duration::from_secs(30);
        let client = loop {
            if cookie.exists()
                && let Ok(c) = client(&url, &cookie)
                && c.call::<Value>("getblockcount", &[]).is_ok()
            {
                break c;
            }
            if Instant::now() > deadline {
                return Err(WalletError::Rpc("bitcoind did not start within 30s".into()));
            }
            std::thread::sleep(Duration::from_millis(100));
        };

        let node = Self {
            child,
            _dir: dir,
            rpc: RpcConfig {
                url,
                auth: RpcAuth::Cookie(cookie),
            },
            client,
        };
        node.call("createwallet", &[json!("faucet")])?;
        node.mine(101)?;
        Ok(node)
    }

    /// Settings for `chain::Node::connect(.., Network::Regtest)`.
    pub fn rpc_config(&self) -> RpcConfig {
        self.rpc.clone()
    }

    pub fn call(&self, method: &str, params: &[Value]) -> Result<Value> {
        self.client.call(method, params).map_err(rpc_err)
    }

    pub fn faucet_address(&self) -> Result<Address> {
        let s = self.call("getnewaddress", &[])?;
        let s = s
            .as_str()
            .ok_or_else(|| rpc_err("getnewaddress: not a string"))?;
        s.parse::<Address<NetworkUnchecked>>()
            .map_err(rpc_err)?
            .require_network(Network::Regtest)
            .map_err(rpc_err)
    }

    /// Mine `n` blocks to the faucet, in batches (mining is ~200 ms/block on Core v31).
    pub fn mine(&self, n: u64) -> Result<Vec<BlockHash>> {
        let to = self.faucet_address()?.to_string();
        let mut all = Vec::with_capacity(n as usize);
        let mut left = n;
        while left > 0 {
            let batch = left.min(25);
            let hashes = self.call("generatetoaddress", &[json!(batch), json!(to)])?;
            all.extend(serde_json::from_value::<Vec<BlockHash>>(hashes).map_err(rpc_err)?);
            left -= batch;
        }
        Ok(all)
    }

    /// Send from the faucet (unconfirmed until you [`TestNode::mine`]).
    pub fn fund(&self, to: &Address, amount: Amount) -> Result<Txid> {
        let txid = self.call(
            "sendtoaddress",
            &[json!(to.to_string()), json!(amount.to_btc())],
        )?;
        serde_json::from_value(txid).map_err(rpc_err)
    }

    pub fn tip_height(&self) -> Result<u32> {
        let h = self.call("getblockcount", &[])?;
        h.as_u64()
            .and_then(|h| u32::try_from(h).ok())
            .ok_or_else(|| rpc_err("getblockcount: bad value"))
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        let _ = self.client.call::<Value>("stop", &[]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
