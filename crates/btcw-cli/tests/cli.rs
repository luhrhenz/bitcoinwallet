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
use std::time::{Duration, Instant};

use btcw_core::bdk_wallet::{KeychainKind, Wallet};
use btcw_core::bitcoin::address::NetworkUnchecked;
use btcw_core::bitcoin::{Address, Amount, Network, Psbt};
use btcw_core::config::RpcAuth;
use btcw_core::keys;
use btcw_core::testnode::TestNode;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const PASSWORD: &str = "correct horse battery";
/// The BIP84 test-vector phrase.
const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
/// Nothing listens on port 1 (tcpmux), so connecting is refused immediately.
const CLOSED_RPC_URL: &str = "http://127.0.0.1:1";
/// `ABANDON`'s first receive address on testnet4: valid, but for the wrong network here.
const TESTNET_ADDRESS: &str = "tb1q6rz28mcfaxtmd6v789l9rrlrusdprr9pqcpvkl";

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

    /// The same on regtest, but inside a pseudo-terminal: `script` runs the command with a pty
    /// as its controlling terminal and copies our stdin into it, which is exactly what someone
    /// typing at a prompt looks like. Its stdout is everything the command wrote to the pty.
    fn command_in_pty(&self, args: &[&str]) -> Command {
        let mut inner = Command::new(env!("CARGO_BIN_EXE_btcw"));
        self.configure(&mut inner, "regtest", args);
        let line: Vec<String> = std::iter::once(inner.get_program())
            .chain(inner.get_args())
            .map(|arg| shell_quote(&arg.to_string_lossy()))
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
        // `script` passes its environment on to the command.
        for (key, value) in inner.get_envs() {
            match value {
                Some(value) => cmd.env(key, value),
                None => cmd.env_remove(key),
            };
        }
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

/// `it's` → `'it'\''s'`, for `sh -c`.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Same grouping as the CLI's `output::grouped`, written independently.
fn in_groups_of_four(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(4)
        .map(|group| group.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `TestNode::available()` runs `bitcoind -version`, and Core v31 rewrites
/// `~/.bitcoin/settings.json` even for that: two probes at once (another test binary, another
/// checkout) can fail on the rename and read as "no bitcoind", silently skipping the node test.
/// Retry, so it only skips when bitcoind really is missing.
fn bitcoind_available() -> bool {
    (0..3).any(|_| TestNode::available())
}

fn has_program(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn mempool(node: &TestNode) -> TestResult<Vec<Value>> {
    Ok(node
        .call("getrawmempool", &[])?
        .as_array()
        .cloned()
        .ok_or("getrawmempool: not an array")?)
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
        "mine", "bump", "contacts", "label",
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

/// create → addresses → offline views → restore the displayed phrase elsewhere.
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
    assert!(err.contains("btcw backup show"), "{err}");
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

/// `send` and `status` check everything they can before asking for a password or a node, and
/// say exactly what is wrong.
#[test]
fn send_and_status_fail_fast_without_a_node() -> TestResult {
    let env = Env::offline()?;
    let txid = "5e3c1f1d2a6b7c8d9e0f11223344556677889900aabbccddeeff001122334455";
    let (code, _) = env.json_error(&["status", txid])?;
    assert_eq!(code, "wallet_not_found");
    let (code, _) = env.json_error(&[
        "send",
        "--yes",
        "--to",
        "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk",
        "--amount",
        "10000",
    ])?;
    assert_eq!(code, "wallet_not_found");

    let created = env.json(&["create"])?;
    let first = created["first_address"]
        .as_str()
        .ok_or("no address")?
        .to_owned();

    // `--json` can't stop to ask, so it needs `--yes`.
    let (code, message) = env.json_error(&["send", "--to", &first, "--amount", "10000"])?;
    assert_eq!(code, "cli");
    assert!(message.starts_with("--json needs --yes"), "{message}");

    // Bad payments are refused before the password is read: each of these runs with a wrong
    // password, which would otherwise be the error.
    let existing = env.root.path().join("existing.psbt");
    std::fs::write(&existing, "keep me")?;
    let existing_str = existing.to_str().ok_or("non-UTF-8 temp path")?;
    for (args, expected, needle) in [
        (
            vec!["--to", TESTNET_ADDRESS, "--amount", "10000"],
            "network_mismatch",
            "network mismatch: expected regtest, found a testnet4/signet address",
        ),
        (
            vec!["--to", "bcrt1qnotanaddress", "--amount", "10000"],
            "invalid_address",
            "invalid address: `bcrt1qnotanaddress`",
        ),
        (
            vec!["--to", &first, "--amount", "293"],
            "dust_amount",
            "below the dust limit",
        ),
        (
            vec!["--to", &first, "--amount", "10000", "--fee-rate", "0"],
            "tx_build",
            "fee rate 0 sat/vB is below the 1 sat/vB minimum",
        ),
        (
            vec![
                "--to",
                &first,
                "--amount",
                "10000",
                "--psbt-out",
                existing_str,
            ],
            "cli",
            "already exists; refusing to overwrite it",
        ),
    ] {
        let mut full = vec!["--json", "send", "--yes"];
        full.extend(args);
        let out = env
            .command_on("regtest", &full)
            .env("BTCW_PASSWORD", "not the password")
            .output()?;
        let (code, message) = json_error(&out)?;
        assert_eq!(code, expected, "{full:?}: {message}");
        assert!(message.contains(needle), "{full:?}: {message}");
    }
    assert_eq!(std::fs::read_to_string(&existing)?, "keep me");

    // The password isn't asked until a payment has been previewed and approved, so without a
    // node the next stop is the connection, whatever the password.
    for password in ["not the password", PASSWORD] {
        let out = env
            .command_on(
                "regtest",
                &[
                    "--json", "send", "--yes", "--to", &first, "--amount", "10000",
                ],
            )
            .env("BTCW_PASSWORD", password)
            .output()?;
        let (code, message) = json_error(&out)?;
        assert_eq!(code, "rpc", "{message}");
        assert!(message.contains("connection refused"), "{message}");
    }

    // `status`: a malformed txid, then an unknown one answered from the wallet file.
    let (code, message) = env.json_error(&["status", "not-a-txid"])?;
    assert_eq!(code, "cli");
    assert!(
        message.starts_with("invalid transaction id `not-a-txid`"),
        "{message}"
    );
    assert!(message.contains("64 hexadecimal characters"), "{message}");
    let out = env.run(&["--json", "status", txid])?;
    let (code, message) = json_error(&out)?;
    assert_eq!(code, "tx_not_found");
    assert_eq!(message, format!("transaction not found: {txid}"));
    let err = stderr(&out);
    assert!(
        err.contains("warning: could not sync with bitcoind, so this is the status as of block 0"),
        "{err}"
    );
    assert!(err.contains("this wallet's own transactions"), "{err}");
    let out = env.run(&["status", "--watch", "--interval", "0", txid])?;
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));

    // The confirmation is read from the terminal, never from stdin; without one, say so.
    if has_program("setsid") {
        let out = env
            .command_without_terminal(&["send", "--to", &first, "--amount", "10000"])
            .output()?;
        assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
        assert!(out.stdout.is_empty(), "{}", describe(&out));
        let err = stderr(&out);
        assert!(
            err.starts_with("error: cannot open the terminal to confirm the payment"),
            "{err}"
        );
        assert!(err.contains("pass --yes"), "{err}");
    } else {
        eprintln!("skipping the no-terminal check: no `setsid`");
    }
    Ok(())
}

/// Agent I: the address book, labels and `bump`, as far as they go without a node.
#[test]
fn contacts_labels_and_bump_without_a_node() -> TestResult {
    let env = Env::offline()?;
    let txid = "5e3c1f1d2a6b7c8d9e0f11223344556677889900aabbccddeeff001122334455";
    let (code, _) = env.json_error(&["contacts", "list"])?;
    assert_eq!(code, "wallet_not_found");
    let (code, _) = env.json_error(&["bump", txid, "--fee-rate", "5", "--yes"])?;
    assert_eq!(code, "wallet_not_found");

    let created = env.json(&["create"])?;
    let first = created["first_address"]
        .as_str()
        .ok_or("no address")?
        .to_owned();

    let out = env.run(&["contacts", "list"])?;
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        stdout(&out).starts_with("[regtest] No contacts yet"),
        "{}",
        describe(&out)
    );
    assert_eq!(env.json(&["contacts", "list"])?["contacts"], json!([]));

    let added = env.json(&["contacts", "add", " Alice ", &first, "--note", "landlord"])?;
    assert_eq!(
        added,
        json!({
            "network": "regtest",
            "contact": { "name": "Alice", "address": first, "note": "landlord" }
        })
    );
    let out = env.run(&["contacts", "add", "Bob", &first])?;
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        stdout(&out).contains(&format!("  Address   {}", in_groups_of_four(&first))),
        "{}",
        describe(&out)
    );
    for (args, expected, needle) in [
        (
            vec!["contacts", "add", "ALICE", &first],
            "contact",
            "a contact named `Alice` already exists",
        ),
        (
            vec!["contacts", "add", "Carol", TESTNET_ADDRESS],
            "network_mismatch",
            "expected regtest",
        ),
        (
            vec!["contacts", "add", "bcrt1q carol", &first],
            "contact",
            "looks like a Bitcoin address",
        ),
        (
            vec!["contacts", "remove", "Carol"],
            "contact",
            "no contact is named `Carol`",
        ),
        (
            vec!["contacts", "rename", "bob", "alice"],
            "contact",
            "already exists",
        ),
    ] {
        let (code, message) = env.json_error(&args)?;
        assert_eq!(code, expected, "{args:?}: {message}");
        assert!(message.contains(needle), "{args:?}: {message}");
    }
    let renamed = env.json(&["contacts", "rename", "bob", "Robert"])?;
    assert_eq!(renamed["renamed_from"], "Bob");
    assert_eq!(renamed["contact"]["name"], "Robert");
    let listed = env.json(&["contacts", "list"])?;
    let names: Vec<&Value> = listed["contacts"]
        .as_array()
        .ok_or("no contacts")?
        .iter()
        .map(|c| &c["name"])
        .collect();
    assert_eq!(names, ["Alice", "Robert"]);
    let text = stdout(&env.run(&["contacts", "list"])?);
    assert!(text.starts_with("[regtest] 2 contacts\n"), "{text}");
    assert!(text.contains(&first) && text.contains("landlord"), "{text}");
    let removed = env.json(&["contacts", "remove", "robert"])?;
    assert_eq!(removed["removed"]["name"], "Robert");

    // `send --to NAME`: an unknown name is refused before the node; a known one gets as far as
    // the node (there is none), whatever the password.
    let (code, message) =
        env.json_error(&["send", "--yes", "--to", "Alcie", "--amount", "10000"])?;
    assert_eq!(code, "contact");
    assert_eq!(
        message,
        "no contact is named `Alcie`, and it is not a valid address either"
    );
    let (code, message) = env.json_error(&["send", "--yes", "--to", "alice", "--amount", "293"])?;
    assert_eq!(code, "dust_amount", "{message}");
    let out = env
        .command_on(
            "regtest",
            &[
                "--json", "send", "--yes", "--to", "alice", "--amount", "10000",
            ],
        )
        .env("BTCW_PASSWORD", "not the password")
        .output()?;
    let (code, message) = json_error(&out)?;
    assert_eq!(code, "rpc", "{message}");

    // Labels need a transaction the wallet knows.
    let (code, _) = env.json_error(&["label", txid, "rent"])?;
    assert_eq!(code, "tx_not_found");
    let (code, _) = env.json_error(&["label", txid, "--clear"])?;
    assert_eq!(code, "tx_not_found");
    let (code, message) = env.json_error(&["label", "nope", "rent"])?;
    assert_eq!(code, "cli");
    assert!(message.starts_with("invalid transaction id"), "{message}");
    let out = env.run(&["label", txid])?;
    assert_eq!(
        out.status.code(),
        Some(2),
        "text or --clear: {}",
        describe(&out)
    );
    let out = env.run(&["label", txid, "rent", "--clear"])?;
    assert_eq!(out.status.code(), Some(2), "not both: {}", describe(&out));

    // `bump`: checks first, then the node (which isn't there).
    for (args, expected, needle) in [
        (
            vec!["bump", txid, "--fee-rate", "5"],
            "cli",
            "--json needs --yes",
        ),
        (
            vec!["bump", "nope", "--fee-rate", "5", "--yes"],
            "cli",
            "invalid transaction id",
        ),
        (
            vec!["bump", txid, "--fee-rate", "0", "--yes"],
            "tx_build",
            "below the 1 sat/vB minimum",
        ),
        (
            vec!["bump", txid, "--fee-rate", "30000", "--yes"],
            "tx_build",
            "above the 25000 sat/vB safety limit",
        ),
        (
            vec!["bump", txid, "--fee-rate", "5", "--yes"],
            "rpc",
            "connection refused",
        ),
    ] {
        let (code, message) = env.json_error(&args)?;
        assert_eq!(code, expected, "{args:?}: {message}");
        assert!(message.contains(needle), "{args:?}: {message}");
    }
    if has_program("setsid") {
        let out = env
            .command_without_terminal(&["bump", txid, "--fee-rate", "5"])
            .output()?;
        assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
        assert!(stderr(&out).contains("pass --yes"), "{}", describe(&out));
    }
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
    if !bitcoind_available() {
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
    // (Before any payment, so the restored history matches the one above.)
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
    drop(elsewhere);

    send_and_follow(&node, &env)
}

/// `send` and `status` against the node, from the wallet the round trip above built: 1 000 000
/// sat spendable (the 50 BTC reward is still immature).
fn send_and_follow(node: &TestNode, env: &Env) -> TestResult {
    let faucet = node.faucet_address()?.to_string();
    let tip = node.tip_height()?;
    assert!(mempool(node)?.is_empty());

    let (code, message) =
        env.json_error(&["send", "--yes", "--to", &faucet, "--amount", "100000000"])?;
    assert_eq!(code, "insufficient_funds", "{message}");
    assert!(
        message.starts_with("insufficient funds: need 1.0"),
        "{message}"
    );

    // Declined at the prompt, typed into a real pseudo-terminal: nothing is sent.
    if has_program("script") {
        let mut child = env
            .command_in_pty(&["send", "--to", &faucet, "--amount", "100000"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().ok_or("no stdin")?.write_all(b"n\n")?;
        let out = child.wait_with_output()?;
        assert!(out.status.success(), "{}", describe(&out));
        let text = stdout(&out);
        assert!(text.contains("[regtest] Payment preview"), "{text}");
        assert!(text.contains(&in_groups_of_four(&faucet)), "{text}");
        assert!(text.contains("Send? [y/N]"), "{text}");
        assert!(text.contains("Cancelled; nothing was sent."), "{text}");
        assert!(mempool(node)?.is_empty());
    } else {
        eprintln!("skipping the interactive prompt check: no `script`");
    }

    // Approved but signed with the wrong password: refused after the preview, nothing sent.
    let out = env
        .command_on(
            "regtest",
            &[
                "--json", "send", "--yes", "--to", &faucet, "--amount", "100000",
            ],
        )
        .env("BTCW_PASSWORD", "not the password")
        .output()?;
    let (code, message) = json_error(&out)?;
    assert_eq!(code, "wrong_password", "{message}");
    assert!(mempool(node)?.is_empty());

    // Sent, with the unsigned PSBT kept for inspection.
    let psbt_path = env.root.path().join("payment.psbt");
    let out = env.run(&[
        "--json",
        "send",
        "--yes",
        "--to",
        &faucet,
        "--amount",
        "100000",
        "--fee-rate",
        "5",
        "--psbt-out",
        psbt_path.to_str().ok_or("non-UTF-8 temp path")?,
    ])?;
    let sent = ok_json(&out)?;
    assert_eq!(
        keys_of(&sent),
        BTreeSet::from(["network", "preview", "txid"].map(String::from))
    );
    assert_eq!(sent["network"], "regtest");
    let txid = sent["txid"].as_str().ok_or("no txid")?.to_owned();
    let preview = &sent["preview"];
    let fee = as_u64(&preview["fee_sat"])?;
    let vsize = as_u64(&preview["vsize"])?;
    assert_eq!(preview["to"], faucet.as_str());
    assert_eq!(preview["amount_sat"], 100_000);
    assert_eq!(as_u64(&preview["total_sat"])?, 100_000 + fee);
    assert_eq!(as_u64(&preview["change_sat"])?, 1_000_000 - 100_000 - fee);
    assert!((5 * vsize - 5..=5 * vsize + 5).contains(&fee), "{preview}");
    let rate = preview["fee_rate_sat_vb"].as_f64().ok_or("no fee rate")?;
    assert!((rate - 5.0).abs() < 0.1, "{preview}");
    assert!(
        stderr(&out).contains("wrote the unsigned PSBT"),
        "{}",
        describe(&out)
    );

    // The PSBT file: private, unsigned, the very transaction that was broadcast, and Core agrees
    // with our fee.
    let text = std::fs::read_to_string(&psbt_path)?;
    let psbt: Psbt = text.trim_end().parse()?;
    assert_eq!(psbt.unsigned_tx.compute_txid().to_string(), txid);
    assert!(
        psbt.inputs
            .iter()
            .all(|input| input.partial_sigs.is_empty() && input.final_script_witness.is_none())
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&psbt_path)?.permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }
    let decoded = node.call("decodepsbt", &[json!(text.trim_end())])?;
    assert_eq!(decoded["tx"]["txid"], txid.as_str());
    let core_fee = decoded["fee"].as_f64().ok_or("decodepsbt: no fee")?;
    assert_eq!((core_fee * 1e8).round(), fee as f64);

    // In the node's mempool, and in the wallet before any sync.
    assert_eq!(mempool(node)?, [json!(txid)]);
    let change = 1_000_000 - 100_000 - fee;
    let balance = env.json(&["balance"])?;
    assert_eq!(balance["balance"]["confirmed_sat"], 0);
    assert_eq!(balance["balance"]["unconfirmed_sat"], change);

    let status = env.json(&["status", &txid])?;
    assert_eq!(
        keys_of(&status),
        BTreeSet::from(
            ["network", "status", "sync_error", "synced_height", "txid"].map(String::from)
        )
    );
    assert_eq!(status["txid"], txid.as_str());
    assert_eq!(status["status"]["state"], "unconfirmed");
    assert_eq!(status["sync_error"], Value::Null);
    assert_eq!(as_u64(&status["synced_height"])?, u64::from(tip));
    let text = stdout(&env.run(&["status", &txid])?);
    assert!(
        text.starts_with(&format!("[regtest] Transaction {txid}\n")),
        "{text}"
    );
    assert!(
        text.contains("unconfirmed, waiting in the mempool"),
        "{text}"
    );
    assert!(text.contains(&format!("As of       block {tip}")), "{text}");

    // `--watch` until 2 confirmations while two blocks are mined, one at a time.
    let mut watcher = env
        .command_on(
            "regtest",
            &[
                "status",
                &txid,
                "--watch",
                "--until",
                "2",
                "--interval",
                "1",
            ],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    std::thread::sleep(Duration::from_millis(1_500));
    node.mine(1)?;
    std::thread::sleep(Duration::from_millis(1_500));
    node.mine(1)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while watcher.try_wait()?.is_none() {
        if Instant::now() > deadline {
            watcher.kill()?;
            return Err("status --watch did not stop at 2 confirmations".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = watcher.wait_with_output()?;
    assert!(out.status.success(), "{}", describe(&out));
    let text = stdout(&out);
    assert!(
        text.starts_with(&format!(
            "[regtest] Watching {txid} until it has 2 confirmations (checking every 1 s; Ctrl-C to stop)\n"
        )),
        "{text}"
    );
    // A line per change and never the same line twice. (Whether the first check still saw the
    // transaction unconfirmed depends on timing, so that line isn't required.)
    let lines: Vec<&str> = text.lines().skip(1).collect();
    assert!(lines.windows(2).all(|pair| pair[0] != pair[1]), "{text}");
    assert!(
        lines.len() <= 3,
        "unconfirmed, 1 and 2 confirmations at most: {text}"
    );
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("  block ") && line.contains("confirm")),
        "{text}"
    );
    assert!(
        text.trim_end()
            .ends_with(&format!("confirmed in block {}: 2 confirmations", tip + 1)),
        "{text}"
    );

    let status = env.json(&["status", &txid, "--watch", "--until", "2"])?;
    assert_eq!(
        status["status"],
        json!({
            "state": "confirmed",
            "height": tip + 1,
            "confirmations": 2,
            "block_time": status["status"]["block_time"],
        })
    );
    assert_eq!(
        as_u64(&env.json(&["balance"])?["balance"]["confirmed_sat"])?,
        change
    );

    // In human mode, with fees that deserve a second look: 150 sat/vB, and far more than 10% of
    // the 1 000 sat being sent.
    let out = env.run(&[
        "send",
        "--yes",
        "--to",
        &faucet,
        "--amount",
        "1000",
        "--fee-rate",
        "150",
    ])?;
    assert!(out.status.success(), "{}", describe(&out));
    let text = stdout(&out);
    assert!(text.starts_with("[regtest] Payment preview\n"), "{text}");
    assert!(
        text.contains(&format!("  To        {}\n", in_groups_of_four(&faucet))),
        "{text}"
    );
    assert!(
        text.contains("  Amount    0.00001000 BTC (1,000 sat)"),
        "{text}"
    );
    assert!(
        text.contains("[regtest] Sent 0.00001000 BTC (1,000 sat) to"),
        "{text}"
    );
    assert!(text.contains("--watch`."), "{text}");
    let err = stderr(&out);
    assert!(err.contains("warning: the fee ("), "{err}");
    assert!(err.contains("of the amount being sent"), "{err}");
    assert!(err.contains("unusually high"), "{err}");
    assert_eq!(mempool(node)?.len(), 1);

    contacts_labels_and_bump(node, env)
}

/// Agent I, on the same wallet and node: pay a contact by name (the preview shows the full
/// address), label the payment, speed it up with `bump` (declined once at a real prompt, then
/// sent), bump the replacement again as JSON, and the refusals: too low, `--json` without
/// `--yes`, already confirmed.
fn contacts_labels_and_bump(node: &TestNode, env: &Env) -> TestResult {
    let faucet = node.faucet_address()?.to_string();
    // Start from confirmed coins only, so the payments below have no unconfirmed parent.
    node.mine(1)?;
    env.json(&["sync"])?;
    assert!(mempool(node)?.is_empty());
    env.json(&[
        "contacts",
        "add",
        "Faucet",
        &faucet,
        "--note",
        "the node's wallet",
    ])?;

    // Human mode, by name: the address is still printed in full, the name below it.
    let out = env.run(&[
        "send",
        "--yes",
        "--to",
        "faucet",
        "--amount",
        "20000",
        "--fee-rate",
        "2",
    ])?;
    assert!(out.status.success(), "{}", describe(&out));
    let text = stdout(&out);
    assert!(
        text.contains(&format!(
            "  To        {}\n  Contact   Faucet\n",
            in_groups_of_four(&faucet)
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "[regtest] Sent 0.00020000 BTC (20,000 sat) to Faucet ({faucet})"
        )),
        "{text}"
    );
    let original = text
        .lines()
        .find_map(|line| line.strip_prefix("Txid: "))
        .ok_or("no txid line")?
        .to_owned();
    assert_eq!(mempool(node)?, [json!(original)]);

    // Label it; `history` shows the label (JSON and the table's last column).
    let labelled = env.json(&["label", &original, "coffee", "beans"])?;
    assert_eq!(labelled["label"], "coffee beans");
    let history = env.json(&["history"])?;
    let row = history["transactions"]
        .as_array()
        .ok_or("no transactions")?
        .iter()
        .find(|tx| tx["txid"] == original.as_str())
        .ok_or("the payment is not in the history")?
        .clone();
    assert_eq!(row["label"], "coffee beans");
    let old_fee = as_u64(&row["fee_sat"])?;
    let text = stdout(&env.run(&["history"])?);
    assert!(
        text.contains("Label") && text.contains("coffee beans"),
        "{text}"
    );
    let balance_before = as_u64(&env.json(&["balance"])?["balance"]["total_sat"])?;

    // Refused before anything is built or asked.
    let (code, message) = env.json_error(&["bump", &original, "--fee-rate", "5"])?;
    assert_eq!(code, "cli");
    assert!(message.starts_with("--json needs --yes"), "{message}");
    let (code, message) = env.json_error(&["bump", &original, "--fee-rate", "2", "--yes"])?;
    assert_eq!(code, "tx_build", "{message}");
    assert!(message.contains("too low"), "{message}");

    // Declined at a real prompt: the preview names the payment it replaces; nothing is sent.
    if has_program("script") {
        let mut child = env
            .command_in_pty(&["bump", &original, "--fee-rate", "5"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().ok_or("no stdin")?.write_all(b"n\n")?;
        let out = child.wait_with_output()?;
        assert!(out.status.success(), "{}", describe(&out));
        let text = stdout(&out);
        assert!(text.contains("[regtest] Fee bump preview"), "{text}");
        assert!(text.contains(&format!("Replaces  {original}")), "{text}");
        assert!(text.contains(&in_groups_of_four(&faucet)), "{text}");
        assert!(text.contains("Send the replacement? [y/N]"), "{text}");
        assert!(text.contains("Cancelled; nothing was sent"), "{text}");
        assert_eq!(mempool(node)?, [json!(original)]);
    } else {
        eprintln!("skipping the interactive bump prompt: no `script`");
    }

    // Sent: the node and the wallet hold only the replacement, which keeps the label.
    let out = env.run(&["bump", &original, "--fee-rate", "5", "--yes"])?;
    assert!(out.status.success(), "{}", describe(&out));
    let text = stdout(&out);
    assert!(text.starts_with("[regtest] Fee bump preview\n"), "{text}");
    assert!(
        text.contains(&format!("  Replaces  {original}\n")),
        "{text}"
    );
    assert!(
        !text.contains("  Contact   Faucet\n"),
        "a bump names no contact: {text}"
    );
    assert!(text.contains(&format!("(from {old_fee} sat)")), "{text}");
    let replacement = text
        .lines()
        .find_map(|line| line.strip_prefix("[regtest] Sent the replacement: "))
        .ok_or("no replacement line")?
        .to_owned();
    assert_eq!(mempool(node)?, [json!(replacement)]);
    let history = env.json(&["history"])?;
    let rows = history["transactions"]
        .as_array()
        .ok_or("no transactions")?;
    assert!(rows.iter().all(|tx| tx["txid"] != original.as_str()));
    let row = rows
        .iter()
        .find(|tx| tx["txid"] == replacement.as_str())
        .ok_or("the replacement is not in the history")?;
    assert_eq!(row["label"], "coffee beans");
    let new_fee = as_u64(&row["fee_sat"])?;
    assert!(new_fee > old_fee);
    assert_eq!(
        as_u64(&env.json(&["balance"])?["balance"]["total_sat"])?,
        balance_before - (new_fee - old_fee)
    );
    let (code, _) = env.json_error(&["status", &original])?;
    assert_eq!(code, "tx_not_found");

    // Again, as JSON: same shape as `send`, plus `preview.replaces`.
    let bumped = env.json(&["bump", &replacement, "--fee-rate", "9", "--yes"])?;
    assert_eq!(
        keys_of(&bumped),
        BTreeSet::from(["network", "preview", "txid"].map(String::from))
    );
    assert_eq!(bumped["preview"]["replaces"], replacement.as_str());
    assert_eq!(bumped["preview"]["to"], faucet.as_str());
    assert_eq!(bumped["preview"]["amount_sat"], 20_000);
    assert_eq!(bumped["preview"]["contact"], Value::Null);
    let third = bumped["txid"].as_str().ok_or("no txid")?.to_owned();
    assert_eq!(mempool(node)?, [json!(third)]);
    let entry = node.call("getmempoolentry", &[json!(third)])?;
    let core_fee = entry["fees"]["base"].as_f64().ok_or("no fees.base")?;
    assert_eq!(
        Amount::from_btc(core_fee)?.to_sat(),
        as_u64(&bumped["preview"]["fee_sat"])?
    );

    // Confirmed: too late to bump.
    node.mine(1)?;
    let (code, message) = env.json_error(&["bump", &third, "--fee-rate", "20", "--yes"])?;
    assert_eq!(code, "tx_build", "{message}");
    assert!(message.contains("already confirmed"), "{message}");
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

#[test]
fn backup_reminder_verify_and_show() -> TestResult {
    let env = Env::offline()?;
    let out = env.run(&["--json", "create"])?;
    let created = ok_json(&out)?;
    let phrase = phrase_from_grid(&stderr(&out))?;
    assert!(
        stderr(&out).contains("btcw backup verify"),
        "{}",
        describe(&out)
    );
    let first = created["first_address"]
        .as_str()
        .ok_or("no first_address")?
        .to_owned();

    // Every other command reminds (on stderr; stdout JSON stays clean) until verified.
    let out = env.run(&["--json", "balance"])?;
    ok_json(&out)?;
    assert!(
        stderr(&out).contains("run `btcw backup verify`"),
        "{}",
        describe(&out)
    );
    let out = env.run(&["address", "list"])?;
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        stderr(&out).contains("btcw backup verify"),
        "{}",
        describe(&out)
    );

    // `show` needs the password, prints the same words, and is never JSON.
    let (code, message) = env.json_error(&["backup", "show"])?;
    assert_eq!(code, "cli");
    assert!(
        message.contains("never written as machine-readable data"),
        "{message}"
    );
    let out = env
        .command_on("regtest", &["backup", "show"])
        .env("BTCW_PASSWORD", "not the password")
        .output()?;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("wrong password"),
        "{}",
        describe(&out)
    );
    assert!(!stdout(&out).contains(phrase.split(' ').next().unwrap_or("-")));
    let out = env.run(&["backup", "show"])?;
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(phrase_from_grid(&stdout(&out))?, phrase);
    assert!(
        stderr(&out).contains("not a terminal"),
        "{}",
        describe(&out)
    );

    // A wrong phrase on stdin: positions only in the message, and still unverified.
    let mut wrong: Vec<&str> = phrase.split(' ').collect();
    wrong.swap(0, 1);
    let out = env.run_with_stdin(
        &["--json", "backup", "verify"],
        &format!("{}\n", wrong.join(" ")),
    )?;
    let (code, message) = json_error(&out)?;
    assert_eq!(code, "backup_mismatch");
    assert_eq!(message, "words 1 and 2 do not match your recovery phrase");
    assert!(stderr(&env.run(&["balance"])?).contains("btcw backup verify"));

    // The right phrase verifies; the reminder stops; `show` still works afterwards.
    let out = env.run_with_stdin(&["--json", "backup", "verify"], &format!("{phrase}\n"))?;
    assert_eq!(ok_json(&out)?["backup_verified"], true);
    let out = env.run(&["balance"])?;
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        !stderr(&out).contains("backup verify"),
        "{}",
        describe(&out)
    );
    let out = env.run(&["backup", "show"])?;
    assert_eq!(phrase_from_grid(&stdout(&out))?, phrase);

    // A restored wallet starts verified: no reminder at all.
    let restored = Env::offline()?;
    let out = restored.run_with_stdin(&["restore"], &format!("{phrase}\n"))?;
    assert!(out.status.success(), "{}", describe(&out));
    let out = restored.run(&["address", "new"])?;
    assert!(stdout(&out).contains(&first), "{}", describe(&out));
    assert!(
        !stderr(&out).contains("backup verify"),
        "{}",
        describe(&out)
    );
    Ok(())
}
