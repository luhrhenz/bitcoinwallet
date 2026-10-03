//! High-level wallet lifecycle shared by the CLI and the desktop app.
//!
//! These functions only orchestrate `keys`, `keystore` and `wallet`; they hold no logic of
//! their own. Syncing is left to the caller (it needs a [`crate::chain::Node`] and progress UI).
//!
//! Two ways in:
//! - [`open_watch_only`]: no password. Balance, history, addresses, sync. Cannot sign.
//! - [`unlock_wallet`]: password → decrypt seed → [`Unlocked`] with a [`Signer`] for sending.
//!   [`load_signer`] does the same for a wallet that is already open watch-only, so a frontend
//!   can show a payment preview first and ask for the password only once the user says "send".
//!
//! Backup (the recovery phrase on paper is the only real backup):
//! - a new wallet starts *unverified*; frontends remind the user until [`verify_backup`] passes
//!   (a restored wallet is verified: the user just typed the phrase in)
//! - [`backup_challenge`] picks the word positions to ask for
//! - [`reveal_phrase`] shows the phrase again, with the password, at any time

use secrecy::{ExposeSecret, SecretString};

use crate::config::Config;
use crate::error::{Result, WalletError};
use crate::keys::{self, Mnemonic, Signer, WordCount};
use crate::keystore;
use crate::wallet::WalletService;

/// Only account 0 is used (`m/84'/coin'/0'`).
pub const ACCOUNT: u32 = 0;

/// Words asked for in the backup check.
pub const BACKUP_CHECK_WORDS: usize = 3;

/// Minimum length for a new wallet password, enforced here so the CLI and desktop app agree.
/// Argon2id makes each guess expensive, but it can't save a 4-character password.
pub const MIN_PASSWORD_LEN: usize = 8;

/// A wallet that can spend. Drop it (or just the signer) to lock.
#[derive(Debug)]
pub struct Unlocked {
    pub wallet: WalletService,
    pub signer: Signer,
}

/// Returned once from [`create_wallet`]: the caller must show `mnemonic` to the user for
/// backup and then drop it. It is never stored unencrypted.
#[derive(Debug)]
pub struct CreatedWallet {
    pub mnemonic: Mnemonic,
    pub unlocked: Unlocked,
}

pub fn wallet_exists(cfg: &Config) -> bool {
    cfg.keystore_path().exists() || cfg.wallet_db_path().exists()
}

/// New random wallet. `birthday` should be the current tip height when a node is reachable.
pub fn create_wallet(
    cfg: &Config,
    words: WordCount,
    password: &SecretString,
    birthday: Option<u32>,
) -> Result<CreatedWallet> {
    ensure_absent(cfg)?;
    check_new_password(password)?;
    let mnemonic = keys::generate_mnemonic(words)?;
    let unlocked = init(cfg, &mnemonic, password, birthday)?;
    Ok(CreatedWallet { mnemonic, unlocked })
}

/// Wallet from an existing phrase. `birthday = None` rescans from genesis (safe default).
pub fn restore_wallet(
    cfg: &Config,
    phrase: &str,
    password: &SecretString,
    birthday: Option<u32>,
) -> Result<Unlocked> {
    ensure_absent(cfg)?;
    check_new_password(password)?;
    let mnemonic = keys::parse_mnemonic(phrase)?;
    let mut unlocked = init(cfg, &mnemonic, password, birthday)?;
    // The user just typed the whole phrase in: that is the backup, and it's correct.
    unlocked.wallet.set_backup_verified(true)?;
    Ok(unlocked)
}

/// Read-only access; no password needed.
pub fn open_watch_only(cfg: &Config) -> Result<WalletService> {
    WalletService::open(cfg, None)
}

/// Decrypt the keystore and open the wallet with a signer, verifying the seed matches it.
pub fn unlock_wallet(cfg: &Config, password: &SecretString) -> Result<Unlocked> {
    let mnemonic = keystore::load(&cfg.keystore_path(), cfg.network, password)?;
    let (descriptors, signer) = keys::derive_account(&mnemonic, "", cfg.network, ACCOUNT)?;
    let wallet = WalletService::open(cfg, Some(&descriptors))?;
    Ok(Unlocked { wallet, signer })
}

/// Applied to *new* passwords only; unlocking never second-guesses an existing one.
pub fn check_new_password(password: &SecretString) -> Result<()> {
    if password.expose_secret().chars().count() < MIN_PASSWORD_LEN {
        return Err(WalletError::WeakPassword(MIN_PASSWORD_LEN));
    }
    Ok(())
}

/// The [`Signer`] for a wallet that is already open (e.g. watch-only, to show a preview first).
/// Fails with `Persist` if the decrypted seed doesn't belong to `wallet`.
pub fn load_signer(
    cfg: &Config,
    wallet: &WalletService,
    password: &SecretString,
) -> Result<Signer> {
    let mnemonic = keystore::load(&cfg.keystore_path(), cfg.network, password)?;
    let (descriptors, signer) = keys::derive_account(&mnemonic, "", cfg.network, ACCOUNT)?;
    if !wallet.matches(&descriptors) {
        return Err(WalletError::Persist(
            "the encrypted seed does not belong to this wallet database".into(),
        ));
    }
    Ok(signer)
}

/// The recovery phrase, decrypted with the password. The caller shows it and drops it; it must
/// never be logged, stored or put in machine-readable output.
pub fn reveal_phrase(cfg: &Config, password: &SecretString) -> Result<Mnemonic> {
    keystore::load(&cfg.keystore_path(), cfg.network, password)
}

/// [`BACKUP_CHECK_WORDS`] distinct, randomly chosen 1-based word positions, ascending.
pub fn backup_challenge(word_count: usize) -> Result<Vec<usize>> {
    let picks = BACKUP_CHECK_WORDS.min(word_count);
    let mut positions: Vec<usize> = Vec::with_capacity(picks);
    while positions.len() < picks {
        let position = random_below(word_count)? + 1;
        if !positions.contains(&position) {
            positions.push(position);
        }
    }
    positions.sort_unstable();
    Ok(positions)
}

/// Check words the user read from their paper copy against the encrypted phrase; on success the
/// wallet is marked verified. `answers` are `(1-based position, word)`; give at least
/// [`BACKUP_CHECK_WORDS`] (or every word). Comparison ignores case and surrounding spaces.
///
/// `BackupMismatch` lists the wrong positions (never the words); `WrongPassword` if the password
/// is wrong. A failed check leaves the flag unchanged.
pub fn verify_backup(
    cfg: &Config,
    wallet: &mut WalletService,
    password: &SecretString,
    answers: &[(usize, String)],
) -> Result<()> {
    let mnemonic = keystore::load(&cfg.keystore_path(), cfg.network, password)?;
    let phrase = mnemonic.phrase();
    let words: Vec<&str> = phrase.split_whitespace().collect();

    let needed = BACKUP_CHECK_WORDS.min(words.len());
    let mut seen: Vec<usize> = Vec::with_capacity(answers.len());
    for (position, _) in answers {
        if *position == 0 || *position > words.len() {
            return Err(WalletError::Config(format!(
                "word position {position} is outside 1..={}",
                words.len()
            )));
        }
        if seen.contains(position) {
            return Err(WalletError::Config(format!(
                "word position {position} was given twice"
            )));
        }
        seen.push(*position);
    }
    if seen.len() < needed {
        return Err(WalletError::Config(format!(
            "the backup check needs at least {needed} words, got {}",
            seen.len()
        )));
    }

    let mut wrong: Vec<usize> = answers
        .iter()
        .filter(|(position, answer)| {
            let expected = words.get(position - 1).copied().unwrap_or_default();
            !answer.trim().eq_ignore_ascii_case(expected)
        })
        .map(|(position, _)| *position)
        .collect();
    if !wrong.is_empty() {
        wrong.sort_unstable();
        return Err(WalletError::BackupMismatch(wrong));
    }
    wallet.set_backup_verified(true)
}

/// Uniform in `0..n` from the OS RNG (rejection sampling, so no modulo bias).
fn random_below(n: usize) -> Result<usize> {
    let n = u32::try_from(n)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| WalletError::Config(format!("cannot pick a position out of {n} words")))?;
    let zone = u32::MAX - (u32::MAX % n);
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).map_err(|e| {
            WalletError::Io(std::io::Error::other(format!(
                "OS random number generator failed: {e}"
            )))
        })?;
        let value = u32::from_le_bytes(bytes);
        if value < zone {
            // `value % n < n ≤ u32::MAX`, and usize is at least 32 bits on every target we build.
            return Ok((value % n) as usize);
        }
    }
}

fn ensure_absent(cfg: &Config) -> Result<()> {
    if wallet_exists(cfg) {
        return Err(WalletError::WalletExists(cfg.network_dir()));
    }
    Ok(())
}

fn init(
    cfg: &Config,
    mnemonic: &Mnemonic,
    password: &SecretString,
    birthday: Option<u32>,
) -> Result<Unlocked> {
    std::fs::create_dir_all(cfg.network_dir())?;
    let (descriptors, signer) = keys::derive_account(mnemonic, "", cfg.network, ACCOUNT)?;
    keystore::save(&cfg.keystore_path(), mnemonic, cfg.network, password)?;
    // Don't leave a keystore without a wallet behind if creating the database fails.
    let wallet = WalletService::create(cfg, &descriptors, birthday).inspect_err(|_| {
        let _ = std::fs::remove_file(cfg.keystore_path());
    })?;
    Ok(Unlocked { wallet, signer })
}
