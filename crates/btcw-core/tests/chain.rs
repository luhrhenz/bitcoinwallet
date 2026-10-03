//! Agent B: `chain::Node` against a real regtest `bitcoind`: connecting (and every way it can
//! fail), tip height, fee estimates, mining, and syncing a wallet through receive → confirm →
//! reorg → re-confirm, plus the birthday and resuming from the persisted checkpoint.
//!
//! Node tests skip (not fail) without `bitcoind`; set `BITCOIND_EXE`. Each node start mines 101
//! blocks (~20 s on Core v31), so checks are grouped into a few tests that share a node. The
//! broadcast test is an in-module test in `src/chain.rs`, because building a payment needs the
//! crate-private `bdk_mut()`.

use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};

use btcw_core::WalletError;
use btcw_core::bitcoin::address::NetworkUnchecked;
use btcw_core::bitcoin::{Address, Amount, Network};
use btcw_core::chain::{FALLBACK_FEE_RATE, Node};
use btcw_core::config::{Config, Overrides, RpcAuth, RpcConfig};
use btcw_core::keys::{self, Descriptors};
use btcw_core::testnode::TestNode;
use btcw_core::types::{BalanceView, SyncProgress, SyncReport, TxStatus};
use btcw_core::wallet::WalletService;
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn Error>>;

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

fn regtest_config(datadir: &Path) -> Result<Config, Box<dyn Error>> {
    Ok(Config::load_with(
        Overrides {
            datadir: Some(datadir.to_path_buf()),
            network: Some("regtest".into()),
            ..Default::default()
        },
        |_| None,
    )?)
}

fn descriptors() -> Result<Descriptors, Box<dyn Error>> {
    let (descriptors, _signer) =
        keys::derive_account(&keys::parse_mnemonic(ABANDON)?, "", Network::Regtest, 0)?;
    Ok(descriptors)
}

fn regtest_address(s: &str) -> Result<Address, Box<dyn Error>> {
    Ok(s.parse::<Address<NetworkUnchecked>>()?
        .require_network(Network::Regtest)?)
}

/// `Result::unwrap_err` needs `T: Debug`, and `Node` deliberately isn't (it holds credentials).
fn expect_err<T>(r: btcw_core::Result<T>) -> Result<WalletError, Box<dyn Error>> {
    match r {
        Ok(_) => Err("expected an error, got Ok".into()),
        Err(e) => Ok(e),
    }
}

fn rpc_message(e: WalletError) -> Result<String, Box<dyn Error>> {
    match e {
        WalletError::Rpc(msg) => Ok(msg),
        other => Err(format!("expected an Rpc error, got {other:?}").into()),
    }
}

/// Sync and record every progress event.
fn sync(
    node: &Node,
    wallet: &mut WalletService,
) -> Result<(SyncReport, Vec<SyncProgress>), Box<dyn Error>> {
    let mut events = Vec::new();
    let report = node.sync(wallet, &mut |p| events.push(p))?;
    Ok((report, events))
}

/// The wallet's only transaction, as (status, net amount).
fn only_tx(wallet: &WalletService) -> Result<TxStatus, Box<dyn Error>> {
    match wallet.history().as_slice() {
        [row] => Ok(row.status.clone()),
        other => Err(format!("expected exactly one transaction, got {other:?}").into()),
    }
}

fn balance(confirmed_sat: u64, unconfirmed_sat: u64) -> BalanceView {
    BalanceView {
        confirmed_sat,
        unconfirmed_sat,
        immature_sat: 0,
        total_sat: confirmed_sat + unconfirmed_sat,
    }
}

fn block_hash(test_node: &TestNode, height: u32) -> Result<Value, Box<dyn Error>> {
    Ok(test_node.call("getblockhash", &[json!(height)])?)
}

#[test]
fn connect_failures_are_quick_and_actionable() -> TestResult {
    // Nothing listens on port 1 (privileged, unused): connection refused, immediately. Not a
    // just-released ephemeral port: the regtest nodes other tests start in parallel pick free
    // ports too, and one occasionally landed on it.
    let closed = RpcConfig {
        url: "http://127.0.0.1:1".to_owned(),
        auth: RpcAuth::UserPass {
            user: "alice".into(),
            pass: "hunter2".to_string().into(),
        },
    };
    let started = Instant::now();
    let msg = rpc_message(expect_err(Node::connect(&closed, Network::Regtest))?)?;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    assert!(msg.contains(&closed.url), "{msg}");
    assert!(msg.contains("connection refused"), "{msg}");
    assert!(msg.contains("regtest"), "{msg}");
    assert!(!msg.contains("hunter2"), "{msg}");

    // No cookie file: name the path and say what to do.
    let dir = tempfile::tempdir()?;
    let cookie = dir.path().join("regtest").join(".cookie");
    let no_cookie = RpcConfig {
        url: closed.url.clone(),
        auth: RpcAuth::Cookie(cookie.clone()),
    };
    let msg = rpc_message(expect_err(Node::connect(&no_cookie, Network::Regtest))?)?;
    assert!(msg.contains(&cookie.display().to_string()), "{msg}");
    assert!(msg.contains("is bitcoind running for regtest"), "{msg}");
    assert!(msg.contains("--rpc-cookie"), "{msg}");

    // A malformed URL is a configuration problem, and credentials in it are not echoed.
    let bad_url = RpcConfig {
        url: "ftp://alice:hunter2@127.0.0.1:1".into(),
        auth: closed.auth.clone(),
    };
    match expect_err(Node::connect(&bad_url, Network::Regtest))? {
        WalletError::Config(msg) => {
            assert!(msg.contains("ftp://127.0.0.1:1"), "{msg}");
            assert!(!msg.contains("hunter2"), "{msg}");
        }
        other => return Err(format!("expected Config error, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn connect_checks_the_chain_and_answers_queries() -> TestResult {
    if !TestNode::available() {
        eprintln!("skipping: no bitcoind (set BITCOIND_EXE)");
        return Ok(());
    }
    let test_node = TestNode::start()?;
    let rpc = test_node.rpc_config();

    let node = Node::connect(&rpc, Network::Regtest)?;
    assert_eq!(node.network(), Network::Regtest);
    assert_eq!(node.tip_height()?, test_node.tip_height()?);

    // A testnet4 wallet must never talk to this regtest node.
    match expect_err(Node::connect(&rpc, Network::Testnet4))? {
        WalletError::NetworkMismatch { expected, found } => {
            assert_eq!(expected, Network::Testnet4);
            assert_eq!(found, "regtest");
        }
        other => return Err(format!("expected NetworkMismatch, got {other:?}").into()),
    }

    // A fresh regtest chain has no fee history, so the estimate falls back.
    assert_eq!(node.estimate_fee_rate(6)?, FALLBACK_FEE_RATE);

    // User/password auth: the cookie's credentials work, a wrong password is a clear 401.
    let RpcAuth::Cookie(cookie_path) = &rpc.auth else {
        return Err("test node uses cookie auth".into());
    };
    let cookie = std::fs::read_to_string(cookie_path)?;
    let (user, pass) = cookie
        .trim_end()
        .split_once(':')
        .ok_or("cookie without a colon")?;
    let with_pass = |pass: &str| RpcConfig {
        url: rpc.url.clone(),
        auth: RpcAuth::UserPass {
            user: user.to_owned(),
            pass: pass.to_owned().into(),
        },
    };
    assert_eq!(
        Node::connect(&with_pass(pass), Network::Regtest)?.tip_height()?,
        test_node.tip_height()?
    );
    let msg = rpc_message(expect_err(Node::connect(
        &with_pass("not-the-password"),
        Network::Regtest,
    ))?)?;
    assert!(msg.contains("401"), "{msg}");
    assert!(!msg.contains("not-the-password"), "{msg}");

    // 30 blocks = two `generatetoaddress` batches (25 + 5).
    let before = node.tip_height()?;
    let hashes = node.mine(30, &test_node.faucet_address()?)?;
    assert_eq!(hashes.len(), 30);
    assert_eq!(node.tip_height()?, before + 30);
    assert_eq!(
        block_hash(&test_node, before + 30)?,
        json!(hashes[29].to_string())
    );
    Ok(())
}

#[test]
fn sync_receives_confirms_survives_a_reorg_and_resumes() -> TestResult {
    if !TestNode::available() {
        eprintln!("skipping: no bitcoind (set BITCOIND_EXE)");
        return Ok(());
    }
    let test_node = TestNode::start()?;
    let node = Node::connect(&test_node.rpc_config(), Network::Regtest)?;
    let descriptors = descriptors()?;

    // --- Birthday: a new wallet scans only from the tip at creation, not from genesis. ---
    let dir = tempfile::tempdir()?;
    let cfg = regtest_config(dir.path())?;
    let birthday = node.tip_height()?;
    let mut wallet = WalletService::create(&cfg, &descriptors, Some(birthday))?;
    let (report, events) = sync(&node, &mut wallet)?;
    // Exactly one block: the Emitter jumps from genesis to the birthday block and emits it.
    assert_eq!(
        report,
        SyncReport {
            tip_height: birthday,
            blocks_scanned: 1,
            mempool_txs: 0,
        }
    );
    assert_eq!(
        events,
        vec![SyncProgress {
            height: birthday,
            tip_height: birthday,
        }]
    );

    // --- Receive: unconfirmed first. ---
    let address = regtest_address(&wallet.new_address()?.address)?;
    let txid = test_node.fund(&address, Amount::from_sat(1_000_000))?;
    let (report, _) = sync(&node, &mut wallet)?;
    assert_eq!(report.blocks_scanned, 0);
    assert!(report.mempool_txs >= 1, "{report:?}");
    assert_eq!(wallet.balance(), balance(0, 1_000_000));
    assert!(matches!(only_tx(&wallet)?, TxStatus::Unconfirmed { .. }));
    assert_eq!(wallet.history()[0].txid, txid.to_string());

    // --- Mine it: confirmed with one confirmation. ---
    test_node.mine(1)?;
    let funded_at = birthday + 1;
    let (report, events) = sync(&node, &mut wallet)?;
    assert_eq!(
        report,
        SyncReport {
            tip_height: funded_at,
            blocks_scanned: 1,
            mempool_txs: 0,
        }
    );
    assert_eq!(events.last().map(|p| p.height), Some(funded_at));
    assert_eq!(wallet.balance(), balance(1_000_000, 0));
    match only_tx(&wallet)? {
        TxStatus::Confirmed {
            height,
            confirmations,
            ..
        } => assert_eq!((height, confirmations), (funded_at, 1)),
        other => return Err(format!("expected confirmed, got {other:?}").into()),
    }

    // Nothing new: an immediate second sync downloads nothing.
    let (report, events) = sync(&node, &mut wallet)?;
    assert_eq!(report.blocks_scanned, 0);
    assert!(events.is_empty());
    assert_eq!(report.tip_height, funded_at);

    // --- Reorg: the block holding our payment is replaced by one without it. ---
    let stale_block = block_hash(&test_node, funded_at)?;
    test_node.call("invalidateblock", std::slice::from_ref(&stale_block))?;
    assert_eq!(node.tip_height()?, birthday);
    // With no competing block yet, the node's chain is just shorter. The Emitter only reports
    // blocks that connect, so there's nothing to apply: sync succeeds and keeps our tip until a
    // block at that height exists again (logged as a warning).
    let (report, _) = sync(&node, &mut wallet)?;
    assert_eq!(report.blocks_scanned, 0);

    // The competing block: mined on the old parent with *no* transactions (`generateblock`
    // with an empty list), so our payment stays in the mempool, where Core put it back.
    let faucet = test_node.faucet_address()?.to_string();
    let replacement = test_node.call("generateblock", &[json!(faucet), json!([])])?;
    assert_ne!(replacement["hash"], stale_block);
    let (report, events) = sync(&node, &mut wallet)?;
    // The replacement is emitted at the reorged height, connected to the agreement point below
    // it; BDK drops our stale checkpoint and with it the payment's confirmation.
    assert_eq!(report.blocks_scanned, 1);
    assert_eq!(report.tip_height, funded_at);
    assert_eq!(
        events,
        vec![SyncProgress {
            height: funded_at,
            tip_height: funded_at,
        }]
    );
    assert!(report.mempool_txs >= 1, "{report:?}");
    assert_eq!(wallet.balance(), balance(0, 1_000_000));
    assert!(matches!(only_tx(&wallet)?, TxStatus::Unconfirmed { .. }));

    // Mined again, one block later.
    test_node.mine(1)?;
    let refunded_at = funded_at + 1;
    let (report, _) = sync(&node, &mut wallet)?;
    assert_eq!(report.blocks_scanned, 1);
    assert_eq!(report.mempool_txs, 0);
    assert_eq!(wallet.balance(), balance(1_000_000, 0));
    match only_tx(&wallet)? {
        TxStatus::Confirmed {
            height,
            confirmations,
            ..
        } => assert_eq!((height, confirmations), (refunded_at, 1)),
        other => return Err(format!("expected confirmed, got {other:?}").into()),
    }

    // --- Full rescan: birthday 0 (a restore) scans every block and finds the same coin. ---
    let rescan_dir = tempfile::tempdir()?;
    let mut restored =
        WalletService::create(&regtest_config(rescan_dir.path())?, &descriptors, None)?;
    let tip = node.tip_height()?;
    let (report, events) = sync(&node, &mut restored)?;
    assert_eq!(report.blocks_scanned, tip);
    assert_eq!(report.tip_height, tip);
    let heights: Vec<u32> = events.iter().map(|p| p.height).collect();
    assert_eq!(heights, (1..=tip).collect::<Vec<_>>()); // strictly increasing, ends at the tip
    assert!(events.iter().all(|p| p.tip_height == tip));
    assert_eq!(restored.balance(), wallet.balance());
    drop(restored);

    // --- Resume: reopen from SQLite and continue after the persisted checkpoint. ---
    let before = wallet.balance();
    drop(wallet);
    test_node.mine(2)?;
    let mut reopened = WalletService::open(&cfg, Some(&descriptors))?;
    assert_eq!(reopened.synced_height(), refunded_at);
    let (report, _) = sync(&node, &mut reopened)?;
    assert_eq!(report.blocks_scanned, 2);
    assert_eq!(report.tip_height, refunded_at + 2);
    assert_eq!(reopened.balance(), before);
    match only_tx(&reopened)? {
        TxStatus::Confirmed { confirmations, .. } => assert_eq!(confirmations, 3),
        other => return Err(format!("expected confirmed, got {other:?}").into()),
    }
    Ok(())
}
