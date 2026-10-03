//! Wallet lifecycle through the high-level API both frontends use (offline, no node).

use std::error::Error;
use std::path::Path;

use btcw_core::WalletError;
use btcw_core::api;
use btcw_core::config::{Config, Overrides};
use btcw_core::keys::WordCount;
use secrecy::SecretString;

type TestResult = Result<(), Box<dyn Error>>;

fn regtest(dir: &Path) -> Result<Config, WalletError> {
    Config::load_with(
        Overrides {
            datadir: Some(dir.to_path_buf()),
            network: Some("regtest".into()),
            ..Default::default()
        },
        |_| None,
    )
}

fn pw(s: &str) -> SecretString {
    SecretString::from(s.to_owned())
}

#[test]
fn create_unlock_watch_only_and_restore() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = regtest(dir.path())?;
    assert!(!api::wallet_exists(&cfg));

    let created = api::create_wallet(&cfg, WordCount::Words12, &pw("correct horse"), Some(7))?;
    assert_eq!(created.mnemonic.word_count(), 12);
    let phrase = created.mnemonic.phrase();
    let mut wallet = created.unlocked.wallet;
    assert_eq!(wallet.birthday_height(), 7);
    let first = wallet.new_address()?;
    drop(wallet);
    drop(created.unlocked.signer);
    assert!(api::wallet_exists(&cfg));

    // A second create in the same place is refused before anything is overwritten.
    assert!(matches!(
        api::create_wallet(&cfg, WordCount::Words12, &pw("correct horse"), None),
        Err(WalletError::WalletExists(_))
    ));

    // Unlock needs the right password; watch-only needs none.
    assert!(matches!(
        api::unlock_wallet(&cfg, &pw("wrong horse!")),
        Err(WalletError::WrongPassword)
    ));
    let unlocked = api::unlock_wallet(&cfg, &pw("correct horse"))?;
    assert_eq!(unlocked.wallet.addresses().first(), Some(&first));
    drop(unlocked);
    let watch = api::open_watch_only(&cfg)?;
    assert_eq!(watch.addresses().first(), Some(&first));
    drop(watch);

    // Restoring the phrase elsewhere yields the same wallet.
    let dir2 = tempfile::tempdir()?;
    let cfg2 = regtest(dir2.path())?;
    let mut restored = api::restore_wallet(&cfg2, &phrase, &pw("another password"), None)?;
    assert_eq!(restored.wallet.birthday_height(), 0);
    assert_eq!(restored.wallet.new_address()?.address, first.address);
    Ok(())
}

#[test]
fn weak_passwords_are_rejected_before_anything_is_written() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = regtest(dir.path())?;
    let short = "1234567"; // one below the minimum
    assert_eq!(short.len() + 1, api::MIN_PASSWORD_LEN);
    assert!(matches!(
        api::create_wallet(&cfg, WordCount::Words12, &pw(short), None),
        Err(WalletError::WeakPassword(8))
    ));
    let abandon = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    assert!(matches!(
        api::restore_wallet(&cfg, abandon, &pw(""), None),
        Err(WalletError::WeakPassword(_))
    ));
    assert!(!api::wallet_exists(&cfg));
    assert!(!cfg.network_dir().exists());
    Ok(())
}

#[test]
fn bad_phrase_on_restore_leaves_nothing_behind() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = regtest(dir.path())?;
    assert!(matches!(
        api::restore_wallet(&cfg, "not a real phrase at all", &pw("long enough"), None),
        Err(WalletError::InvalidMnemonic(_))
    ));
    assert!(!api::wallet_exists(&cfg));
    Ok(())
}
