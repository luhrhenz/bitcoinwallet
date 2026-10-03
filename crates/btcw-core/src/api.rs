//! High-level wallet lifecycle shared by the CLI and the desktop app.
//!
//! These functions only orchestrate `keys`, `keystore` and `wallet`; they hold no logic of
//! their own. Syncing is left to the caller (it needs a [`crate::chain::Node`] and progress UI).
//!
//! Two ways in:
//! - [`open_watch_only`]: no password. Balance, history, addresses, sync. Cannot sign.
//! - [`unlock_wallet`]: password → decrypt seed → [`Unlocked`] with a [`Signer`] for sending.

use secrecy::{ExposeSecret, SecretString};

use crate::config::Config;
use crate::error::{Result, WalletError};
use crate::keys::{self, Mnemonic, Signer, WordCount};
use crate::keystore;
use crate::wallet::WalletService;

/// Only account 0 is used (`m/84'/coin'/0'`).
pub const ACCOUNT: u32 = 0;

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
    init(cfg, &mnemonic, password, birthday)
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
