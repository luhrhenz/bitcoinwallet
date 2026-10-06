//! `btcw backup verify` / `btcw backup show`.
//!
//! - `verify`: password, then [`api::BACKUP_CHECK_WORDS`] words at random positions, typed
//!   hidden (or the whole phrase on stdin). Success stops the reminder.
//! - `show`: password, then the numbered grid. Never as `--json`.
//! - [`remind`]: the warning other commands print while the backup is unverified.

use std::io::IsTerminal;

use anyhow::{Result, bail};
use btcw_core::WalletError;
use btcw_core::api;
use btcw_core::config::Config;
use btcw_core::wallet::WalletService;
use secrecy::zeroize::Zeroizing;
use serde::Serialize;

use crate::commands::create::write_phrase;
use crate::output::Ui;
use crate::prompt;

/// Room for a 24-word grid, the warning and escape codes, so the phrase buffer never
/// reallocates and leaves no unwiped copy.
const PHRASE_TEXT_CAPACITY: usize = 4096;

#[derive(Debug, Serialize)]
struct VerifyJson {
    network: String,
    backup_verified: bool,
}

/// Warn on stderr while the backup is unverified. Lock-free, so it works while the desktop app
/// has the wallet open; any error just skips the reminder.
pub fn remind(cfg: &Config, ui: &Ui) {
    if let Ok(Some(false)) = WalletService::read_backup_verified(cfg) {
        ui.warn(
            "your recovery phrase backup is not verified yet: run `btcw backup verify` \
             (`btcw backup show` displays the words again)",
        );
    }
}

pub fn verify(cfg: &Config, ui: &Ui) -> Result<()> {
    if !api::wallet_exists(cfg) {
        return Err(WalletError::WalletNotFound(cfg.network_dir()).into());
    }
    // Open first: it takes the lock, and the result is written to the wallet database.
    let mut wallet = api::open_watch_only(cfg)?;
    let password = prompt::password(ui)?;

    let answers: Vec<(usize, String)> = if std::io::stdin().is_terminal() {
        // Also checks the password before the user types any words.
        let word_count = api::reveal_phrase(cfg, &password)?.word_count();
        let positions = api::backup_challenge(word_count)?;
        let _ = ui.eprintln(
            "Take out your paper copy and type the requested words (hidden while you type).\n\
             Word positions count from 1, as numbered when the phrase was shown.",
        );
        let mut answers = Vec::with_capacity(positions.len());
        for position in positions {
            let word = prompt::hidden_word(&format!("Word #{position}: "))?;
            answers.push((position, word.trim().to_owned()));
        }
        answers
    } else {
        // Scripts and tests: the whole phrase on stdin checks every word.
        let phrase = prompt::recovery_phrase()?;
        phrase
            .split_whitespace()
            .enumerate()
            .map(|(i, word)| (i + 1, word.to_owned()))
            .collect()
    };
    let result = api::verify_backup(cfg, &mut wallet, &password, &answers);
    // The typed words are as secret as the phrase.
    for (_, word) in answers {
        drop(Zeroizing::new(word));
    }
    result?;
    drop(wallet);

    if ui.json() {
        return ui.print_json(&VerifyJson {
            network: cfg.network.to_string(),
            backup_verified: true,
        });
    }
    ui.println(&format!(
        "{} Backup verified: your copy of the recovery phrase is correct. Keep it offline and safe.",
        ui.out.badge(cfg.network)
    ))
}

pub fn show(cfg: &Config, ui: &Ui) -> Result<()> {
    if ui.json() {
        bail!(
            "`backup show` has no --json output: the recovery phrase is never written as \
             machine-readable data"
        );
    }
    if !api::wallet_exists(cfg) {
        return Err(WalletError::WalletNotFound(cfg.network_dir()).into());
    }
    let password = prompt::password(ui)?;
    let mnemonic = api::reveal_phrase(cfg, &password)?;
    drop(password);

    let phrase = mnemonic.phrase();
    drop(mnemonic);
    let mut text = Zeroizing::new(String::with_capacity(PHRASE_TEXT_CAPACITY));
    text.push_str(&format!("{} ", ui.out.badge(cfg.network)));
    write_phrase(&mut text, &phrase, ui.out)
        .map_err(|e| anyhow::anyhow!("cannot format the recovery phrase: {e}"))?;
    drop(phrase);
    ui.println(&text)?;
    drop(text);

    if !std::io::stdout().is_terminal() {
        ui.warn(
            "standard output is not a terminal, so the recovery phrase was written to wherever \
             it is redirected; delete that copy",
        );
    }
    if let Ok(Some(false)) = WalletService::read_backup_verified(cfg) {
        ui.note("once the words are on paper, run `btcw backup verify`");
    }
    Ok(())
}
