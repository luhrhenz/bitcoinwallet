//! Reading secrets: wallet passwords and the recovery phrase.
//!
//! | Secret | Interactive | Non-interactive |
//! |---|---|---|
//! | password | hidden prompt on the TTY (`rpassword` opens `/dev/tty`) | `BTCW_PASSWORD` (insecure, scripts and tests only) |
//! | recovery phrase | hidden prompt when stdin is a terminal | first line of stdin (`echo "<words>" \| btcw restore`) |
//!
//! Prompts go to the TTY, never stdout. Secrets go straight into `SecretString` / `Zeroizing`
//! buffers. [`Terminal`] reads confirmations from the TTY too, so a piped-in "y" can't approve
//! a payment.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};

use anyhow::{Context, Result, anyhow, bail};
use btcw_core::api;
use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret, SecretString};

use crate::output::Ui;

/// Wallet password for non-interactive use. The environment can leak (`/proc/<pid>/environ`,
/// child processes), so this is for demos and tests only.
pub const PASSWORD_ENV: &str = "BTCW_PASSWORD";

/// Longest stdin line accepted as a phrase (24 words fit in 215 bytes). Allocated up front so
/// reading never reallocates and leaves no unwiped copy.
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

/// The password of an existing wallet, to unlock it for signing. Not checked against the
/// new-password rules.
pub fn password(ui: &Ui) -> Result<SecretString> {
    if let Some(password) = password_from_env(ui)? {
        return Ok(password);
    }
    read_password("Wallet password: ")
}

/// The controlling terminal, for yes/no questions. Opened before any work starts, so a missing
/// terminal is reported first.
pub struct Terminal {
    input: BufReader<File>,
    output: File,
}

#[cfg(windows)]
const TTY_PATHS: (&str, &str) = ("CONIN$", "CONOUT$");
#[cfg(not(windows))]
const TTY_PATHS: (&str, &str) = ("/dev/tty", "/dev/tty");

impl Terminal {
    pub fn open() -> Result<Self> {
        let (input, output) = TTY_PATHS;
        let open = || -> std::io::Result<Self> {
            Ok(Self {
                input: BufReader::new(File::open(input)?),
                output: OpenOptions::new().write(true).open(output)?,
            })
        };
        open().map_err(|e| {
            anyhow!(
                "cannot open the terminal to confirm the payment ({e}); review the amounts and \
                 pass --yes to send without a confirmation prompt"
            )
        })
    }

    /// `question [y/N] `: true only for `y` or `yes` (any case). Anything else, including an
    /// empty line or end of input, is a no.
    pub fn confirm(&mut self, question: &str) -> Result<bool> {
        write!(self.output, "{question} [y/N] ")
            .and_then(|()| self.output.flush())
            .context("writing to the terminal")?;
        let mut answer = String::new();
        self.input
            .read_line(&mut answer)
            .context("reading the answer from the terminal")?;
        Ok(is_yes(&answer))
    }
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The recovery phrase for `restore`: a hidden prompt on a terminal, else the first line of
/// stdin. Never echoed.
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
    // `take` keeps a huge stdin from growing (and copying) the buffer.
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

/// One word of the recovery phrase for the backup check, typed hidden.
pub fn hidden_word(prompt: &str) -> Result<Zeroizing<String>> {
    rpassword::prompt_password(prompt)
        .map(Zeroizing::new)
        .map_err(|e| anyhow!("cannot read from the terminal: {e}"))
}

/// One hidden prompt on the terminal.
fn read_password(prompt: &str) -> Result<SecretString> {
    let typed = Zeroizing::new(rpassword::prompt_password(prompt).map_err(|e| {
        anyhow!(
            "cannot prompt for a password ({e}); for non-interactive use set {PASSWORD_ENV} \
             (insecure: meant for scripts and tests)"
        )
    })?);
    // Exactly-sized copy (no reallocation); `typed` is wiped on drop.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_yes_confirms() {
        for yes in ["y", "Y", "yes", "YES", " yes \n", "y\r\n"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "yep", "sure", "y y", "1"] {
            assert!(!is_yes(no), "{no:?}");
        }
    }
}
