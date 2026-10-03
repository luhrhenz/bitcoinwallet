//! Reading secrets: wallet passwords and the recovery phrase.
//!
//! | Secret | Interactive | Non-interactive |
//! |---|---|---|
//! | password | hidden prompt on the TTY (`rpassword` opens `/dev/tty`) | `BTCW_PASSWORD` (insecure, scripts and tests only) |
//! | recovery phrase | hidden prompt when stdin is a terminal | first line of stdin (`echo "<words>" \| btcw restore`) |
//!
//! Prompts are written to the TTY, never to stdout, so `--json` output stays clean. Secrets
//! are copied into `SecretString` / `Zeroizing` buffers straight away and never printed.

use std::io::{BufRead, IsTerminal, Read};

use anyhow::{Context, Result, anyhow, bail};
use btcw_core::api;
use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret, SecretString};

use crate::output::Ui;

/// Wallet password for non-interactive use. Anything in the environment can leak (shell
/// history, child processes, `/proc/<pid>/environ`, crash reports), so it is meant for demos
/// and tests only.
pub const PASSWORD_ENV: &str = "BTCW_PASSWORD";

/// Longest stdin line we accept as a phrase: 24 words of at most 8 letters fit in 215 bytes.
/// The buffer is allocated at this size up front so reading never reallocates (and never
/// leaves an unwiped copy behind).
const MAX_PHRASE_BYTES: usize = 1024;

/// A mistyped or too-short new password gets this many tries before giving up.
const NEW_PASSWORD_ATTEMPTS: u32 = 3;

/// A new wallet password (create / restore): typed twice, must match and pass
/// [`api::check_new_password`].
pub fn new_password(ui: &Ui) -> Result<SecretString> {
    if let Some(password) = password_from_env(ui)? {
        api::check_new_password(&password)?;
        return Ok(password);
    }
    let _ = ui.eprintln(&format!(
        "Choose a password to encrypt the recovery phrase on this computer (at least {} characters).\n\
         You will need it to send coins; viewing the balance and history does not need it.",
        api::MIN_PASSWORD_LEN
    ));
    let mut attempt = 1;
    loop {
        let first = read_password("New wallet password: ")?;
        let problem = match api::check_new_password(&first) {
            Err(weak) => anyhow::Error::from(weak),
            Ok(()) => {
                let second = read_password("Repeat the password: ")?;
                if first.expose_secret() == second.expose_secret() {
                    return Ok(first);
                }
                anyhow!("the two passwords do not match")
            }
        };
        if attempt >= NEW_PASSWORD_ATTEMPTS {
            return Err(problem);
        }
        ui.warn(format_args!("{problem}; please try again"));
        attempt += 1;
    }
}

/// The recovery phrase for `restore`. Hidden prompt on a terminal; otherwise the first line of
/// stdin. The words are validated by the caller (`keys::parse_mnemonic`), never echoed.
pub fn recovery_phrase() -> Result<Zeroizing<String>> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        let typed = rpassword::prompt_password(
            "Recovery phrase (words separated by spaces, hidden while you type): ",
        )
        .map_err(|e| anyhow!("cannot read the recovery phrase from the terminal: {e}"))?;
        return Ok(Zeroizing::new(typed));
    }
    let mut line = Zeroizing::new(String::with_capacity(MAX_PHRASE_BYTES));
    // `take` keeps a stray multi-megabyte stdin from growing the buffer (and copying it).
    // `usize` → `u64` cannot truncate on any platform Rust supports.
    stdin
        .lock()
        .take(MAX_PHRASE_BYTES as u64)
        .read_line(&mut line)
        .context("reading the recovery phrase from standard input")?;
    if line.trim().is_empty() {
        bail!("no recovery phrase on standard input (expected the words on the first line)");
    }
    Ok(line)
}

/// One hidden prompt on the terminal.
fn read_password(prompt: &str) -> Result<SecretString> {
    let typed = Zeroizing::new(rpassword::prompt_password(prompt).map_err(|e| {
        anyhow!(
            "cannot prompt for a password ({e}); for non-interactive use set {PASSWORD_ENV} \
             (insecure: meant for scripts and tests)"
        )
    })?);
    // Copy into an exactly-sized allocation (no reallocation inside `SecretString::from`);
    // `typed` is wiped when it drops.
    Ok(SecretString::from(typed.as_str()))
}

/// `BTCW_PASSWORD`, if set and non-empty (an empty value means "prompt me").
fn password_from_env(ui: &Ui) -> Result<Option<SecretString>> {
    let Some(value) = std::env::var_os(PASSWORD_ENV) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let password = value
        .to_str()
        .map(SecretString::from)
        .ok_or_else(|| anyhow!("{PASSWORD_ENV} is not valid UTF-8"))?;
    ui.note(format_args!(
        "using the wallet password from {PASSWORD_ENV} (insecure; meant for scripts and tests)"
    ));
    Ok(Some(password))
}
