//! End-to-end tests of the `btcw` binary: spawn it, read stdout/stderr and the exit code.
//!
//! Every run gets a temp `--datadir`, `--network regtest`, `NO_COLOR=1`, `BTCW_PASSWORD`, a cwd
//! inside the temp dir, and *no* inherited `BTCW_*` variables, so the developer's own setup
//! (e.g. `eval "$(scripts/regtest.sh env)"`) can't leak in.
//!
//! "Offline" tests point `--rpc-url` at port 1, where nothing listens, so every node call is
//! refused at once. The one node test needs `bitcoind` (`BITCOIND_EXE` or `PATH`) and is
//! skipped, not failed, without it.

use std::collections::BTreeSet;
use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use btcw_core::bdk_wallet::{KeychainKind, Wallet};
use btcw_core::bitcoin::address::NetworkUnchecked;
use btcw_core::bitcoin::{Address, Amount, Network};
use btcw_core::config::RpcAuth;
use btcw_core::keys;
use btcw_core::testnode::TestNode;
use serde_json::Value;
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const PASSWORD: &str = "correct horse battery";
/// The BIP84 test-vector phrase.
const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
/// Nothing listens on port 1 (tcpmux), so connecting is refused immediately.
const CLOSED_RPC_URL: &str = "http://127.0.0.1:1";

/// One isolated btcw setup: its own data directory and node settings.
struct Env {
    root: TempDir,
    rpc_url: String,
    rpc_cookie: PathBuf,
    password: String,
}

impl Env {
    /// No node: a well-formed cookie file, but nothing listening at the RPC URL.
    fn offline() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let rpc_cookie = root.path().join("fake.cookie");
        std::fs::write(&rpc_cookie, "__cookie__:no-node-here")?;
        Ok(Self {
            root,
            rpc_url: CLOSED_RPC_URL.to_owned(),
            rpc_cookie,
            password: PASSWORD.to_owned(),
        })
    }

    fn with_node(node: &TestNode) -> TestResult<Self> {
        let rpc = node.rpc_config();
        let RpcAuth::Cookie(rpc_cookie) = rpc.auth else {
            return Err("TestNode uses cookie auth".into());
        };
        Ok(Self {
            root: tempfile::tempdir()?,
            rpc_url: rpc.url,
            rpc_cookie,
            password: PASSWORD.to_owned(),
        })
    }

    fn datadir(&self) -> PathBuf {
        self.root.path().join("data")
    }

    fn network_dir(&self) -> PathBuf {
        self.datadir().join("regtest")
    }

    /// `btcw --network <network> --datadir .. --rpc-url .. --rpc-cookie .. <args>`
    fn command_on(&self, network: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_btcw"));
        self.configure(&mut cmd, network, args);
        cmd
    }

    /// The same on regtest, but in a new session with no controlling terminal (`setsid`), so
    /// `/dev/tty` can't be opened: like running under cron or CI.
    fn command_without_terminal(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("setsid");
        cmd.arg("--wait").arg(env!("CARGO_BIN_EXE_btcw"));
        self.configure(&mut cmd, "regtest", args);
        cmd
    }

    fn configure(&self, cmd: &mut Command, network: &str, args: &[&str]) {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BTCW_") {
                cmd.env_remove(key);
            }
        }
        cmd.env_remove("RUST_LOG")
            .env("NO_COLOR", "1")
            .env("BTCW_PASSWORD", &self.password)
            .current_dir(self.root.path())
            .stdin(Stdio::null())
            .args(["--network", network])
            .arg("--datadir")
            .arg(self.datadir())
            .args(["--rpc-url", &self.rpc_url])
            .arg("--rpc-cookie")
            .arg(&self.rpc_cookie)
            .args(args);
    }

    fn run(&self, args: &[&str]) -> TestResult<Output> {
        Ok(self.command_on("regtest", args).output()?)
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> TestResult<Output> {
        let mut child = self
            .command_on("regtest", args)
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

    /// `btcw --json <args>`: must succeed and print exactly one JSON value.
    fn json(&self, args: &[&str]) -> TestResult<Value> {
        ok_json(&self.run(&with_json(args))?)
    }

    /// `btcw --json <args>`: must fail with exit code 1 and a JSON error; `(code, message)`.
    fn json_error(&self, args: &[&str]) -> TestResult<(String, String)> {
        json_error(&self.run(&with_json(args))?)
    }
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
    // `from_slice` rejects anything after the first value, so this also checks "exactly one".
    serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("stdout is not one JSON value ({e}): {}", describe(out)).into())
}

fn json_error(out: &Output) -> TestResult<(String, String)> {
    if out.status.code() != Some(1) {
        return Err(format!("expected exit code 1, got {}", describe(out)).into());
    }
    let value: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("stdout is not one JSON value ({e}): {}", describe(out)))?;
    let object = value.as_object().ok_or("error output is not an object")?;
    assert_eq!(object.len(), 1, "only an `error` key: {value}");
    let code = value["error"]["code"].as_str().ok_or("no error.code")?;
    let message = value["error"]["message"]
        .as_str()
        .ok_or("no error.message")?;
    Ok((code.to_owned(), message.to_owned()))
}

fn keys_of(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
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
    if positions != (1..=numbered.len()).collect::<Vec<_>>() {
        return Err(format!("words are not numbered 1..n: {positions:?}").into());
    }
    let words: Vec<String> = numbered.into_iter().map(|(_, w)| w).collect();
    Ok(words.join(" "))
}

/// BIP84 receive address #0 for `phrase` on regtest, derived without the CLI: the core's key
/// derivation fed into a plain, in-memory BDK wallet.
fn expected_first_address(phrase: &str) -> TestResult<String> {
    let mnemonic = keys::parse_mnemonic(phrase)?;
    let (descriptors, _signer) = keys::derive_account(&mnemonic, "", Network::Regtest, 0)?;
    let wallet = Wallet::create(
        descriptors.external().to_owned(),
        descriptors.internal().to_owned(),
    )
    .network(Network::Regtest)
    .create_wallet_no_persist()?;
    Ok(wallet
        .peek_address(KeychainKind::External, 0)
        .address
        .to_string())
}

fn as_u64(value: &Value) -> TestResult<u64> {
    value
        .as_u64()
        .ok_or_else(|| format!("not an unsigned integer: {value}").into())
}

#[test]
fn help_lists_the_commands_and_the_password_variable() -> TestResult {
    let env = Env::offline()?;
    let out = env.command_on("regtest", &["--help"]).output()?;
    assert!(out.status.success(), "{}", describe(&out));
    let help = stdout(&out);
    for command in [
        "create", "restore", "address", "sync", "balance", "history", "utxos", "send", "status",
        "mine",
    ] {
        assert!(help.contains(command), "`{command}` missing from:\n{help}");
    }
    assert!(help.contains("BTCW_PASSWORD"), "{help}");
    assert!(help.contains("INSECURE"), "{help}");

    let out = env.command_on("regtest", &["address", "--help"]).output()?;
    assert!(out.status.success(), "{}", describe(&out));
    assert!(stdout(&out).contains("unused"), "{}", describe(&out));
    Ok(())
}

#[test]
fn usage_errors_exit_2_and_respect_json() -> TestResult {
    let env = Env::offline()?;
    // Missing argument, as JSON on stdout.
    let out = env.run(&["--json", "mine"])?;
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    let value: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(value["error"]["code"], "cli");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("<BLOCKS>")),
        "{value}"
    );
    // Out of range, human: clap's message on stderr, nothing on stdout.
    let out = env.run(&["mine", "0"])?;
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(out.stdout.is_empty(), "{}", describe(&out));
    assert!(stderr(&out).contains("error:"), "{}", describe(&out));
    Ok(())
}

#[test]
fn commands_before_create_say_wallet_not_found() -> TestResult {
    let env = Env::offline()?;
    for args in [
        &["balance"][..],
        &["history"],
        &["utxos"],
        &["address", "new"],
        &["address", "list"],
        &["sync"],
    ] {
        let (code, message) = env.json_error(args)?;
        assert_eq!(code, "wallet_not_found", "{args:?}: {message}");
        assert!(message.contains("create or restore one first"), "{message}");
    }

    // Human mode: the core's message, unchanged, on stderr; nothing on stdout.
    let out = env.run(&["balance"])?;
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(out.stdout.is_empty(), "{}", describe(&out));
    let err = stderr(&out);
    assert!(
        err.starts_with(&format!(
            "error: no wallet found in {}; create or restore one first",
            env.network_dir().display()
        )),
        "{err}"
    );
    // Opening a missing wallet leaves no trace.
    assert!(!env.network_dir().exists());
    Ok(())
}

#[test]
fn network_policy_errors_reach_the_user() -> TestResult {
    let env = Env::offline()?;
    let (code, message) = json_error(&env.command_on("bitcoin", &["--json", "balance"]).output()?)?;
    assert_eq!(code, "mainnet_disabled", "{message}");
    assert!(message.contains("mainnet is disabled"), "{message}");

    // The opt-in flag alone is not enough without the cargo feature.
    let out = env
        .command_on(
            "bitcoin",
            &["--json", "--i-understand-mainnet-risk", "balance"],
        )
        .output()?;
    let (code, _) = json_error(&out)?;
    let expected = if cfg!(feature = "mainnet") {
        "wallet_not_found"
    } else {
        "mainnet_disabled"
    };
    assert_eq!(code, expected);

    let (code, message) = json_error(
        &env.command_on("testnet3", &["--json", "balance"])
            .output()?,
    )?;
    assert_eq!(code, "config");
    assert_eq!(
        message,
        "configuration error: testnet3 is not supported; use testnet4"
    );

    let out = env.command_on("dogecoin", &["balance"]).output()?;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("unknown network `dogecoin`"),
        "{}",
        describe(&out)
    );
    Ok(())
}

#[test]
fn weak_password_is_refused_before_anything_is_written() -> TestResult {
    let mut env = Env::offline()?;
    env.password = "1234567".to_owned(); // one short of the minimum
    let (code, message) = env.json_error(&["create"])?;
    assert_eq!(code, "weak_password");
    assert_eq!(message, "password must be at least 8 characters");
    assert!(!env.network_dir().join("seed.enc").exists());
    assert!(!env.network_dir().join("wallet.sqlite").exists());

    let (code, _) = env.json_error(&["restore"])?;
    // stdin is empty here, so restore fails even earlier, on the missing phrase.
    assert_eq!(code, "cli");
    let out = env.run_with_stdin(&["--json", "restore"], &format!("{ABANDON}\n"))?;
    let (code, _) = json_error(&out)?;
    assert_eq!(code, "weak_password");
    assert!(!env.network_dir().join("seed.enc").exists());
    Ok(())
}

#[test]
fn without_a_terminal_the_password_must_come_from_the_environment() -> TestResult {
    if Command::new("setsid").arg("--version").output().is_err() {
        eprintln!("skipping: no `setsid`");
        return Ok(());
    }
    let mut env = Env::offline()?;
    env.password = String::new(); // empty means "not set": prompt on the terminal
    let out = env
        .command_without_terminal(&["--json", "create"])
        .output()?;
    let (code, message) = json_error(&out)?;
    assert_eq!(code, "cli");
    assert!(
        message.starts_with("cannot prompt for a password"),
        "{message}"
    );
    assert!(message.contains("set BTCW_PASSWORD"), "{message}");
    assert!(!env.network_dir().join("seed.enc").exists());
    Ok(())
}

#[test]
fn create_with_24_words() -> TestResult {
    let env = Env::offline()?;
    let out = env.run(&["--json", "create", "--words", "24"])?;
    ok_json(&out)?;
    let phrase = phrase_from_grid(&stderr(&out))?;
    assert_eq!(phrase.split(' ').count(), 24);
    keys::parse_mnemonic(&phrase)?;

    // Only 12 and 24 are offered.
    let out = env.run(&["create", "--words", "18"])?;
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    Ok(())
}

/// create → addresses → offline views → Phase 2 stubs → restore the displayed phrase elsewhere.
#[test]
fn offline_wallet_lifecycle() -> TestResult {
    let env = Env::offline()?;

    let out = env.run(&["--json", "create"])?;
    let created = ok_json(&out)?;
    // Exactly these fields: the phrase never goes to stdout in JSON mode.
    assert_eq!(
        keys_of(&created),
        BTreeSet::from(["birthday_height", "first_address", "network"].map(String::from))
    );
    assert_eq!(created["network"], "regtest");
    assert_eq!(
        created["birthday_height"], 0,
        "no node, so scan from genesis"
    );
    let first = created["first_address"]
        .as_str()
        .ok_or("first_address is not a string")?
        .to_owned();
    assert!(first.starts_with("bcrt1q"), "{first}");

    // The phrase, with its warning, went to stderr, along with the "no node" warning.
    let err = stderr(&out);
    let phrase = phrase_from_grid(&err)?;
    assert_eq!(phrase.split(' ').count(), 12, "{err}");
    keys::parse_mnemonic(&phrase)?;
    assert!(!stdout(&out).contains(&phrase));
    assert!(
        err.contains("Anyone with these words can take your coins"),
        "{err}"
    );
    assert!(err.contains("btcw will never show them again"), "{err}");
    assert!(err.contains("scan from the genesis block"), "{err}");
    assert!(!err.contains(PASSWORD), "{err}");
    // The displayed words *are* this wallet's: they derive its first address.
    assert_eq!(expected_first_address(&phrase)?, first);

    let (code, message) = env.json_error(&["create"])?;
    assert_eq!(code, "wallet_exists");
    assert!(
        message.contains(&env.network_dir().display().to_string()),
        "{message}"
    );

    // The same unused address until it is paid.
    let a = env.json(&["address", "new"])?;
    let b = env.json(&["address", "new"])?;
    assert_eq!(a, b);
    assert_eq!(
        a,
        serde_json::json!({
            "network": "regtest", "index": 0, "address": first,
            "keychain": "external", "used": false
        })
    );
    let list = env.json(&["address", "list"])?;
    assert_eq!(list["synced_height"], 0);
    assert_eq!(
        list["addresses"],
        serde_json::json!([{ "index": 0, "address": first, "keychain": "external", "used": false }])
    );

    let balance = env.json(&["balance"])?;
    assert_eq!(
        balance,
        serde_json::json!({
            "network": "regtest", "synced_height": 0,
            "balance": { "confirmed_sat": 0, "unconfirmed_sat": 0, "immature_sat": 0, "total_sat": 0 }
        })
    );
    assert_eq!(
        env.json(&["history"])?["transactions"],
        serde_json::json!([])
    );
    assert_eq!(env.json(&["utxos"])?["utxos"], serde_json::json!([]));

    // Human output: badge, freshness note, amounts, empty states.
    let out = env.run(&["balance"])?;
    assert!(out.status.success(), "{}", describe(&out));
    let text = stdout(&out);
    assert!(
        text.starts_with("[regtest] Balance (not synced yet; run `btcw sync`)"),
        "{text}"
    );
    assert!(
        text.contains("Total       0.00000000 BTC (0 sat)"),
        "{text}"
    );
    let text = stdout(&env.run(&["history"])?);
    assert!(text.starts_with("[regtest] No transactions yet"), "{text}");
    let text = stdout(&env.run(&["utxos"])?);
    assert!(text.starts_with("[regtest] No unspent outputs"), "{text}");
    let text = stdout(&env.run(&["address", "list"])?);
    assert!(text.starts_with("[regtest] Receive addresses"), "{text}");
    assert!(text.contains(&first) && text.contains("unused"), "{text}");
    let text = stdout(&env.run(&["address", "new"])?);
    assert!(
        text.starts_with(&format!("[regtest] Receive address #0\n{first}\n")),
        "{text}"
    );

    // Phase 2 commands: a clear error, still with the JSON shape and exit code.
    let (code, message) = env.json_error(&["send", "--to", &first, "--amount", "1000", "--yes"])?;
    assert_eq!(code, "cli");
    assert_eq!(message, "`btcw send` is not implemented yet (Phase 2)");
    let (code, message) = env.json_error(&["status", &"0".repeat(64)])?;
    assert_eq!(code, "cli");
    assert!(
        message.contains("not implemented yet (Phase 2)"),
        "{message}"
    );
    let out = env.run(&["status", &"0".repeat(64)])?;
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "error: `btcw status` is not implemented yet (Phase 2)\n"
    );

    // No node: sync and mine fail with the core's RPC error.
    let (code, message) = env.json_error(&["sync"])?;
    assert_eq!(code, "rpc");
    assert!(message.contains("connection refused"), "{message}");
    let (code, _) = env.json_error(&["mine", "1"])?;
    assert_eq!(code, "rpc");
    // A wrong-network address is caught before any node is needed.
    let (code, message) = env.json_error(&[
        "mine",
        "1",
        "--to",
        "tb1q6rz28mcfaxtmd6v789l9rrlrusdprr9pqcpvkl",
    ])?;
    assert_eq!(code, "network_mismatch");
    assert_eq!(
        message,
        "network mismatch: expected regtest, found a testnet4/signet address"
    );
    let (code, _) = env.json_error(&["mine", "1", "--to", "bcrt1qnotanaddress"])?;
    assert_eq!(code, "invalid_address");

    // Restoring the displayed phrase elsewhere gives the same wallet (human mode this time).
    let elsewhere = Env::offline()?;
    let out = elsewhere.run_with_stdin(&["restore"], &format!("{phrase}\n"))?;
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        stdout(&out).starts_with("[regtest] Restored a 12-word wallet in "),
        "{}",
        describe(&out)
    );
    assert!(!stdout(&out).contains(&phrase) && !stderr(&out).contains(&phrase));
    assert_eq!(
        elsewhere.json(&["address", "new"])?["address"],
        first.as_str()
    );
    Ok(())
}

#[test]
fn restore_from_stdin_without_a_node_matches_bip84() -> TestResult {
    let env = Env::offline()?;
    let out = env.run_with_stdin(
        &["--json", "restore", "--birthday", "7"],
        &format!("{ABANDON}\n"),
    )?;
    let restored = ok_json(&out)?;
    assert_eq!(restored["network"], "regtest");
    assert_eq!(restored["birthday_height"], 7);
    assert_eq!(restored["synced_height"], 0);
    assert_eq!(restored["sync"], Value::Null);
    assert_eq!(restored["sync_error"]["code"], "rpc");
    assert_eq!(restored["balance"]["total_sat"], 0);
    let err = stderr(&out);
    assert!(
        err.contains("warning: the wallet was restored but not synced"),
        "{err}"
    );
    assert!(err.contains("run `btcw sync`"), "{err}");
    assert!(!err.contains("abandon") && !stdout(&out).contains("abandon"));

    let address = env.json(&["address", "new"])?;
    let expected = expected_first_address(ABANDON)?;
    assert_eq!(address["address"], expected.as_str());
    // Same value BIP84's vector gives for m/84'/1'/0'/0/0, with the regtest prefix.
    assert_eq!(expected, "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk");

    let (code, _) =
        json_error(&env.run_with_stdin(&["--json", "restore"], &format!("{ABANDON}\n"))?)?;
    assert_eq!(code, "wallet_exists");

    // A bad phrase is refused before a password is asked for or anything is written.
    let fresh = Env::offline()?;
    let out = fresh.run_with_stdin(
        &["--json", "restore"],
        &format!("{}\n", "abandon ".repeat(12)),
    )?;
    let (code, message) = json_error(&out)?;
    assert_eq!(code, "invalid_mnemonic");
    assert!(message.contains("checksum"), "{message}");
    assert!(!message.contains("abandon"), "{message}");
    assert!(!fresh.network_dir().exists());
    Ok(())
}

/// Against a real regtest node: create → fund → sync (mempool) → mine → sync → views →
/// restore the phrase from genesis. One test, because each node takes ~20 s to start.
#[test]
fn regtest_round_trip_with_a_node() -> TestResult {
    if !TestNode::available() {
        eprintln!("skipping: no bitcoind (set BITCOIND_EXE)");
        return Ok(());
    }
    let node = TestNode::start()?;
    let env = Env::with_node(&node)?;
    let tip = node.tip_height()?;

    let out = env.run(&["--json", "create"])?;
    let created = ok_json(&out)?;
    assert_eq!(as_u64(&created["birthday_height"])?, u64::from(tip));
    let phrase = phrase_from_grid(&stderr(&out))?;
    let first = created["first_address"]
        .as_str()
        .ok_or("no address")?
        .to_owned();
    assert_eq!(env.json(&["address", "new"])?["address"], first.as_str());

    // Incoming payment, still in the mempool.
    let to = first
        .parse::<Address<NetworkUnchecked>>()?
        .require_network(Network::Regtest)?;
    let funding = node.fund(&to, Amount::from_sat(1_000_000))?.to_string();
    let sync = env.json(&["sync"])?;
    assert_eq!(sync["network"], "regtest");
    assert_eq!(as_u64(&sync["tip_height"])?, u64::from(tip));
    assert_eq!(sync["blocks_scanned"], 1, "only the birthday block");
    assert!(as_u64(&sync["mempool_txs"])? >= 1, "{sync}");
    let balance = env.json(&["balance"])?;
    assert_eq!(balance["balance"]["confirmed_sat"], 0);
    assert_eq!(balance["balance"]["unconfirmed_sat"], 1_000_000);
    let history = env.json(&["history"])?;
    assert_eq!(history["transactions"][0]["txid"], funding.as_str());
    assert_eq!(history["transactions"][0]["status"]["state"], "unconfirmed");
    assert_eq!(history["transactions"][0]["fee_sat"], Value::Null);
    assert_eq!(
        env.json(&["address", "list"])?["addresses"][0]["used"],
        true
    );

    // Mine one block to the wallet itself: index 0 is used now, so index 1 gets the reward.
    let mined = env.json(&["mine", "1"])?;
    assert_eq!(mined["wallet_address_index"], 1);
    assert_eq!(as_u64(&mined["tip_height"])?, u64::from(tip) + 1);
    assert_eq!(mined["block_hashes"].as_array().map(Vec::len), Some(1));
    assert_ne!(mined["to"], first.as_str());

    let sync = env.json(&["sync"])?;
    assert_eq!(sync["blocks_scanned"], 1);
    assert_eq!(sync["mempool_txs"], 0);
    let balance = env.json(&["balance"])?;
    let view = &balance["balance"];
    assert_eq!(as_u64(&balance["synced_height"])?, u64::from(tip) + 1);
    assert_eq!(view["confirmed_sat"], 1_000_000);
    assert_eq!(view["unconfirmed_sat"], 0);
    // 50 BTC subsidy plus the funding transaction's fee, locked for 100 blocks.
    let immature = as_u64(&view["immature_sat"])?;
    assert!(
        (5_000_000_001..5_000_100_000).contains(&immature),
        "{immature}"
    );
    assert_eq!(as_u64(&view["total_sat"])?, 1_000_000 + immature);

    let history = env.json(&["history"])?;
    let txs = history["transactions"]
        .as_array()
        .ok_or("no transactions")?;
    assert_eq!(txs.len(), 2);
    for tx in txs {
        assert_eq!(tx["status"]["state"], "confirmed", "{tx}");
        assert_eq!(tx["status"]["confirmations"], 1, "{tx}");
        assert_eq!(as_u64(&tx["status"]["height"])?, u64::from(tip) + 1);
    }
    assert!(
        txs.iter()
            .any(|tx| tx["txid"] == funding.as_str() && tx["net_sat"] == 1_000_000)
    );
    assert!(
        txs.iter()
            .any(|tx| as_u64(&tx["net_sat"]).ok() == Some(immature))
    );

    let utxos = env.json(&["utxos"])?;
    let utxos = utxos["utxos"].as_array().ok_or("no utxos")?;
    assert_eq!(utxos.len(), 2);
    assert!(
        utxos
            .iter()
            .all(|u| u["confirmed"] == true && u["keychain"] == "external")
    );
    let mut indexes: Vec<u64> = utxos
        .iter()
        .map(|u| as_u64(&u["derivation_index"]))
        .collect::<TestResult<_>>()?;
    indexes.sort_unstable();
    assert_eq!(indexes, [0, 1]);

    // Human views of the same state.
    let text = stdout(&env.run(&["history"])?);
    assert!(
        text.starts_with(&format!("[regtest] 2 transactions as of block {}", tip + 1)),
        "{text}"
    );
    assert!(text.contains("+0.01000000 BTC (+1,000,000 sat)"), "{text}");
    assert!(
        text.contains("1 confirmation") && text.contains("received"),
        "{text}"
    );
    assert!(text.contains(&funding), "{text}");
    let text = stdout(&env.run(&["balance"])?);
    assert!(text.contains("0.01000000 BTC (1,000,000 sat)"), "{text}");
    assert!(
        text.contains("Immature") && text.contains("spendable after 100"),
        "{text}"
    );
    let text = stdout(&env.run(&["sync"])?);
    assert_eq!(
        text,
        format!(
            "[regtest] Synced to block {}: scanned 0 blocks; no unconfirmed wallet transactions.\n",
            tip + 1
        )
    );

    // Two more blocks: confirmations follow the synced tip.
    node.mine(2)?;
    env.json(&["sync"])?;
    let history = env.json(&["history"])?;
    let confirmations: Vec<&Value> = history["transactions"]
        .as_array()
        .ok_or("no transactions")?
        .iter()
        .map(|tx| &tx["status"]["confirmations"])
        .collect();
    assert_eq!(confirmations, [3, 3]);
    let balance = env.json(&["balance"])?;

    // Restoring the phrase in a fresh datadir rescans from genesis and finds the same coins.
    let elsewhere = Env::with_node(&node)?;
    let out = elsewhere.run_with_stdin(&["--json", "restore"], &format!("{phrase}\n"))?;
    let restored = ok_json(&out)?;
    assert_eq!(restored["birthday_height"], 0);
    assert_eq!(restored["sync_error"], Value::Null);
    assert_eq!(
        as_u64(&restored["sync"]["blocks_scanned"])?,
        u64::from(tip) + 3
    );
    assert_eq!(restored["balance"], balance["balance"]);
    assert_eq!(elsewhere.json(&["history"])?, history);
    Ok(())
}

/// Sanity check for the helper the tests above rely on.
#[test]
fn grid_parser_reads_numbered_words_in_order() -> TestResult {
    let grid =
        "Recovery phrase (4 words).\n\n     1. zoo     2. wrong\n     3. abandon  4. about\n";
    assert_eq!(phrase_from_grid(grid)?, "zoo wrong abandon about");
    assert!(phrase_from_grid("     2. zoo\n").is_err());
    assert!(Path::new(env!("CARGO_BIN_EXE_btcw")).exists());
    Ok(())
}
