//! Agent F: sending and transaction status against a real regtest `bitcoind`.
//!
//! fund → sync → build at a given fee rate → preview → sign → extract → broadcast → record →
//! mine → sync → confirmed, plus the failure paths that need real wallet state (insufficient
//! funds, cancel, a signer from another seed), through both the step-by-step functions and the
//! `prepare_send` / `complete_send` helpers the desktop bridge uses.
//!
//! Node tests skip (not fail) without `bitcoind`; set `BITCOIND_EXE`. Each node start mines 101
//! blocks (~20 s on Core v31), so the checks are grouped into two tests. Offline unit tests
//! (address parsing, preview math on a hand-funded wallet, dust, fee-rate limits) live in
//! `src/tx.rs`, because funding a wallet without a node needs the crate-private `bdk_mut()`.

use std::error::Error;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use btcw_core::WalletError;
use btcw_core::bitcoin::hashes::Hash;
use btcw_core::bitcoin::{Amount, FeeRate, Network, Psbt, ScriptBuf, Sequence, Txid};
use btcw_core::chain::Node;
use btcw_core::config::{Config, Overrides};
use btcw_core::keys::{self, Signer, WordCount};
use btcw_core::testnode::TestNode;
use btcw_core::tx;
use btcw_core::types::{BalanceView, Keychain, TxStatus};
use btcw_core::wallet::WalletService;
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

/// One funded wallet on a fresh regtest node.
struct Setup {
    test_node: TestNode,
    node: Node,
    cfg: Config,
    wallet: WalletService,
    signer: Signer,
    _dir: tempfile::TempDir,
}

/// `TestNode::available()` runs `bitcoind -version`, and Core v31 rewrites
/// `~/.bitcoin/settings.json` even for that: two probes at once can fail on the rename and read
/// as "no bitcoind", which would silently skip a test. Serialize the probes in this binary and
/// retry, so a test only skips when bitcoind really is missing.
fn bitcoind_available() -> bool {
    static PROBE: Mutex<()> = Mutex::new(());
    let _guard = PROBE.lock().unwrap_or_else(PoisonError::into_inner);
    (0..3).any(|_| TestNode::available())
}

fn regtest_config(datadir: &Path) -> TestResult<Config> {
    Ok(Config::load_with(
        Overrides {
            datadir: Some(datadir.to_path_buf()),
            network: Some("regtest".into()),
            ..Default::default()
        },
        |_| None,
    )?)
}

/// Start a node, create the `abandon … about` wallet at the tip, and give it one confirmed
/// 1 000 000 sat coin.
fn setup() -> TestResult<Setup> {
    let test_node = TestNode::start()?;
    let node = Node::connect(&test_node.rpc_config(), Network::Regtest)?;
    let dir = tempfile::tempdir()?;
    let cfg = regtest_config(dir.path())?;
    let (descriptors, signer) =
        keys::derive_account(&keys::parse_mnemonic(ABANDON)?, "", Network::Regtest, 0)?;
    let mut wallet = WalletService::create(&cfg, &descriptors, Some(node.tip_height()?))?;

    let receive = tx::parse_address(&wallet.new_address()?.address, Network::Regtest)?;
    test_node.fund(&receive, Amount::from_sat(1_000_000))?;
    test_node.mine(1)?;
    node.sync(&mut wallet, &mut |_| {})?;
    assert_eq!(wallet.balance().confirmed_sat, 1_000_000);
    Ok(Setup {
        test_node,
        node,
        cfg,
        wallet,
        signer,
        _dir: dir,
    })
}

fn rate(sat_per_vb: u64) -> TestResult<FeeRate> {
    FeeRate::from_sat_per_vb(sat_per_vb).ok_or_else(|| "fee rate overflow".into())
}

/// The scripts of every output that doesn't pay `recipient`: the change.
fn change_scripts(psbt: &Psbt, recipient: &ScriptBuf) -> Vec<ScriptBuf> {
    psbt.unsigned_tx
        .output
        .iter()
        .filter(|out| out.script_pubkey != *recipient)
        .map(|out| out.script_pubkey.clone())
        .collect()
}

fn expect_err<T>(result: btcw_core::Result<T>) -> TestResult<WalletError> {
    match result {
        Ok(_) => Err("expected an error, got Ok".into()),
        Err(e) => Ok(e),
    }
}

fn confirmations(status: Option<TxStatus>) -> TestResult<(u32, u32)> {
    match status {
        Some(TxStatus::Confirmed {
            height,
            confirmations,
            block_time,
        }) => {
            assert!(block_time > 0);
            Ok((height, confirmations))
        }
        other => Err(format!("expected a confirmed transaction, got {other:?}").into()),
    }
}

fn in_mempool(test_node: &TestNode, txid: Txid) -> TestResult<bool> {
    let mempool = test_node.call("getrawmempool", &[])?;
    Ok(mempool
        .as_array()
        .is_some_and(|txids| txids.contains(&json!(txid.to_string()))))
}

/// The step-by-step API: build → preview → sign → extract → broadcast → record → confirm.
#[test]
fn send_step_by_step_then_watch_it_confirm() -> TestResult {
    if !bitcoind_available() {
        eprintln!("skipping: no bitcoind (set BITCOIND_EXE)");
        return Ok(());
    }
    let Setup {
        test_node,
        node,
        cfg,
        mut wallet,
        signer,
        _dir,
    } = setup()?;
    let faucet = test_node.faucet_address()?;
    let recipient = faucet.script_pubkey();
    let amount = Amount::from_sat(100_000);

    // Failure paths against real, synced state.
    assert_eq!(
        tx::tx_status(&wallet, Txid::from_byte_array([42; 32])),
        None
    );
    match expect_err(tx::build_psbt(
        &mut wallet,
        &faucet,
        Amount::from_sat(2_000_000),
        rate(5)?,
    ))? {
        WalletError::InsufficientFunds { needed, available } => {
            assert_eq!(available, Amount::from_sat(1_000_000));
            assert!(needed > Amount::from_sat(2_000_000), "{needed}");
        }
        other => return Err(format!("expected InsufficientFunds, got {other:?}").into()),
    }
    assert!(matches!(
        expect_err(tx::build_psbt(
            &mut wallet,
            &faucet,
            Amount::from_sat(293),
            rate(5)?
        ))?,
        WalletError::DustAmount(_)
    ));
    assert!(matches!(
        expect_err(tx::build_psbt(
            &mut wallet,
            &faucet,
            amount,
            FeeRate::from_sat_per_kwu(249)
        ))?,
        WalletError::TxBuild(_)
    ));
    assert!(matches!(
        expect_err(tx::parse_address(
            "tb1q6rz28mcfaxtmd6v789l9rrlrusdprr9pqcpvkl",
            Network::Regtest
        ))?,
        WalletError::NetworkMismatch { .. }
    ));

    // A declined payment gives its change address back to the next one.
    let declined = tx::build_psbt(&mut wallet, &faucet, amount, rate(5)?)?;
    let declined_change = change_scripts(&declined, &recipient);
    tx::cancel(&mut wallet, &declined);
    let mut psbt = tx::build_psbt(&mut wallet, &faucet, amount, rate(5)?)?;
    assert_eq!(change_scripts(&psbt, &recipient), declined_change);
    assert_eq!(declined_change.len(), 1);

    let preview = tx::preview(&wallet, &psbt, &faucet, amount)?;
    assert_eq!(preview.to, faucet.to_string());
    assert_eq!(preview.amount_sat, 100_000);
    assert_eq!(preview.total_sat, preview.amount_sat + preview.fee_sat);
    assert_eq!(
        preview.change_sat,
        Some(1_000_000 - 100_000 - preview.fee_sat)
    );
    // BDK sized the fee for this same estimated vsize, give or take rounding.
    let target = 5 * preview.vsize;
    assert!(
        (target - 5..=target + 5).contains(&preview.fee_sat),
        "fee {} sat for ~{} vB at 5 sat/vB",
        preview.fee_sat,
        preview.vsize
    );
    assert!(
        psbt.unsigned_tx
            .input
            .iter()
            .all(|txin| txin.sequence == Sequence::ENABLE_RBF_NO_LOCKTIME),
        "every input signals RBF"
    );

    tx::sign_psbt(&wallet, &signer, &mut psbt)?;
    let signed = tx::extract_tx(psbt)?;
    let inputs = u64::try_from(signed.input.len())?;
    let real = u64::try_from(signed.vsize())?;
    assert!(
        real <= preview.vsize && preview.vsize <= real + 2 * inputs,
        "estimated {} vB, signed {real} vB",
        preview.vsize
    );

    let txid = node.broadcast(&signed)?;
    assert_eq!(txid, signed.compute_txid());
    assert!(in_mempool(&test_node, txid)?);
    tx::record_broadcast(&mut wallet, &signed)?;

    // The spend shows before any sync: the coin is gone, the change is pending.
    let change = preview.change_sat.ok_or("no change")?;
    let pending = BalanceView {
        confirmed_sat: 0,
        unconfirmed_sat: change,
        immature_sat: 0,
        total_sat: change,
    };
    assert_eq!(wallet.balance(), pending);
    let utxos = wallet.utxos();
    assert_eq!(utxos.len(), 1);
    assert_eq!(
        utxos[0].keychain,
        Keychain::Internal,
        "change goes to the internal keychain"
    );
    assert_eq!(utxos[0].value_sat, change);
    let row = wallet
        .history()
        .into_iter()
        .find(|row| row.txid == txid.to_string())
        .ok_or("the payment is not in the history")?;
    assert_eq!(row.net_sat, -i64::try_from(preview.total_sat)?);
    assert_eq!(row.fee_sat, Some(preview.fee_sat));
    assert!(matches!(
        tx::tx_status(&wallet, txid),
        Some(TxStatus::Unconfirmed {
            first_seen: Some(_)
        })
    ));

    // `record_broadcast` persisted it: a restart (before any sync) still knows.
    drop(wallet);
    let mut wallet = WalletService::open(&cfg, None)?;
    assert_eq!(wallet.balance(), pending);
    node.sync(&mut wallet, &mut |_| {})?;
    assert!(matches!(
        tx::tx_status(&wallet, txid),
        Some(TxStatus::Unconfirmed { .. })
    ));

    test_node.mine(1)?;
    node.sync(&mut wallet, &mut |_| {})?;
    let tip = node.tip_height()?;
    assert_eq!(confirmations(tx::tx_status(&wallet, txid))?, (tip, 1));
    assert_eq!(wallet.balance().confirmed_sat, change);

    test_node.mine(2)?;
    node.sync(&mut wallet, &mut |_| {})?;
    assert_eq!(confirmations(tx::tx_status(&wallet, txid))?, (tip, 3));
    Ok(())
}

/// `prepare_send` → `complete_send`, the pair the desktop bridge calls, including a failed
/// signing attempt with someone else's key.
#[test]
fn prepare_and_complete_send() -> TestResult {
    if !bitcoind_available() {
        eprintln!("skipping: no bitcoind (set BITCOIND_EXE)");
        return Ok(());
    }
    let Setup {
        test_node,
        node,
        mut wallet,
        signer,
        _dir,
        ..
    } = setup()?;
    let faucet = test_node.faucet_address()?;
    let recipient = faucet.script_pubkey();
    let to = format!("  {faucet}\n"); // pasted with whitespace

    // Bad input is reported by the same function.
    for (bad, code) in [
        (
            "tb1q6rz28mcfaxtmd6v789l9rrlrusdprr9pqcpvkl",
            "network_mismatch",
        ),
        ("bcrt1qnotanaddress", "invalid_address"),
    ] {
        let e = expect_err(tx::prepare_send(&mut wallet, &node, bad, 50_000, None))?;
        assert_eq!(e.code(), code, "{bad}: {e}");
    }
    let e = expect_err(tx::prepare_send(&mut wallet, &node, &to, 5_000_000, None))?;
    assert_eq!(e.code(), "insufficient_funds", "{e}");

    // No fee rate given: the node's estimate (regtest has none, so the 2 sat/vB fallback).
    let (psbt, preview) = tx::prepare_send(&mut wallet, &node, &to, 50_000, None)?;
    assert_eq!(preview.to, faucet.to_string());
    assert!(preview.fee_rate_sat_vb >= 1.9, "{preview:?}");
    let first_change = change_scripts(&psbt, &recipient);

    // Someone else's key signs nothing; the change address is released again.
    let other_seed = keys::generate_mnemonic(WordCount::Words12)?;
    let (_, stranger) = keys::derive_account(&other_seed, "", Network::Regtest, 0)?;
    match expect_err(tx::complete_send(&mut wallet, &stranger, &node, psbt))? {
        WalletError::Sign(msg) => assert!(msg.contains("signed 0 of 1 inputs"), "{msg}"),
        other => return Err(format!("expected Sign, got {other:?}").into()),
    }
    let mempool_before = test_node.call("getrawmempool", &[])?;
    assert_eq!(mempool_before, json!([]), "nothing was broadcast");

    let (psbt, preview) = tx::prepare_send(&mut wallet, &node, &to, 50_000, Some(rate(10)?))?;
    assert_eq!(change_scripts(&psbt, &recipient), first_change);
    assert!((preview.fee_rate_sat_vb - 10.0).abs() < 0.1, "{preview:?}");

    let txid = tx::complete_send(&mut wallet, &signer, &node, psbt)?;
    assert!(in_mempool(&test_node, txid)?);
    assert_eq!(wallet.balance().total_sat, 1_000_000 - preview.total_sat);
    assert!(matches!(
        tx::tx_status(&wallet, txid),
        Some(TxStatus::Unconfirmed { .. })
    ));

    test_node.mine(1)?;
    node.sync(&mut wallet, &mut |_| {})?;
    assert_eq!(
        confirmations(tx::tx_status(&wallet, txid))?,
        (node.tip_height()?, 1)
    );
    Ok(())
}
