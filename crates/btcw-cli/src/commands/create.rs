//! `btcw create`: new wallet, recovery phrase shown exactly once (PLAN §5.1).

use std::fmt::{self, Write as _};
use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Result, anyhow};
use btcw_core::WalletError;
use btcw_core::api::{self, CreatedWallet, Unlocked};
use btcw_core::bitcoin::Network;
use btcw_core::chain::Node;
use btcw_core::config::Config;
use btcw_core::keys::WordCount;
use btcw_core::types::AddressRow;
use secrecy::zeroize::Zeroizing;
use serde::Serialize;

use crate::output::{Painter, Ui};
use crate::prompt;

/// `--json` result. Deliberately has no phrase field: the phrase goes to stderr, so it can't
/// end up in a file or log that only expected machine-readable output.
#[derive(Debug, Serialize)]
struct CreateJson {
    network: String,
    birthday_height: u32,
    first_address: String,
}

/// Big enough for a 24-word grid, the warning, the other lines and escape codes (plus the
/// wallet path, added at runtime), so the buffer holding the phrase never reallocates, which
/// would leave an unwiped copy behind.
const PHRASE_TEXT_CAPACITY: usize = 4096;

pub fn run(cfg: &Config, ui: &Ui, words: WordCount) -> Result<()> {
    // Checked again (under the lock) by the core; this just avoids asking for a password first.
    if api::wallet_exists(cfg) {
        return Err(WalletError::WalletExists(cfg.network_dir()).into());
    }
    let password = prompt::new_password(ui)?;

    // A new wallet can't have older transactions, so its first sync can start at today's tip.
    let birthday = match current_tip(cfg) {
        Ok(height) => Some(height),
        Err(e) => {
            ui.warn(format_args!(
                "could not read the current block height from bitcoind, so the wallet's first \
                 sync will scan from the genesis block (slow on testnet4/signet): {e}"
            ));
            None
        }
    };

    let CreatedWallet {
        mnemonic,
        unlocked: Unlocked { mut wallet, signer },
    } = api::create_wallet(cfg, words, &password, birthday)?;
    drop(password);
    // Creating needs no signing; wipe the master key now rather than at the end.
    drop(signer);
    let first_address = wallet.new_address();
    let birthday_height = wallet.birthday_height();
    drop(wallet);

    // The wallet exists now and this is the only time its phrase can be shown, so it is
    // rendered and printed before anything else can fail.
    let network_dir = cfg.network_dir();
    let mut text = Zeroizing::new(String::with_capacity(
        PHRASE_TEXT_CAPACITY + network_dir.as_os_str().len(),
    ));
    let phrase = mnemonic.phrase();
    drop(mnemonic);
    let shown = if ui.json() {
        // JSON mode: the phrase goes to stderr; stdout gets only the JSON below.
        write_phrase(&mut text, &phrase, ui.err)
            .and_then(|()| write!(text, "\n\nNext: check your copy with `btcw backup verify`."))
            .map_err(|e| not_shown(&network_dir, &e.to_string()))
            .and_then(|()| {
                ui.eprintln(&text)
                    .map_err(|e| not_shown(&network_dir, &e.to_string()))
            })
    } else {
        let human = Human {
            paint: ui.out,
            network: cfg.network,
            network_dir: &network_dir,
            first_address: first_address.as_ref().ok(),
            birthday: birthday.map(|_| birthday_height),
        };
        human
            .write(&mut text, &phrase)
            .map_err(|e| not_shown(&network_dir, &e.to_string()))
            .and_then(|()| {
                ui.println(&text)
                    .map_err(|e| not_shown(&network_dir, &format!("{e:#}")))
            })
    };
    drop(phrase);
    drop(text);
    shown?;

    let first_address = first_address?;
    if ui.json() {
        return ui.print_json(&CreateJson {
            network: cfg.network.to_string(),
            birthday_height,
            first_address: first_address.address,
        });
    }
    if !std::io::stdout().is_terminal() {
        ui.warn(
            "standard output is not a terminal, so the recovery phrase was written to wherever \
             it is redirected; delete that copy once the words are written down",
        );
    }
    Ok(())
}

fn current_tip(cfg: &Config) -> btcw_core::Result<u32> {
    Node::connect(&cfg.rpc, cfg.network)?.tip_height()
}

/// Everything `create` prints in human mode, phrase included.
struct Human<'a> {
    paint: Painter,
    network: Network,
    network_dir: &'a Path,
    /// `None` if revealing it failed; the error is reported after the phrase is shown.
    first_address: Option<&'a AddressRow>,
    /// `None` when the node was unreachable (the wallet then scans from genesis).
    birthday: Option<u32>,
}

impl Human<'_> {
    fn write(&self, text: &mut String, phrase: &str) -> fmt::Result {
        let paint = self.paint;
        writeln!(
            text,
            "{} Created a new wallet in {}\n",
            paint.badge(self.network),
            self.network_dir.display()
        )?;
        write_phrase(text, phrase, paint)?;
        writeln!(text, "\n")?;
        if let Some(row) = self.first_address {
            writeln!(
                text,
                "First receive address (#{}): {}",
                row.index,
                paint.bold(&row.address)
            )?;
        }
        match self.birthday {
            Some(height) => writeln!(
                text,
                "Wallet birthday: block {height} (the first sync starts there)"
            )?,
            None => writeln!(
                text,
                "Wallet birthday: genesis (the node was unreachable; the first sync scans the \
                 whole chain)"
            )?,
        }
        write!(
            text,
            "Next: check your copy with `btcw backup verify`, then `btcw sync` and `btcw balance`."
        )
    }
}

/// The numbered grid plus the warning. Row-major, 4 words per row: `1. word   2. word ...`.
/// Written straight into `text` so no other buffer ever holds the words.
pub(crate) fn write_phrase(text: &mut String, phrase: &str, paint: Painter) -> fmt::Result {
    let words: Vec<&str> = phrase.split_whitespace().collect();
    writeln!(
        text,
        "{}\n",
        paint.bold(format_args!(
            "Recovery phrase ({} words). Write them down on paper, in this order:",
            words.len()
        ))
    )?;
    for (row, chunk) in words.chunks(4).enumerate() {
        text.push_str("  ");
        for (column, word) in chunk.iter().enumerate() {
            let number = row * 4 + column + 1;
            // BIP39 English words are at most 8 letters.
            write!(text, "{number:>4}. {word:<9}")?;
        }
        // Drop the padding after the last word without allocating a trimmed copy.
        let trimmed = text.trim_end_matches(' ').len();
        text.truncate(trimmed);
        text.push('\n');
    }
    writeln!(text)?;
    writeln!(
        text,
        "{} Anyone with these words can take your coins. Keep them offline and private;",
        paint.warning("WARNING:")
    )?;
    writeln!(
        text,
        "         never type them into a website or keep a photo of them."
    )?;
    write!(
        text,
        "         They are the only backup: lose them and this computer, and the coins are gone.\n         \
         (`btcw backup show` displays them again; it needs your password.)"
    )
}

/// Printing the phrase failed after the wallet was created.
fn not_shown(network_dir: &Path, reason: &str) -> anyhow::Error {
    anyhow!(
        "the wallet was created but its recovery phrase could not be displayed ({reason}); \
         nothing has been received yet, so delete {} and run `btcw create` again",
        network_dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Twelve words in a known order (a formatting test; the checksum doesn't matter here).
    const WORDS: &str =
        "abandon ability able about above absent absorb abstract absurd abuse access accident";

    #[test]
    fn phrase_grid_numbers_every_word_once() -> fmt::Result {
        let mut text = String::new();
        write_phrase(&mut text, WORDS, Painter::plain())?;
        let grid: Vec<&str> = text
            .lines()
            .filter(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
            .collect();
        assert_eq!(
            grid,
            vec![
                "     1. abandon     2. ability     3. able        4. about",
                "     5. above       6. absent      7. absorb      8. abstract",
                "     9. absurd     10. abuse      11. access     12. accident",
            ]
        );
        assert!(text.contains("Recovery phrase (12 words)"));
        assert!(text.contains("Anyone with these words can take your coins"));
        assert!(text.contains("btcw backup show"));
        Ok(())
    }
}
