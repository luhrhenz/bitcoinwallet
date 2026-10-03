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

// ── Backup verification, showing the phrase, deferred signing ─────────────────────────────

use btcw_core::wallet::WalletService;

fn words_of(phrase: &str) -> Vec<String> {
    phrase.split_whitespace().map(str::to_owned).collect()
}

#[test]
fn new_wallets_start_unverified_and_restores_start_verified() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = regtest(dir.path())?;
    let created = api::create_wallet(&cfg, WordCount::Words12, &pw("correct horse"), None)?;
    let phrase = created.mnemonic.phrase();
    assert!(!created.unlocked.wallet.backup_verified());
    drop(created);
    assert_eq!(WalletService::read_backup_verified(&cfg)?, Some(false));
    assert!(!api::open_watch_only(&cfg)?.backup_verified());

    let dir2 = tempfile::tempdir()?;
    let cfg2 = regtest(dir2.path())?;
    let restored = api::restore_wallet(&cfg2, &phrase, &pw("correct horse"), None)?;
    assert!(restored.wallet.backup_verified());
    drop(restored);
    assert_eq!(WalletService::read_backup_verified(&cfg2)?, Some(true));

    // No wallet at all: nothing to remind about.
    let empty = tempfile::tempdir()?;
    assert_eq!(
        WalletService::read_backup_verified(&regtest(empty.path())?)?,
        None
    );
    Ok(())
}

#[test]
fn verify_backup_checks_words_and_persists() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = regtest(dir.path())?;
    let created = api::create_wallet(&cfg, WordCount::Words12, &pw("correct horse"), None)?;
    let words = words_of(&created.mnemonic.phrase());
    let mut wallet = created.unlocked.wallet;

    // Positions: three distinct, ascending, within 1..=12.
    for _ in 0..50 {
        let positions = api::backup_challenge(12)?;
        assert_eq!(positions.len(), api::BACKUP_CHECK_WORDS);
        assert!(positions.windows(2).all(|w| w[0] < w[1]));
        assert!(positions.iter().all(|p| (1..=12).contains(p)));
    }

    let answer = |positions: &[usize]| -> Vec<(usize, String)> {
        positions
            .iter()
            .map(|&p| (p, words[p - 1].clone()))
            .collect()
    };

    // Wrong words: the error names positions, never words, and nothing is marked verified.
    let mut wrong = answer(&[2, 5, 9]);
    wrong[1].1 = "zoo".into();
    wrong[2].1 = "zoo".into();
    match api::verify_backup(&cfg, &mut wallet, &pw("correct horse"), &wrong) {
        Err(e @ WalletError::BackupMismatch(_)) => {
            assert_eq!(e.code(), "backup_mismatch");
            let message = e.to_string();
            assert_eq!(message, "words 5 and 9 do not match your recovery phrase");
            for word in &words {
                assert!(!message.split_whitespace().any(|m| m == word));
            }
        }
        other => return Err(format!("expected BackupMismatch, got {other:?}").into()),
    }
    assert!(!wallet.backup_verified());

    // Wrong password, too few words, bad positions.
    assert!(matches!(
        api::verify_backup(&cfg, &mut wallet, &pw("wrong horse!"), &answer(&[1, 2, 3])),
        Err(WalletError::WrongPassword)
    ));
    assert!(matches!(
        api::verify_backup(&cfg, &mut wallet, &pw("correct horse"), &answer(&[1, 2])),
        Err(WalletError::Config(_))
    ));
    let mut out_of_range = answer(&[1, 2]);
    out_of_range.push((13, "abandon".into()));
    assert!(matches!(
        api::verify_backup(&cfg, &mut wallet, &pw("correct horse"), &out_of_range),
        Err(WalletError::Config(_))
    ));
    let mut twice = answer(&[1, 2]);
    twice.push((1, words[0].clone()));
    assert!(matches!(
        api::verify_backup(&cfg, &mut wallet, &pw("correct horse"), &twice),
        Err(WalletError::Config(_))
    ));
    assert!(!wallet.backup_verified());

    // Right words (any case, stray spaces) verify, and it survives a reopen.
    let mut right = answer(&[3, 7, 12]);
    right[0].1 = format!("  {}  ", right[0].1.to_uppercase());
    api::verify_backup(&cfg, &mut wallet, &pw("correct horse"), &right)?;
    assert!(wallet.backup_verified());
    drop(wallet);
    assert_eq!(WalletService::read_backup_verified(&cfg)?, Some(true));
    assert!(api::open_watch_only(&cfg)?.backup_verified());

    // The whole phrase also counts as an answer (`echo <phrase> | btcw backup verify`).
    let dir2 = tempfile::tempdir()?;
    let cfg2 = regtest(dir2.path())?;
    let created2 = api::create_wallet(&cfg2, WordCount::Words24, &pw("correct horse"), None)?;
    let all: Vec<(usize, String)> = words_of(&created2.mnemonic.phrase())
        .into_iter()
        .enumerate()
        .map(|(i, w)| (i + 1, w))
        .collect();
    let mut wallet2 = created2.unlocked.wallet;
    api::verify_backup(&cfg2, &mut wallet2, &pw("correct horse"), &all)?;
    assert!(wallet2.backup_verified());
    Ok(())
}

#[test]
fn reveal_phrase_needs_the_password_at_any_time() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = regtest(dir.path())?;
    let created = api::create_wallet(&cfg, WordCount::Words12, &pw("correct horse"), None)?;
    let phrase = created.mnemonic.phrase();
    let words = words_of(&phrase);
    let mut wallet = created.unlocked.wallet;

    assert!(matches!(
        api::reveal_phrase(&cfg, &pw("wrong horse!")),
        Err(WalletError::WrongPassword)
    ));
    assert_eq!(
        *api::reveal_phrase(&cfg, &pw("correct horse"))?.phrase(),
        *phrase
    );

    // Still available after the backup is verified.
    let answers: Vec<(usize, String)> = [1, 2, 3]
        .iter()
        .map(|&p| (p, words[p - 1].clone()))
        .collect();
    api::verify_backup(&cfg, &mut wallet, &pw("correct horse"), &answers)?;
    assert_eq!(
        *api::reveal_phrase(&cfg, &pw("correct horse"))?.phrase(),
        *phrase
    );
    Ok(())
}

#[test]
fn load_signer_checks_the_seed_belongs_to_the_open_wallet() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = regtest(dir.path())?;
    drop(api::create_wallet(
        &cfg,
        WordCount::Words12,
        &pw("correct horse"),
        None,
    )?);
    let wallet = api::open_watch_only(&cfg)?;
    assert!(matches!(
        api::load_signer(&cfg, &wallet, &pw("wrong horse!")),
        Err(WalletError::WrongPassword)
    ));
    api::load_signer(&cfg, &wallet, &pw("correct horse"))?;

    // Swap in another wallet's keystore: the password works, but the seed isn't this wallet's.
    let other = tempfile::tempdir()?;
    let other_cfg = regtest(other.path())?;
    drop(api::create_wallet(
        &other_cfg,
        WordCount::Words12,
        &pw("correct horse"),
        None,
    )?);
    std::fs::remove_file(cfg.keystore_path())?;
    std::fs::copy(other_cfg.keystore_path(), cfg.keystore_path())?;
    assert!(matches!(
        api::load_signer(&cfg, &wallet, &pw("correct horse")),
        Err(WalletError::Persist(_))
    ));
    Ok(())
}

#[test]
fn backup_flag_is_readable_while_another_process_has_the_wallet_open() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = regtest(dir.path())?;
    let created = api::create_wallet(&cfg, WordCount::Words12, &pw("correct horse"), None)?;
    // The wallet (and its lock) is still held here.
    assert_eq!(WalletService::read_backup_verified(&cfg)?, Some(false));
    assert!(matches!(
        api::open_watch_only(&cfg),
        Err(WalletError::WalletInUse)
    ));
    drop(created);
    Ok(())
}
