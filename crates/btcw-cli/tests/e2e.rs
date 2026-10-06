//! End-to-end user journeys: the real `btcw` binary, one process per command, against a real
//! regtest `bitcoind`.
//!
//! These chain commands the way a person would and check that the parts agree with each other
//! and with Bitcoin Core: fees, balances to the satoshi, two wallets paying each other, and
//! restores.
//!
//! Assertions are tagged with the requirement they check:
//! R1 create · R2 restore · R3 BIP84 derivation · R4 receive addresses + used flags · R5 sync ·
//! R6 balance · R7 history · R8 PSBT signing · R9 broadcast · R10 tx status · R11 persistence.
//!
//! Each test starts its own `bitcoind` and is skipped only when none can be run. Nodes and
//! spawned `btcw` processes are stopped on drop, so a failing assertion leaves nothing running.

use std::collections::BTreeSet;
use std::error::Error;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use btcw_core::WalletError;
use btcw_core::api;
use btcw_core::bdk_wallet::rusqlite::Connection;
use btcw_core::bdk_wallet::{KeychainKind, Wallet};
use btcw_core::bitcoin::address::NetworkUnchecked;
use btcw_core::bitcoin::{Address, Amount, Network};
use btcw_core::config::{Config, Overrides, RpcAuth};
use btcw_core::keys;
use btcw_core::testnode::TestNode;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const PASSWORD: &str = "correct horse battery";
/// Nothing listens on port 1 (tcpmux): every node call is refused at once.
const CLOSED_RPC_URL: &str = "http://127.0.0.1:1";
const WALLET_IN_USE: &str = "wallet is open in another btcw process";

// ── Harness ─────────────────────────────────────────────────────────────────────────────────

/// One wallet's own datadir, pointed at the test node, with the same isolated environment as
/// `cli.rs`.
struct Env {
    root: TempDir,
    rpc_url: String,
    rpc_cookie: PathBuf,
}

impl Env {
    fn new(node: &TestNode) -> TestResult<Self> {
        let rpc = node.rpc_config();
        let RpcAuth::Cookie(rpc_cookie) = rpc.auth else {
            return Err("TestNode uses cookie auth".into());
        };
        Ok(Self {
            root: tempfile::tempdir()?,
            rpc_url: rpc.url,
            rpc_cookie,
        })
    }

    fn datadir(&self) -> PathBuf {
        self.root.path().join("data")
    }

    fn network_dir(&self) -> PathBuf {
        self.datadir().join("regtest")
    }

    /// The configuration `btcw` resolves for this datadir, for driving the core API directly.
    fn config(&self) -> TestResult<Config> {
        Ok(Config::load_with(
            Overrides {
                network: Some("regtest".into()),
                datadir: Some(self.datadir()),
                rpc_url: Some(self.rpc_url.clone()),
                rpc_cookie: Some(self.rpc_cookie.clone()),
                ..Overrides::default()
            },
            |_| None,
        )?)
    }

    /// `btcw --network regtest --datadir .. --rpc-url <rpc_url> --rpc-cookie .. <args>`
    fn command_via(&self, rpc_url: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_btcw"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BTCW_") {
                cmd.env_remove(key);
            }
        }
        cmd.env_remove("RUST_LOG")
            .env("NO_COLOR", "1")
            .env("BTCW_PASSWORD", PASSWORD)
            .current_dir(self.root.path())
            .stdin(Stdio::null())
            .args(["--network", "regtest", "--datadir"])
            .arg(self.datadir())
            .args(["--rpc-url", rpc_url, "--rpc-cookie"])
            .arg(&self.rpc_cookie)
            .args(args);
        cmd
    }

    fn command(&self, args: &[&str]) -> Command {
        self.command_via(&self.rpc_url, args)
    }

    /// The same command inside a pseudo-terminal (`script`), so it can stop at a real
    /// `Send? [y/N]` prompt; what we write to its stdin is what the user types.
    fn command_in_pty(&self, args: &[&str]) -> Command {
        let inner = self.command(args);
        let line: Vec<String> = std::iter::once(inner.get_program())
            .chain(inner.get_args())
            .map(|arg| format!("'{}'", arg.to_string_lossy().replace('\'', r"'\''")))
            .collect();
        let mut cmd = Command::new("script");
        cmd.args([
            "--quiet",
            "--return",
            "--command",
            &line.join(" "),
            "/dev/null",
        ])
        .current_dir(self.root.path());
        for (key, value) in inner.get_envs() {
            match value {
                Some(value) => cmd.env(key, value),
                None => cmd.env_remove(key),
            };
        }
        cmd
    }

    fn run(&self, args: &[&str]) -> TestResult<Output> {
        Ok(self.command(args).output()?)
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> TestResult<Output> {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("no stdin pipe")?
            .write_all(stdin.as_bytes())?;
        Ok(child.wait_with_output()?)
    }

    /// `btcw --json <args>`: must succeed with exactly one JSON value on stdout.
    fn json(&self, args: &[&str]) -> TestResult<Value> {
        self.json_via(&self.rpc_url, args)
    }

    fn json_via(&self, rpc_url: &str, args: &[&str]) -> TestResult<Value> {
        ok_json(&self.command_via(rpc_url, &with_json(args)).output()?)
    }

    /// `btcw --json <args>`: must fail (exit 1) with a JSON error; returns `(code, message)`.
    fn json_error(&self, args: &[&str]) -> TestResult<(String, String)> {
        json_error(&self.run(&with_json(args))?)
    }

    /// `btcw --json create`: the result and the phrase (which goes to stderr, never stdout).
    fn create(&self) -> TestResult<(Value, String)> {
        let out = self.run(&["--json", "create"])?;
        let created = ok_json(&out)?;
        let phrase = phrase_from_grid(&stderr(&out))?;
        Ok((created, phrase))
    }

    /// `btcw --json send --yes`: `(txid, preview)`.
    fn send(&self, to: &str, amount_sat: u64, fee_rate: u64) -> TestResult<(String, Value)> {
        let sent = self.json(&[
            "send",
            "--yes",
            "--to",
            to,
            "--amount",
            &amount_sat.to_string(),
            "--fee-rate",
            &fee_rate.to_string(),
        ])?;
        let txid = sent["txid"].as_str().ok_or("send: no txid")?.to_owned();
        Ok((txid, sent["preview"].clone()))
    }
}

/// A spawned `btcw` that is killed (and reaped) if the test fails before it exits by itself.
struct Running {
    child: Option<Child>,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Running {
    /// Spawn with piped stdout/stderr, collected in the background so we can wait for a line.
    fn spawn(mut cmd: Command) -> TestResult<Self> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = collect(child.stdout.take().ok_or("no stdout pipe")?);
        let stderr = collect(child.stderr.take().ok_or("no stderr pipe")?);
        Ok(Self {
            child: Some(child),
            stdout,
            stderr,
        })
    }

    fn stdin(&mut self) -> TestResult<ChildStdin> {
        Ok(self
            .child
            .as_mut()
            .and_then(|c| c.stdin.take())
            .ok_or("no stdin pipe")?)
    }

    fn stdout_text(&self) -> String {
        text_of(&self.stdout)
    }

    fn stderr_text(&self) -> String {
        text_of(&self.stderr)
    }

    fn is_running(&mut self) -> TestResult<bool> {
        let child = self.child.as_mut().ok_or("already reaped")?;
        Ok(child.try_wait()?.is_none())
    }

    /// Wait until stdout contains `needle` (the process is still running).
    fn wait_for_stdout(&mut self, needle: &str, timeout: Duration) -> TestResult {
        let deadline = Instant::now() + timeout;
        while !self.stdout_text().contains(needle) {
            if Instant::now() > deadline || !self.is_running()? {
                return Err(format!(
                    "never printed {needle:?}\n--- stdout ---\n{}--- stderr ---\n{}",
                    self.stdout_text(),
                    self.stderr_text()
                )
                .into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    /// Wait for the exit; `(success, stdout, stderr)`.
    fn finish(mut self, timeout: Duration) -> TestResult<(bool, String, String)> {
        let deadline = Instant::now() + timeout;
        let mut child = self.child.take().ok_or("already reaped")?;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "did not exit within {timeout:?}\n--- stdout ---\n{}--- stderr ---\n{}",
                    self.stdout_text(),
                    self.stderr_text()
                )
                .into());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        // The readers stop at EOF; give them a moment to drain the pipes.
        std::thread::sleep(Duration::from_millis(100));
        Ok((status.success(), self.stdout_text(), self.stderr_text()))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn collect(mut pipe: impl Read + Send + 'static) -> Arc<Mutex<Vec<u8>>> {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buffer);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            if let Ok(mut buffer) = sink.lock() {
                buffer.extend_from_slice(&chunk[..n]);
            }
        }
    });
    buffer
}

fn text_of(buffer: &Mutex<Vec<u8>>) -> String {
    buffer
        .lock()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// A TCP proxy in front of bitcoind's RPC port that counts `getrawtransaction` requests, i.e.
/// what a sync downloaded.
struct CountingProxy {
    url: String,
    getrawtransaction: Arc<AtomicUsize>,
}

impl CountingProxy {
    const NEEDLE: &'static [u8] = b"\"getrawtransaction\"";

    fn start(upstream_url: &str) -> TestResult<Self> {
        let upstream = upstream_url
            .strip_prefix("http://")
            .ok_or("the test node's URL is not http://")?
            .trim_end_matches('/')
            .to_owned();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        // Accepts until the test process exits; each connection gets two pumps.
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let Ok(server) = TcpStream::connect(&upstream) else {
                    continue;
                };
                let (Ok(client_rx), Ok(server_tx)) = (client.try_clone(), server.try_clone())
                else {
                    continue;
                };
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || pump(client_rx, server_tx, Some(&counter)));
                std::thread::spawn(move || pump(server, client, None));
            }
        });
        Ok(Self {
            url,
            getrawtransaction: count,
        })
    }

    /// Requests seen since the last call.
    fn take(&self) -> usize {
        self.getrawtransaction.swap(0, Ordering::SeqCst)
    }
}

fn pump(mut from: TcpStream, mut to: TcpStream, counter: Option<&AtomicUsize>) {
    let needle = CountingProxy::NEEDLE;
    let mut chunk = [0u8; 16 * 1024];
    // The end of the previous chunk, so a name split across two reads is still seen (shorter
    // than the needle, so nothing is counted twice).
    let mut tail: Vec<u8> = Vec::new();
    loop {
        let n = match from.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if let Some(counter) = counter {
            let mut window = std::mem::take(&mut tail);
            window.extend_from_slice(&chunk[..n]);
            let hits = window
                .windows(needle.len())
                .filter(|w| *w == needle)
                .count();
            counter.fetch_add(hits, Ordering::SeqCst);
            let keep = window.len().min(needle.len() - 1);
            tail = window.split_off(window.len() - keep);
        }
        if to.write_all(&chunk[..n]).is_err() {
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

/// `TestNode::available()` runs `bitcoind -version`, which can collide with a parallel probe on
/// Core's settings file; retry so the tests only skip when bitcoind really is missing.
fn start_node() -> TestResult<Option<TestNode>> {
    if !(0..3).any(|_| TestNode::available()) {
        eprintln!("skipping: no bitcoind (set BITCOIND_EXE)");
        return Ok(None);
    }
    Ok(Some(TestNode::start()?))
}

fn with_json<'a>(args: &[&'a str]) -> Vec<&'a str> {
    std::iter::once("--json")
        .chain(args.iter().copied())
        .collect()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn describe(out: &Output) -> String {
    format!(
        "{}\n--- stdout ---\n{}--- stderr ---\n{}",
        out.status,
        stdout(out),
        stderr(out)
    )
}

fn ok_json(out: &Output) -> TestResult<Value> {
    if !out.status.success() {
        return Err(format!("expected success, got {}", describe(out)).into());
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("stdout is not one JSON value ({e}): {}", describe(out)).into())
}

fn json_error(out: &Output) -> TestResult<(String, String)> {
    if out.status.code() != Some(1) {
        return Err(format!("expected exit code 1, got {}", describe(out)).into());
    }
    let value: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("stdout is not one JSON value ({e}): {}", describe(out)))?;
    let code = value["error"]["code"].as_str().ok_or("no error.code")?;
    let message = value["error"]["message"]
        .as_str()
        .ok_or("no error.message")?;
    Ok((code.to_owned(), message.to_owned()))
}

/// The numbered grid `create` prints (` 1. word   2. word ...`), in order.
fn phrase_from_grid(text: &str) -> TestResult<String> {
    let mut numbered = Vec::new();
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        for pair in tokens.windows(2) {
            if let Some(n) = pair[0]
                .strip_suffix('.')
                .and_then(|n| n.parse::<usize>().ok())
            {
                numbered.push((n, pair[1].to_owned()));
            }
        }
    }
    numbered.sort();
    let positions: Vec<usize> = numbered.iter().map(|(n, _)| *n).collect();
    if numbered.is_empty() || positions != (1..=numbered.len()).collect::<Vec<_>>() {
        return Err(format!("no phrase grid (positions {positions:?}) in:\n{text}").into());
    }
    let words: Vec<String> = numbered.into_iter().map(|(_, w)| w).collect();
    Ok(words.join(" "))
}

/// BIP84 address `m/84'/1'/0'/<keychain>/<index>` for `phrase`, derived without btcw's wallet.
fn derived_address(phrase: &str, keychain: KeychainKind, index: u32) -> TestResult<String> {
    let mnemonic = keys::parse_mnemonic(phrase)?;
    let (descriptors, _signer) = keys::derive_account(&mnemonic, "", Network::Regtest, 0)?;
    let wallet = Wallet::create(
        descriptors.external().to_owned(),
        descriptors.internal().to_owned(),
    )
    .network(Network::Regtest)
    .create_wallet_no_persist()?;
    Ok(wallet.peek_address(keychain, index).address.to_string())
}

fn regtest_address(s: &str) -> TestResult<Address> {
    Ok(s.parse::<Address<NetworkUnchecked>>()?
        .require_network(Network::Regtest)?)
}

fn u64_of(value: &Value) -> TestResult<u64> {
    value
        .as_u64()
        .ok_or_else(|| format!("not an unsigned integer: {value}").into())
}

fn i64_of(value: &Value) -> TestResult<i64> {
    value
        .as_i64()
        .ok_or_else(|| format!("not an integer: {value}").into())
}

fn str_of(value: &Value) -> TestResult<&str> {
    value
        .as_str()
        .ok_or_else(|| format!("not a string: {value}").into())
}

fn signed(sat: u64) -> TestResult<i64> {
    Ok(i64::try_from(sat)?)
}

/// Core reports amounts in BTC with 8 decimals.
fn sat_of_btc(value: &Value) -> TestResult<u64> {
    let btc = value
        .as_f64()
        .ok_or_else(|| format!("not a BTC amount: {value}"))?;
    Ok(Amount::from_btc(btc)?.to_sat())
}

fn btc(sat: u64) -> Value {
    json!(Amount::from_sat(sat).to_btc())
}

/// The row for `txid` in `btcw --json history`.
fn row<'a>(history: &'a Value, txid: &str) -> TestResult<&'a Value> {
    history["transactions"]
        .as_array()
        .ok_or("history: no transactions array")?
        .iter()
        .find(|tx| tx["txid"] == txid)
        .ok_or_else(|| format!("{txid} is not in the history: {history}").into())
}

fn txids(history: &Value) -> TestResult<BTreeSet<String>> {
    history["transactions"]
        .as_array()
        .ok_or("history: no transactions array")?
        .iter()
        .map(|tx| Ok(str_of(&tx["txid"])?.to_owned()))
        .collect()
}

fn receive_addresses(list: &Value) -> TestResult<BTreeSet<String>> {
    list["addresses"]
        .as_array()
        .ok_or("address list: no addresses array")?
        .iter()
        .map(|a| Ok(str_of(&a["address"])?.to_owned()))
        .collect()
}

fn mempool(node: &TestNode) -> TestResult<BTreeSet<String>> {
    node.call("getrawmempool", &[])?
        .as_array()
        .ok_or("getrawmempool: not an array")?
        .iter()
        .map(|t| Ok(str_of(t)?.to_owned()))
        .collect()
}

/// Outputs of a mempool transaction as `(address, sat)`, from Core.
fn outputs(node: &TestNode, txid: &str) -> TestResult<Vec<(String, u64)>> {
    let tx = node.call("getrawtransaction", &[json!(txid), json!(true)])?;
    tx["vout"]
        .as_array()
        .ok_or("getrawtransaction: no vout")?
        .iter()
        .map(|out| {
            Ok((
                str_of(&out["scriptPubKey"]["address"])?.to_owned(),
                sat_of_btc(&out["value"])?,
            ))
        })
        .collect()
}

/// The one output of `txid` that doesn't pay `to`: the payer's change.
fn change_output(node: &TestNode, txid: &str, to: &str) -> TestResult<(String, u64)> {
    let others: Vec<(String, u64)> = outputs(node, txid)?
        .into_iter()
        .filter(|(address, _)| address != to)
        .collect();
    match others.as_slice() {
        [change] => Ok(change.clone()),
        _ => Err(format!("{txid}: expected exactly one change output, got {others:?}").into()),
    }
}

/// Spend one of the faucet's outpoints with a raw transaction (signals RBF). For building a
/// payment and a conflicting replacement from the same coin.
fn raw_spend(
    node: &TestNode,
    outpoint: &(String, u64),
    pay: &[(String, u64)],
) -> TestResult<String> {
    let outputs: serde_json::Map<String, Value> = pay
        .iter()
        .map(|(address, sat)| (address.clone(), btc(*sat)))
        .collect();
    let raw = node.call(
        "createrawtransaction",
        &[
            json!([{ "txid": outpoint.0, "vout": outpoint.1 }]),
            Value::Object(outputs),
            json!(0),
            json!(true),
        ],
    )?;
    let signed = node.call("signrawtransactionwithwallet", &[raw])?;
    if signed["complete"] != true {
        return Err(format!("signrawtransactionwithwallet: {signed}").into());
    }
    Ok(str_of(&node.call("sendrawtransaction", &[signed["hex"].clone()])?)?.to_owned())
}

/// A confirmed faucet coin of exactly `sat`, for `raw_spend`: `(txid, vout)`. Leaves it
/// unconfirmed; the caller mines.
fn faucet_coin(node: &TestNode, sat: u64) -> TestResult<(String, u64)> {
    let address = node.faucet_address()?;
    let txid = node.fund(&address, Amount::from_sat(sat))?.to_string();
    let tx = node.call("getrawtransaction", &[json!(txid), json!(true)])?;
    let vout = tx["vout"]
        .as_array()
        .ok_or("no vout")?
        .iter()
        .find(|out| out["scriptPubKey"]["address"] == address.to_string().as_str())
        .ok_or("the coin is not among the outputs")?;
    let coin = (txid, u64_of(&vout["n"])?);
    // Keep the faucet's own payments (`sendtoaddress`) off it.
    node.call(
        "lockunspent",
        &[json!(false), json!([{ "txid": coin.0, "vout": coin.1 }])],
    )?;
    Ok(coin)
}

/// Txids BDK stored in the wallet database (`bdk_txs`), read while no process has the wallet.
fn stored_txids(env: &Env) -> TestResult<BTreeSet<String>> {
    let db = Connection::open(env.network_dir().join("wallet.sqlite"))?;
    let mut query = db.prepare("SELECT txid FROM bdk_txs")?;
    let rows = query.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Txids in `<datadir>/regtest/mempool-cache.txt` (one consensus-hex transaction per line).
fn cached_txids(env: &Env) -> TestResult<BTreeSet<String>> {
    use btcw_core::bitcoin::Transaction;
    use btcw_core::bitcoin::consensus::encode::deserialize_hex;
    let text = std::fs::read_to_string(env.network_dir().join("mempool-cache.txt"))?;
    text.lines()
        .map(|line| {
            Ok(deserialize_hex::<Transaction>(line)?
                .compute_txid()
                .to_string())
        })
        .collect()
}

// ── Journey 1: one wallet, the whole money cycle, a restart and two restores ────────────────

#[test]
fn money_cycle_survives_restarts_and_restores_exactly() -> TestResult {
    let Some(node) = start_node()? else {
        return Ok(());
    };
    let env = Env::new(&node)?;
    let birthday = node.tip_height()?;
    const RECEIVED: u64 = 1_000_000;
    const SENT: u64 = 250_000;

    // R1: a new wallet, its birthday is the node's tip; the phrase only on stderr.
    let (created, phrase) = env.create()?;
    assert_eq!(u64_of(&created["birthday_height"])?, u64::from(birthday));
    assert_eq!(phrase.split(' ').count(), 12);
    let first = str_of(&created["first_address"])?.to_owned();
    // R3: receive address #0 is m/84'/1'/0'/0/0 of that phrase.
    assert_eq!(first, derived_address(&phrase, KeychainKind::External, 0)?);

    // The backup check, with the words on stdin; the reminder stops.
    let out = env.run(&["balance"])?;
    assert!(
        stderr(&out).contains("btcw backup verify"),
        "{}",
        describe(&out)
    );
    let out = env.run_with_stdin(&["--json", "backup", "verify"], &format!("{phrase}\n"))?;
    assert_eq!(ok_json(&out)?["backup_verified"], true);
    let out = env.run(&["balance"])?;
    assert!(
        !stderr(&out).contains("backup verify"),
        "{}",
        describe(&out)
    );

    // R4: the receive address, the same one until it is paid.
    let receive = env.json(&["address", "new"])?;
    assert_eq!(receive["index"], 0);
    assert_eq!(receive["address"], first.as_str());
    assert_eq!(env.json(&["address", "new"])?["address"], first.as_str());

    // The faucet pays it. R5: one sync sees it in the mempool.
    let funding = node
        .fund(&regtest_address(&first)?, Amount::from_sat(RECEIVED))?
        .to_string();
    let sync = env.json(&["sync"])?;
    assert_eq!(u64_of(&sync["tip_height"])?, u64::from(birthday));
    assert_eq!(sync["blocks_scanned"], 1, "only the birthday block");
    assert_eq!(sync["mempool_txs"], 1);
    // R6: unconfirmed money is shown apart from confirmed money.
    let balance = &env.json(&["balance"])?["balance"];
    assert_eq!(balance["confirmed_sat"], 0);
    assert_eq!(balance["unconfirmed_sat"], RECEIVED);
    // R7: the incoming payment, unconfirmed, no fee (its inputs aren't ours).
    let history = env.json(&["history"])?;
    let incoming = row(&history, &funding)?;
    assert_eq!(incoming["status"]["state"], "unconfirmed");
    assert_eq!(i64_of(&incoming["net_sat"])?, signed(RECEIVED)?);
    assert_eq!(incoming["fee_sat"], Value::Null);
    // R4: paid, so marked used.
    assert_eq!(
        env.json(&["address", "list"])?["addresses"][0]["used"],
        true
    );

    // Mined: confirmed, and the count goes up with each block (R5, R10).
    node.mine(1)?;
    let sync = env.json(&["sync"])?;
    assert_eq!(sync["blocks_scanned"], 1);
    assert_eq!(sync["mempool_txs"], 0);
    let status = &env.json(&["status", &funding])?["status"];
    assert_eq!(status["state"], "confirmed");
    assert_eq!(u64_of(&status["height"])?, u64::from(birthday) + 1);
    assert_eq!(status["confirmations"], 1);
    node.mine(1)?;
    env.json(&["sync"])?;
    assert_eq!(
        env.json(&["history"])?["transactions"][0]["status"]["confirmations"],
        2
    );
    let balance = &env.json(&["balance"])?["balance"];
    assert_eq!(balance["confirmed_sat"], RECEIVED);
    assert_eq!(balance["unconfirmed_sat"], 0);

    // R8 + R9: pay the faucet; the PSBT is built, signed and broadcast by one command.
    let faucet = node.faucet_address()?.to_string();
    let (spend, preview) = env.send(&faucet, SENT, 3)?;
    let fee = u64_of(&preview["fee_sat"])?;
    let change = u64_of(&preview["change_sat"])?;
    assert_eq!(change, RECEIVED - SENT - fee);
    assert_eq!(u64_of(&preview["total_sat"])?, SENT + fee);
    // Core agrees on the fee and on where every satoshi goes.
    assert!(mempool(&node)?.contains(&spend));
    let entry = node.call("getmempoolentry", &[json!(spend)])?;
    assert_eq!(sat_of_btc(&entry["fees"]["base"])?, fee);
    let (change_address, change_value) = change_output(&node, &spend, &faucet)?;
    assert_eq!(change_value, change);
    let change_vout = vout_of(&node, &spend, &change_address)?;
    assert!(outputs(&node, &spend)?.contains(&(faucet.clone(), SENT)));
    // R3: change goes to m/84'/1'/0'/1/0, the first internal address.
    assert_eq!(
        change_address,
        derived_address(&phrase, KeychainKind::Internal, 0)?
    );

    // R10: unconfirmed right after the send, confirmed with the right count after 2 blocks.
    assert_eq!(
        env.json(&["status", &spend])?["status"]["state"],
        "unconfirmed"
    );
    node.mine(2)?;
    let watched = env.json(&[
        "status",
        &spend,
        "--watch",
        "--until",
        "2",
        "--interval",
        "1",
    ])?;
    let spend_height = birthday + 3;
    assert_eq!(watched["status"]["state"], "confirmed");
    assert_eq!(
        u64_of(&watched["status"]["height"])?,
        u64::from(spend_height)
    );
    assert_eq!(watched["status"]["confirmations"], 2);
    let tip = birthday + 4;
    assert_eq!(u64_of(&watched["synced_height"])?, u64::from(tip));

    // R7: both transactions, newest first, with exact net amounts and the fee.
    let history = env.json(&["history"])?;
    let rows = history["transactions"].as_array().ok_or("no rows")?;
    assert_eq!(rows.len(), 2, "{history}");
    assert_eq!(rows[0]["txid"], spend.as_str());
    assert_eq!(rows[1]["txid"], funding.as_str());
    let outgoing = &rows[0];
    assert_eq!(i64_of(&outgoing["net_sat"])?, -signed(SENT + fee)?);
    assert_eq!(u64_of(&outgoing["fee_sat"])?, fee);
    assert_eq!(u64_of(&outgoing["sent_sat"])?, RECEIVED);
    assert_eq!(u64_of(&outgoing["received_sat"])?, change);
    assert_eq!(outgoing["status"]["confirmations"], 2);
    let incoming = &rows[1];
    assert_eq!(i64_of(&incoming["net_sat"])?, signed(RECEIVED)?);
    assert_eq!(
        u64_of(&incoming["status"]["confirmations"])?,
        u64::from(tip - (birthday + 1) + 1)
    );

    // R6: received − sent − fee, to the satoshi.
    let balance = env.json(&["balance"])?;
    assert_eq!(
        balance["balance"],
        json!({
            "confirmed_sat": RECEIVED - SENT - fee,
            "unconfirmed_sat": 0,
            "immature_sat": 0,
            "total_sat": RECEIVED - SENT - fee,
        })
    );
    // The only coin left is the change, on the internal keychain, never shown as a receive
    // address (R4).
    let utxos = env.json(&["utxos"])?;
    assert_eq!(
        utxos["utxos"],
        json!([{
            "outpoint": format!("{spend}:{change_vout}"),
            "value_sat": change,
            "address": change_address,
            "keychain": "internal",
            "derivation_index": 0,
            "confirmed": true,
        }])
    );
    let addresses = env.json(&["address", "list"])?;
    assert_eq!(
        receive_addresses(&addresses)?,
        BTreeSet::from([first.clone()])
    );

    // ── R11: every command is a fresh process; the state is the database's, not the node's.
    let views: [&[&str]; 4] = [&["balance"], &["history"], &["utxos"], &["address", "list"]];
    let snapshot: Vec<Value> = views
        .iter()
        .map(|view| env.json(view))
        .collect::<TestResult<_>>()?;
    for (view, expected) in views.iter().zip(&snapshot) {
        assert_eq!(&env.json(view)?, expected, "{view:?} changed between runs");
        // With bitcoind unreachable: the same answer, from SQLite alone.
        assert_eq!(
            &env.json_via(CLOSED_RPC_URL, view)?,
            expected,
            "{view:?} offline"
        );
    }
    let offline = env.json_via(CLOSED_RPC_URL, &["status", &spend])?;
    assert_eq!(offline["status"], watched["status"]);
    assert_eq!(offline["sync_error"]["code"], "rpc");

    // ── R2: restore the phrase in a new datadir, scanning from a height before the payment.
    let restored = Env::new(&node)?;
    let out = restored.run_with_stdin(
        &["--json", "restore", "--birthday", &birthday.to_string()],
        &format!("{phrase}\n"),
    )?;
    let report = ok_json(&out)?;
    assert_eq!(u64_of(&report["birthday_height"])?, u64::from(birthday));
    assert_eq!(report["sync_error"], Value::Null);
    assert_eq!(
        u64_of(&report["sync"]["blocks_scanned"])?,
        u64::from(tip - birthday + 1)
    );
    assert_eq!(report["balance"], snapshot[0]["balance"]);
    // Identical balance, history (txids, net, fee, status), coins and used flags.
    for (view, expected) in views.iter().zip(&snapshot) {
        assert_eq!(&restored.json(view)?, expected, "restored {view:?}");
    }
    // A restored wallet counts as backed up (the user just typed the phrase).
    assert!(!stderr(&restored.run(&["balance"])?).contains("backup verify"));

    // Without a birthday: a scan from genesis (fast on regtest) finds the same.
    let from_genesis = Env::new(&node)?;
    let out = from_genesis.run_with_stdin(&["--json", "restore"], &format!("{phrase}\n"))?;
    let report = ok_json(&out)?;
    assert_eq!(report["birthday_height"], 0);
    assert_eq!(u64_of(&report["sync"]["blocks_scanned"])?, u64::from(tip));
    for (view, expected) in views.iter().zip(&snapshot) {
        assert_eq!(
            &from_genesis.json(view)?,
            expected,
            "restored from genesis {view:?}"
        );
    }

    // R4 after a restore: #0 is used, so all three wallets hand out #1 next, the same address.
    let next = env.json(&["address", "new"])?;
    assert_eq!(next["index"], 1);
    assert_eq!(restored.json(&["address", "new"])?, next);
    assert_eq!(from_genesis.json(&["address", "new"])?, next);
    Ok(())
}

fn vout_of(node: &TestNode, txid: &str, address: &str) -> TestResult<usize> {
    outputs(node, txid)?
        .iter()
        .position(|(a, _)| a == address)
        .ok_or_else(|| format!("{address} is not an output of {txid}").into())
}

// ── Journey 2: two wallets, two datadirs, paying each other; and the lock between processes ─

#[test]
fn two_wallets_pay_each_other_and_lock_out_a_second_process() -> TestResult {
    let Some(node) = start_node()? else {
        return Ok(());
    };
    let alice = Env::new(&node)?;
    let bob = Env::new(&node)?;
    alice.create()?;
    bob.create()?;
    const FUNDED: u64 = 2_000_000;

    let a0 = str_of(&alice.json(&["address", "new"])?["address"])?.to_owned();
    let funding = node
        .fund(&regtest_address(&a0)?, Amount::from_sat(FUNDED))?
        .to_string();
    node.mine(1)?;
    alice.json(&["sync"])?;
    assert_eq!(
        alice.json(&["balance"])?["balance"]["confirmed_sat"],
        FUNDED
    );

    // Alice pays Bob; Bob sees it in the mempool, then confirmed (R5, R6, R7, R10).
    let b0 = str_of(&bob.json(&["address", "new"])?["address"])?.to_owned();
    let (t1, p1) = alice.send(&b0, 600_000, 2)?;
    let fee1 = u64_of(&p1["fee_sat"])?;
    let (c1, _) = change_output(&node, &t1, &b0)?;
    let sync = bob.json(&["sync"])?;
    assert_eq!(sync["mempool_txs"], 1);
    let balance = &bob.json(&["balance"])?["balance"];
    assert_eq!(balance["confirmed_sat"], 0);
    assert_eq!(balance["unconfirmed_sat"], 600_000);
    let history = bob.json(&["history"])?;
    assert_eq!(row(&history, &t1)?["status"]["state"], "unconfirmed");
    assert_eq!(
        bob.json(&["status", &t1])?["status"]["state"],
        "unconfirmed"
    );
    node.mine(1)?;
    bob.json(&["sync"])?;
    let balance = &bob.json(&["balance"])?["balance"];
    assert_eq!(balance["confirmed_sat"], 600_000);
    assert_eq!(balance["unconfirmed_sat"], 0);
    assert_eq!(bob.json(&["status", &t1])?["status"]["confirmations"], 1);

    // R4: #0 is used now, so Bob hands out #1. Alice pays again, from her change.
    let b1 = bob.json(&["address", "new"])?;
    assert_eq!(b1["index"], 1);
    let b1 = str_of(&b1["address"])?.to_owned();
    assert_ne!(b1, b0);
    let (t2, p2) = alice.send(&b1, 300_000, 2)?;
    let fee2 = u64_of(&p2["fee_sat"])?;
    let (c2, _) = change_output(&node, &t2, &b1)?;
    node.mine(1)?;

    // Bob pays some back to Alice's next address.
    let a1 = alice.json(&["address", "new"])?;
    assert_eq!(a1["index"], 1);
    let a1 = str_of(&a1["address"])?.to_owned();
    let (t3, p3) = bob.send(&a1, 200_000, 2)?;
    let fee3 = u64_of(&p3["fee_sat"])?;
    let (c3, _) = change_output(&node, &t3, &a1)?;
    node.mine(1)?;
    alice.json(&["sync"])?;
    bob.json(&["sync"])?;

    // Both histories agree on every transaction between them: same txid, same block, and the
    // two nets add up to minus the payer's fee (R7).
    let alice_history = alice.json(&["history"])?;
    let bob_history = bob.json(&["history"])?;
    assert_eq!(
        txids(&alice_history)?,
        BTreeSet::from([funding.clone(), t1.clone(), t2.clone(), t3.clone()])
    );
    assert_eq!(
        txids(&bob_history)?,
        BTreeSet::from([t1.clone(), t2.clone(), t3.clone()])
    );
    // The payee knows a fee only if it holds the transactions the inputs came from: t2 spends
    // Alice's change from t1, which Bob has; t3 spends Bob's coins from t1 and t2.
    for (txid, amount, fee, alice_paid, payee_knows_fee) in [
        (&t1, 600_000, fee1, true, false),
        (&t2, 300_000, fee2, true, true),
        (&t3, 200_000, fee3, false, true),
    ] {
        let (a, b) = (row(&alice_history, txid)?, row(&bob_history, txid)?);
        assert_eq!(a["status"], b["status"], "{txid}");
        assert_eq!(a["status"]["state"], "confirmed");
        let (payer, payee) = if alice_paid { (a, b) } else { (b, a) };
        assert_eq!(i64_of(&payer["net_sat"])?, -signed(amount + fee)?, "{txid}");
        assert_eq!(u64_of(&payer["fee_sat"])?, fee, "{txid}");
        assert_eq!(i64_of(&payee["net_sat"])?, signed(amount)?, "{txid}");
        let expected_fee = if payee_knows_fee {
            json!(fee)
        } else {
            Value::Null
        };
        assert_eq!(payee["fee_sat"], expected_fee, "{txid}");
    }

    // R6: exact balances, and each equals the sum of its coins.
    let alice_total = FUNDED - 600_000 - fee1 - 300_000 - fee2 + 200_000;
    let bob_total = 600_000 + 300_000 - 200_000 - fee3;
    for (env, total) in [(&alice, alice_total), (&bob, bob_total)] {
        let balance = &env.json(&["balance"])?["balance"];
        assert_eq!(u64_of(&balance["confirmed_sat"])?, total);
        assert_eq!(u64_of(&balance["total_sat"])?, total);
        let coins: u64 = env.json(&["utxos"])?["utxos"]
            .as_array()
            .ok_or("no utxos")?
            .iter()
            .map(|u| u64_of(&u["value_sat"]))
            .sum::<TestResult<u64>>()?;
        assert_eq!(coins, total);
    }

    // Change never reuses an address: every change output is new, internal, and not any
    // receive address of either wallet.
    let receive: BTreeSet<String> = receive_addresses(&alice.json(&["address", "list"])?)?
        .union(&receive_addresses(&bob.json(&["address", "list"])?)?)
        .cloned()
        .collect();
    let changes = BTreeSet::from([c1.clone(), c2.clone(), c3.clone()]);
    assert_eq!(
        changes.len(),
        3,
        "a change address was used twice: {changes:?}"
    );
    assert!(changes.is_disjoint(&receive), "{changes:?} vs {receive:?}");
    // Alice's second payment spent her first change (c1) and sent the rest to a new index.
    let alice_utxos = alice.json(&["utxos"])?;
    let alice_coins: BTreeSet<(String, String, u64)> = alice_utxos["utxos"]
        .as_array()
        .ok_or("no utxos")?
        .iter()
        .map(|u| {
            Ok((
                str_of(&u["address"])?.to_owned(),
                str_of(&u["keychain"])?.to_owned(),
                u64_of(&u["derivation_index"])?,
            ))
        })
        .collect::<TestResult<_>>()?;
    assert_eq!(
        alice_coins,
        BTreeSet::from([(c2, "internal".into(), 1), (a1, "external".into(), 1)])
    );

    // ── Locking: one process at a time per wallet, and a clean error otherwise.
    // 1. A `btcw send` stopped at its `Send? [y/N]` prompt holds Bob's wallet.
    let faucet = node.faucet_address()?.to_string();
    if has_program("script") {
        let mut asking =
            Running::spawn(bob.command_in_pty(&["send", "--to", &faucet, "--amount", "10000"]))?;
        asking.wait_for_stdout("Send? [y/N]", Duration::from_secs(60))?;
        let (code, message) = bob.json_error(&["balance"])?;
        assert_eq!(
            (code.as_str(), message.as_str()),
            ("wallet_in_use", WALLET_IN_USE)
        );
        assert!(matches!(
            api::open_watch_only(&bob.config()?),
            Err(WalletError::WalletInUse)
        ));
        // Alice's wallet is another datadir, another lock.
        alice.json(&["balance"])?;
        asking.stdin()?.write_all(b"n\n")?;
        let (ok, out, err) = asking.finish(Duration::from_secs(30))?;
        assert!(ok, "{out}\n{err}");
        assert!(out.contains("Cancelled; nothing was sent."), "{out}");
    } else {
        eprintln!("skipping the held-at-the-prompt check: no `script`");
    }

    // 2. This test process holds it through the core API, as the desktop app does per command.
    let held = api::open_watch_only(&bob.config()?)?;
    let refused: [&[&str]; 5] = [
        &["balance"],
        &["history"],
        &["address", "new"],
        &["sync"],
        &["send", "--yes", "--to", &faucet, "--amount", "10000"],
    ];
    for args in refused {
        let (code, message) = bob.json_error(args)?;
        assert_eq!(code, "wallet_in_use", "{args:?}: {message}");
        assert_eq!(message, WALLET_IN_USE);
    }
    let out = bob.run(&["balance"])?;
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stderr(&out), format!("error: {WALLET_IN_USE}\n"));
    // `status --watch` waits for its turn instead of giving up, and finishes once it's free.
    let mut watcher = Running::spawn(bob.command(&[
        "status",
        &t3,
        "--watch",
        "--until",
        "1",
        "--interval",
        "1",
    ]))?;
    std::thread::sleep(Duration::from_millis(2_500));
    assert!(watcher.is_running()?, "{}", watcher.stderr_text());
    drop(held);
    let (ok, out, err) = watcher.finish(Duration::from_secs(30))?;
    assert!(ok, "{out}\n{err}");
    assert!(
        err.contains(&format!("{WALLET_IN_USE}; trying again in 1 s")),
        "{err}"
    );
    assert!(out.trim_end().ends_with("1 confirmation"), "{out}");
    // Free again: nothing was lost or changed while it was locked.
    assert_eq!(
        u64_of(&bob.json(&["balance"])?["balance"]["total_sat"])?,
        bob_total
    );
    Ok(())
}

fn has_program(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

// ── Journey 3: reorgs seen through the CLI, then the mempool cache ──────────────────────────

#[test]
fn reorgs_and_the_mempool_cache_through_the_cli() -> TestResult {
    let Some(node) = start_node()? else {
        return Ok(());
    };

    // ── A birthday above the node's tip (reorged away, or a restore height not reached yet):
    // sync must succeed and wait, not fail (R5).
    let early = Env::new(&node)?;
    let (created, phrase) = early.create()?;
    let born = node.tip_height()?;
    assert_eq!(u64_of(&created["birthday_height"])?, u64::from(born));
    let tip_hash = str_of(&node.call("getblockhash", &[json!(born)])?)?.to_owned();
    node.call("invalidateblock", &[json!(tip_hash)])?;
    let sync = early.json(&["sync"])?;
    assert_eq!(sync["blocks_scanned"], 0, "{sync}");
    let future = Env::new(&node)?;
    let out = future.run_with_stdin(
        &["--json", "restore", "--birthday", &(born + 1).to_string()],
        &format!("{phrase}\n"),
    )?;
    let restored = ok_json(&out)?;
    assert_eq!(restored["sync_error"], Value::Null, "{restored}");
    assert_eq!(restored["sync"]["blocks_scanned"], 0);
    // The chain grows back to the first wallet's birthday, then past the second's.
    let regrown = node.mine(1)?[0].to_string();
    assert_eq!(early.json(&["sync"])?["blocks_scanned"], 1);
    assert_eq!(future.json(&["sync"])?["blocks_scanned"], 0);
    // Once more, now that the first wallet has scanned its birthday block: that block goes
    // away too, and comes back as a different one.
    node.call("invalidateblock", &[json!(regrown)])?;
    let out = early.run(&["--json", "sync"])?;
    ok_json(&out)?;
    assert!(
        stderr(&out).contains("shorter than the wallet's"),
        "{}",
        describe(&out)
    );
    node.mine(1)?;
    assert_eq!(early.json(&["sync"])?["blocks_scanned"], 1);
    // While it waits, the mempool is still watched.
    let early_address = str_of(&early.json(&["address", "new"])?["address"])?.to_owned();
    let early_funding = node
        .fund(&regtest_address(&early_address)?, Amount::from_sat(70_000))?
        .to_string();
    assert_eq!(future.json(&["sync"])?["mempool_txs"], 1);
    node.mine(1)?;
    for wallet in [&early, &future] {
        assert_eq!(wallet.json(&["sync"])?["blocks_scanned"], 1);
        let status = &wallet.json(&["status", &early_funding])?["status"];
        assert_eq!(u64_of(&status["height"])?, u64::from(born) + 1);
        assert_eq!(
            wallet.json(&["balance"])?["balance"]["confirmed_sat"],
            70_000
        );
    }

    let env = Env::new(&node)?;
    env.create()?;
    const RECEIVED: u64 = 1_000_000;
    let address = str_of(&env.json(&["address", "new"])?["address"])?.to_owned();
    let funding = node
        .fund(&regtest_address(&address)?, Amount::from_sat(RECEIVED))?
        .to_string();
    let block = node.mine(1)?[0].to_string();
    let height = node.tip_height()?;
    env.json(&["sync"])?;
    assert_eq!(
        env.json(&["status", &funding])?["status"]["confirmations"],
        1
    );

    // The block with our payment is reorged out. Until a competing block exists, the chain is
    // just shorter and the wallet says so (a known limitation).
    node.call("invalidateblock", &[json!(block)])?;
    assert!(mempool(&node)?.contains(&funding), "Core put it back");
    let out = env.run(&["sync"])?;
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        stderr(&out).contains("the node's chain is shorter than the wallet's"),
        "{}",
        describe(&out)
    );
    // A competing, empty block at the same height: R5 follows the reorg, R10/R6 un-confirm.
    let faucet = node.faucet_address()?.to_string();
    node.call("generateblock", &[json!(faucet), json!([])])?;
    let sync = env.json(&["sync"])?;
    assert_eq!(sync["blocks_scanned"], 1);
    assert_eq!(sync["mempool_txs"], 1);
    assert_eq!(
        env.json(&["status", &funding])?["status"]["state"],
        "unconfirmed"
    );
    let balance = &env.json(&["balance"])?["balance"];
    assert_eq!(balance["confirmed_sat"], 0);
    assert_eq!(balance["unconfirmed_sat"], RECEIVED);
    assert_eq!(
        env.json(&["history"])?["transactions"][0]["status"]["state"],
        "unconfirmed"
    );
    // Mined again, one block higher.
    node.mine(1)?;
    env.json(&["sync"])?;
    let status = &env.json(&["status", &funding])?["status"];
    assert_eq!(status["state"], "confirmed");
    assert_eq!(u64_of(&status["height"])?, u64::from(height) + 1);
    assert_eq!(status["confirmations"], 1);
    assert_eq!(
        env.json(&["balance"])?["balance"]["confirmed_sat"],
        RECEIVED
    );

    // Our own payment reorged out: the spent coin stays spent, the change is unconfirmed.
    let (spend, preview) = env.send(&faucet, 300_000, 2)?;
    let change = u64_of(&preview["change_sat"])?;
    let block = node.mine(1)?[0].to_string();
    env.json(&["sync"])?;
    assert_eq!(env.json(&["status", &spend])?["status"]["confirmations"], 1);
    node.call("invalidateblock", &[json!(block)])?;
    node.call("generateblock", &[json!(faucet), json!([])])?;
    env.json(&["sync"])?;
    assert_eq!(
        env.json(&["status", &spend])?["status"]["state"],
        "unconfirmed"
    );
    let balance = &env.json(&["balance"])?["balance"];
    assert_eq!(balance["confirmed_sat"], 0);
    assert_eq!(balance["unconfirmed_sat"], change);
    assert_eq!(
        env.json(&["status", &funding])?["status"]["state"],
        "confirmed"
    );
    node.mine(1)?;
    let watched = env.json(&[
        "status",
        &spend,
        "--watch",
        "--until",
        "1",
        "--interval",
        "1",
    ])?;
    assert_eq!(watched["status"]["confirmations"], 1);
    assert_eq!(env.json(&["balance"])?["balance"]["confirmed_sat"], change);
    let before_cache = change;

    // ── The mempool cache: a sync downloads each mempool transaction once.
    // Two faucet coins we control exactly, so we can later replace what spends them.
    let for_wallet = faucet_coin(&node, 5_000_000)?;
    let for_stranger = faucet_coin(&node, 5_000_000)?;
    node.mine(1)?;
    let proxy = CountingProxy::start(&env.rpc_url)?;
    env.json_via(&proxy.url, &["sync"])?;
    assert_eq!(proxy.take(), 0, "empty mempool, nothing to fetch");
    assert_eq!(cached_txids(&env)?, BTreeSet::new());

    // Four transactions between strangers and one payment to the wallet. The strangers' come
    // first, so none of them can spend an output of the two we replace later.
    let mut strangers = BTreeSet::new();
    for _ in 0..3 {
        let to = node.faucet_address()?;
        strangers.insert(node.fund(&to, Amount::from_sat(10_000))?.to_string());
    }
    let other = node.faucet_address()?.to_string();
    let incoming = raw_spend(
        &node,
        &for_wallet,
        &[(address_of_next(&env)?, 50_000), (other.clone(), 4_940_000)],
    )?;
    let replaced = raw_spend(&node, &for_stranger, &[(other.clone(), 4_990_000)])?;
    strangers.insert(replaced.clone());
    let in_mempool = mempool(&node)?;
    assert_eq!(in_mempool.len(), 5);

    let started = Instant::now();
    let sync = env.json_via(&proxy.url, &["sync"])?;
    let first_sync = started.elapsed();
    assert_eq!(sync["mempool_txs"], 1);
    assert_eq!(proxy.take(), 5, "every mempool transaction is new");
    assert_eq!(cached_txids(&env)?, in_mempool);

    // No new blocks, no new transactions: nothing is downloaded again.
    let started = Instant::now();
    let sync = env.json_via(&proxy.url, &["sync"])?;
    let second_sync = started.elapsed();
    assert_eq!(sync["mempool_txs"], 1);
    assert_eq!(proxy.take(), 0, "the cache already holds all five");
    assert_eq!(cached_txids(&env)?, in_mempool);
    eprintln!("sync with 5 new mempool txs: {first_sync:?}; with all 5 cached: {second_sync:?}");

    // Strangers' transactions never reach the wallet's history or its database.
    let history = env.json(&["history"])?;
    assert!(txids(&history)?.contains(&incoming));
    assert!(txids(&history)?.is_disjoint(&strangers), "{history}");
    assert_eq!(
        env.json(&["balance"])?["balance"]["unconfirmed_sat"],
        50_000
    );

    // Both coins are double-spent with higher fees: a stranger's transaction is replaced, and
    // so is the payment to the wallet (its sender took the money back).
    let replacement = raw_spend(&node, &for_stranger, &[(other.clone(), 4_980_000)])?;
    let clawback = raw_spend(&node, &for_wallet, &[(other, 4_980_000)])?;
    let in_mempool = mempool(&node)?;
    assert!(!in_mempool.contains(&replaced) && !in_mempool.contains(&incoming));
    let sync = env.json_via(&proxy.url, &["sync"])?;
    assert_eq!(proxy.take(), 2, "only the two replacements are new");
    assert_eq!(sync["mempool_txs"], 0);
    assert_eq!(cached_txids(&env)?, in_mempool);

    // The evicted payment is gone from history, balance and status; no stranger ever appears.
    let history = env.json(&["history"])?;
    let seen = txids(&history)?;
    assert!(!seen.contains(&incoming), "{history}");
    let clawback_txid = clawback.clone();
    strangers.extend([replacement, clawback]);
    assert!(seen.is_disjoint(&strangers), "{history}");
    let funding_txid = funding.clone();
    assert_eq!(seen, BTreeSet::from([funding, spend.clone()]));
    let balance = &env.json(&["balance"])?["balance"];
    assert_eq!(u64_of(&balance["total_sat"])?, before_cache);
    assert_eq!(balance["unconfirmed_sat"], 0);
    let (code, _) = env.json_error(&["status", &incoming])?;
    assert_eq!(code, "tx_not_found");
    // Stored: the wallet's own transactions, plus `clawback`, which BDK keeps (never shown)
    // because it conflicts with one of ours. Other strangers are not stored.
    assert_eq!(
        stored_txids(&env)?,
        BTreeSet::from([funding_txid, spend, incoming, clawback_txid])
    );
    Ok(())
}

/// The wallet's current receive address (#1 here: #0 was paid by the funding transaction).
fn address_of_next(env: &Env) -> TestResult<String> {
    Ok(str_of(&env.json(&["address", "new"])?["address"])?.to_owned())
}
