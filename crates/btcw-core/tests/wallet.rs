//! Agent C: `WalletService` lifecycle through the public API: create, open, the lock, and
//! every way opening can fail. Offline; the receive/confirm paths are in-module tests in
//! `src/wallet.rs` because they need the crate-private `bdk_mut()`.

use std::error::Error;
use std::path::Path;

use btcw_core::WalletError;
use btcw_core::bitcoin::Network;
use btcw_core::config::{Config, Overrides};
use btcw_core::keys::{self, Descriptors};
use btcw_core::wallet::WalletService;

type TestResult = Result<(), Box<dyn Error>>;

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
/// A second BIP39 test-vector phrase: a different seed for the "wrong seed" case.
const LEGAL: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";

fn config(datadir: &Path, network: &str) -> Result<Config, Box<dyn Error>> {
    Ok(Config::load_with(
        Overrides {
            datadir: Some(datadir.to_path_buf()),
            network: Some(network.into()),
            ..Default::default()
        },
        |_| None,
    )?)
}

fn descriptors(phrase: &str, network: Network) -> Result<Descriptors, Box<dyn Error>> {
    let (descriptors, _signer) =
        keys::derive_account(&keys::parse_mnemonic(phrase)?, "", network, 0)?;
    Ok(descriptors)
}

/// `Result::unwrap_err` needs `T: Debug`, and these tests return a `Box<dyn Error>` instead.
fn expect_err<T>(r: btcw_core::Result<T>) -> Result<WalletError, Box<dyn Error>> {
    match r {
        Ok(_) => Err("expected an error, got Ok".into()),
        Err(e) => Ok(e),
    }
}

#[test]
fn create_then_open_with_and_without_expected_descriptors() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = config(dir.path(), "regtest")?;
    let descriptors = descriptors(ABANDON, Network::Regtest)?;

    let wallet = WalletService::create(&cfg, &descriptors, Some(4_321))?;
    assert_eq!(wallet.network(), Network::Regtest);
    assert_eq!(wallet.birthday_height(), 4_321);
    assert_eq!(wallet.synced_height(), 0);
    assert!(cfg.wallet_db_path().exists());
    drop(wallet);

    let watch_only = WalletService::open(&cfg, None)?;
    assert_eq!(watch_only.birthday_height(), 4_321);
    assert_eq!(watch_only.synced_height(), 0);
    assert!(watch_only.addresses().is_empty());
    assert_eq!(watch_only.balance().total_sat, 0);
    assert!(watch_only.history().is_empty());
    assert!(watch_only.utxos().is_empty());
    drop(watch_only);

    let unlocked = WalletService::open(&cfg, Some(&descriptors))?;
    assert_eq!(unlocked.birthday_height(), 4_321);
    Ok(())
}

#[test]
fn no_birthday_means_genesis() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = config(dir.path(), "regtest")?;
    drop(WalletService::create(
        &cfg,
        &descriptors(ABANDON, Network::Regtest)?,
        None,
    )?);
    assert_eq!(WalletService::open(&cfg, None)?.birthday_height(), 0);
    Ok(())
}

#[test]
fn addresses_persist_across_reopen() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = config(dir.path(), "regtest")?;
    let mut wallet = WalletService::create(&cfg, &descriptors(ABANDON, Network::Regtest)?, None)?;
    let first = wallet.new_address()?;
    assert_eq!(first.index, 0);
    assert!(first.address.starts_with("bcrt1q"), "{}", first.address);
    drop(wallet);

    let mut reopened = WalletService::open(&cfg, None)?;
    assert_eq!(reopened.addresses(), vec![first.clone()]);
    // Still unused, so it's offered again rather than revealing index 1.
    assert_eq!(reopened.new_address()?, first);
    Ok(())
}

#[test]
fn create_twice_is_wallet_exists() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = config(dir.path(), "regtest")?;
    let descriptors = descriptors(ABANDON, Network::Regtest)?;
    drop(WalletService::create(&cfg, &descriptors, None)?);

    let err = expect_err(WalletService::create(&cfg, &descriptors, None))?;
    assert!(
        matches!(&err, WalletError::WalletExists(p) if *p == cfg.network_dir()),
        "{err:?}"
    );
    // The failed attempt must not have damaged the existing wallet.
    WalletService::open(&cfg, Some(&descriptors))?;
    Ok(())
}

#[test]
fn open_missing_is_wallet_not_found_and_creates_nothing() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = config(dir.path(), "regtest")?;

    let err = expect_err(WalletService::open(&cfg, None))?;
    assert!(
        matches!(&err, WalletError::WalletNotFound(p) if *p == cfg.network_dir()),
        "{err:?}"
    );
    assert!(!cfg.wallet_db_path().exists());
    // Not even the network directory or the lock file.
    assert!(!cfg.network_dir().exists());
    Ok(())
}

#[test]
fn open_database_without_wallet_is_wallet_not_found() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = config(dir.path(), "regtest")?;
    std::fs::create_dir_all(cfg.network_dir())?;
    // A zero-length file is a valid, empty SQLite database.
    std::fs::write(cfg.wallet_db_path(), b"")?;

    let err = expect_err(WalletService::open(&cfg, None))?;
    assert!(matches!(err, WalletError::WalletNotFound(_)), "{err:?}");
    Ok(())
}

#[test]
fn open_garbage_file_is_persist_error() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = config(dir.path(), "regtest")?;
    std::fs::create_dir_all(cfg.network_dir())?;
    std::fs::write(cfg.wallet_db_path(), [0xAB; 4096])?;

    let err = expect_err(WalletService::open(&cfg, None))?;
    assert!(matches!(err, WalletError::Persist(_)), "{err:?}");
    Ok(())
}

#[test]
fn open_with_another_seed_is_persist_error() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = config(dir.path(), "regtest")?;
    drop(WalletService::create(
        &cfg,
        &descriptors(ABANDON, Network::Regtest)?,
        None,
    )?);

    let err = expect_err(WalletService::open(
        &cfg,
        Some(&descriptors(LEGAL, Network::Regtest)?),
    ))?;
    match &err {
        WalletError::Persist(msg) => {
            assert!(msg.contains("does not belong to this seed"), "{msg}");
            assert!(!msg.contains("tpub"), "{msg}");
        }
        other => return Err(format!("expected Persist, got {other:?}").into()),
    }
    // Watch-only open still works: the database itself is fine.
    WalletService::open(&cfg, None)?;
    Ok(())
}

#[test]
fn regtest_database_in_signet_dir_is_network_mismatch() -> TestResult {
    let dir = tempfile::tempdir()?;
    let regtest = config(dir.path(), "regtest")?;
    drop(WalletService::create(
        &regtest,
        &descriptors(ABANDON, Network::Regtest)?,
        None,
    )?);

    let signet = config(dir.path(), "signet")?;
    std::fs::create_dir_all(signet.network_dir())?;
    std::fs::copy(regtest.wallet_db_path(), signet.wallet_db_path())?;

    let err = expect_err(WalletService::open(&signet, None))?;
    match err {
        WalletError::NetworkMismatch { expected, found } => {
            assert_eq!(expected, Network::Signet);
            assert_eq!(found, "regtest");
        }
        other => return Err(format!("expected NetworkMismatch, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn second_open_while_first_is_alive_is_wallet_in_use() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = config(dir.path(), "regtest")?;
    let created = WalletService::create(&cfg, &descriptors(ABANDON, Network::Regtest)?, None)?;

    // `create` holds the lock as well as `open`.
    let err = expect_err(WalletService::open(&cfg, None))?;
    assert!(matches!(err, WalletError::WalletInUse), "{err:?}");
    drop(created);

    let first = WalletService::open(&cfg, None)?;
    let err = expect_err(WalletService::open(&cfg, None))?;
    assert!(matches!(err, WalletError::WalletInUse), "{err:?}");
    assert_eq!(err.code(), "wallet_in_use");
    drop(first);

    WalletService::open(&cfg, None)?;
    Ok(())
}

#[test]
fn lock_is_per_network() -> TestResult {
    let dir = tempfile::tempdir()?;
    let regtest = config(dir.path(), "regtest")?;
    let signet = config(dir.path(), "signet")?;
    let _regtest_wallet =
        WalletService::create(&regtest, &descriptors(ABANDON, Network::Regtest)?, None)?;
    let _signet_wallet =
        WalletService::create(&signet, &descriptors(ABANDON, Network::Signet)?, None)?;
    Ok(())
}

#[test]
fn failed_create_leaves_no_database_behind() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = config(dir.path(), "regtest")?;
    let bad = Descriptors::from_strings("wpkh(not-a-key)", "wpkh(also-not-a-key)");

    let err = expect_err(WalletService::create(&cfg, &bad, None))?;
    assert!(matches!(err, WalletError::Config(_)), "{err:?}");
    assert!(!cfg.wallet_db_path().exists());

    // So a retry with good descriptors succeeds instead of hitting `WalletExists`.
    WalletService::create(&cfg, &descriptors(ABANDON, Network::Regtest)?, Some(7))?;
    Ok(())
}
