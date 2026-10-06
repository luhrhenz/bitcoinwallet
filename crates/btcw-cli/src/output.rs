//! How output looks: the network badge, colors, amounts, dates, tables, `--json` values and
//! errors.
//!
//! Results go to stdout (exactly one JSON value with `--json`); prompts, warnings, progress
//! and logs go to stderr or the TTY, so `--json` output stays clean.

use std::fmt::Display;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use btcw_core::WalletError;
use btcw_core::bitcoin::Network;
use comfy_table::{CellAlignment, ContentArrangement, Table, modifiers, presets};
use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::{Style, Styled};
use serde::Serialize;

const SAT_PER_BTC: u64 = 100_000_000;

/// Error code for failures that don't come from the core (bad input, no terminal, ...).
pub const CLI_ERROR_CODE: &str = "cli";

/// Colors on one stream: on only when it's a terminal and `NO_COLOR` is unset (no-color.org).
#[derive(Debug, Clone, Copy)]
pub struct Painter {
    enabled: bool,
}

impl Painter {
    /// No colors, whatever the terminal supports.
    #[cfg(test)]
    pub fn plain() -> Self {
        Self { enabled: false }
    }

    pub fn enabled(self) -> bool {
        self.enabled
    }

    fn paint<T: Display>(self, style: Style, value: T) -> Styled<T> {
        // An empty `Style` writes no escape codes at all.
        let style = if self.enabled { style } else { Style::new() };
        style.style(value)
    }

    pub fn bold<T: Display>(self, value: T) -> Styled<T> {
        self.paint(Style::new().bold(), value)
    }

    pub fn dim<T: Display>(self, value: T) -> Styled<T> {
        self.paint(Style::new().dimmed(), value)
    }

    pub fn warning<T: Display>(self, value: T) -> Styled<T> {
        self.paint(Style::new().yellow().bold(), value)
    }

    pub fn error<T: Display>(self, value: T) -> Styled<T> {
        self.paint(Style::new().red().bold(), value)
    }

    /// `[testnet4]`, `[signet]`, `[regtest]`, `[mainnet]`: on every human-readable result so
    /// test coins are never mistaken for real ones.
    pub fn badge(self, network: Network) -> Styled<String> {
        let (name, style) = match network {
            Network::Bitcoin => ("mainnet".to_owned(), Style::new().red().bold()),
            Network::Testnet4 => ("testnet4".to_owned(), Style::new().yellow().bold()),
            Network::Signet => ("signet".to_owned(), Style::new().magenta().bold()),
            Network::Regtest => ("regtest".to_owned(), Style::new().cyan().bold()),
            other => (other.to_string(), Style::new().bold()),
        };
        self.paint(style, format!("[{name}]"))
    }
}

/// Output settings for one run, decided once at startup.
#[derive(Debug)]
pub struct Ui {
    json: bool,
    /// Colors for stdout.
    pub out: Painter,
    /// Colors for stderr.
    pub err: Painter,
    /// Progress bars only make sense for a person watching a terminal.
    progress: bool,
}

impl Ui {
    pub fn new(json: bool) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let dumb_term = std::env::var_os("TERM").is_some_and(|t| t == "dumb");
        let colors = |is_terminal: bool| Painter {
            enabled: is_terminal && !no_color && !dumb_term,
        };
        let stderr_is_terminal = std::io::stderr().is_terminal();
        Self {
            json,
            out: colors(std::io::stdout().is_terminal()),
            err: colors(stderr_is_terminal),
            progress: !json && stderr_is_terminal,
        }
    }

    pub fn json(&self) -> bool {
        self.json
    }

    /// Write `text` and a newline to stdout.
    pub fn println(&self, text: &str) -> Result<()> {
        let mut out = std::io::stdout().lock();
        out.write_all(text.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush())
            .context("writing to standard output")
    }

    /// The one JSON value a `--json` command prints.
    pub fn print_json<T: Serialize + ?Sized>(&self, value: &T) -> Result<()> {
        let text = serde_json::to_string_pretty(value).context("encoding JSON output")?;
        self.println(&text)
    }

    /// Write `text` and a newline to stderr.
    pub fn eprintln(&self, text: &str) -> std::io::Result<()> {
        let mut err = std::io::stderr().lock();
        err.write_all(text.as_bytes())?;
        err.write_all(b"\n")?;
        err.flush()
    }

    /// `warning: ...` on stderr, in both human and JSON mode.
    pub fn warn(&self, message: impl Display) {
        let _ = self.eprintln(&format!("{} {message}", self.err.warning("warning:")));
    }

    /// `note: ...` on stderr, in both human and JSON mode.
    pub fn note(&self, message: impl Display) {
        let _ = self.eprintln(&format!("{} {message}", self.err.bold("note:")));
    }

    /// Human mode: `error: <message>` on stderr. JSON mode: `{"error": {..}}` on stdout.
    pub fn report_error(&self, e: &anyhow::Error) {
        let message = format!("{e:#}");
        if self.json {
            let body = ErrorJson::new(error_code(e), message);
            if self.print_json(&body).is_ok() {
                return;
            }
            // stdout is gone; stderr is the only place left to say anything.
            let _ = self.eprintln(&format!("error: {}", body.error.message));
        } else {
            let _ = self.eprintln(&format!("{} {message}", self.err.error("error:")));
        }
    }

    /// A sync progress bar on stderr; hidden with `--json` or when stderr is not a terminal.
    pub fn sync_progress_bar(&self) -> ProgressBar {
        if !self.progress {
            return ProgressBar::hidden();
        }
        let template = if self.err.enabled {
            "{spinner:.cyan} Syncing [{bar:30.cyan/blue}] {msg} ({eta})"
        } else {
            "{spinner} Syncing [{bar:30}] {msg} ({eta})"
        };
        let style = ProgressStyle::with_template(template)
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> ");
        // Length 1 until the first block arrives: indicatif draws 0 of 0 as a full bar.
        let bar = ProgressBar::new(1).with_style(style);
        bar.set_message("waiting for bitcoind");
        bar.enable_steady_tick(Duration::from_millis(120));
        bar
    }
}

/// `{"error": {"code": "...", "message": "..."}}`
#[derive(Debug, Serialize)]
pub struct ErrorJson {
    pub error: ErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
}

impl ErrorJson {
    pub fn new(code: &'static str, message: String) -> Self {
        Self {
            error: ErrorBody { code, message },
        }
    }
}

impl ErrorBody {
    pub fn from_error(e: &anyhow::Error) -> Self {
        Self {
            code: error_code(e),
            message: format!("{e:#}"),
        }
    }
}

/// The core's stable code (`wallet_not_found`, `weak_password`, ...) when a `WalletError` is
/// anywhere in the chain, else [`CLI_ERROR_CODE`].
pub fn error_code(e: &anyhow::Error) -> &'static str {
    e.chain()
        .find_map(|cause| cause.downcast_ref::<WalletError>())
        .map_or(CLI_ERROR_CODE, WalletError::code)
}

/// clap failed to parse the command line. `--help`/`--version` print as usual; usage errors
/// exit with 2, as JSON with `--json`.
pub fn usage_error(e: &clap::Error) -> ExitCode {
    let wants_json = std::env::args_os()
        .skip(1)
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--json");
    if !(e.use_stderr() && wants_json) {
        e.exit();
    }
    // `render()` without ANSI styling, minus clap's own "error: " prefix.
    let rendered = e.render().to_string();
    let message = rendered.trim().trim_start_matches("error: ").to_owned();
    let body = ErrorJson::new(CLI_ERROR_CODE, message);
    if let Ok(text) = serde_json::to_string_pretty(&body) {
        println!("{text}");
    }
    ExitCode::from(2)
}

/// `1234567` → `"1,234,567"`
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1_000_000` sat → `"0.01000000"` (always 8 decimals; integer math, no floats).
pub fn btc(sat: u64) -> String {
    format!("{}.{:08}", sat / SAT_PER_BTC, sat % SAT_PER_BTC)
}

/// `"0.01000000 BTC (1,000,000 sat)"`
pub fn amount(sat: u64) -> String {
    format!("{} BTC ({} sat)", btc(sat), group_thousands(sat))
}

/// `"+0.01000000 BTC (+1,000,000 sat)"`, `"-0.00002000 BTC (-2,000 sat)"`, zero unsigned.
pub fn signed_amount(sat: i64) -> String {
    let sign = match sat.signum() {
        1 => "+",
        -1 => "-",
        _ => "",
    };
    let magnitude = sat.unsigned_abs();
    format!(
        "{sign}{} BTC ({sign}{} sat)",
        btc(magnitude),
        group_thousands(magnitude)
    )
}

/// `"tb1qw508d6qe…"` → `"tb1q w508 d6qe …"`: the full address in groups of four. Never
/// shortened: clipboard-swapping malware changes the middle.
pub fn grouped(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    for (i, c) in text.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Unix seconds → `"2026-10-03 14:05"` (UTC).
pub fn utc_datetime(unix_secs: u64) -> String {
    let (days, secs_of_day) = (unix_secs / 86_400, unix_secs % 86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        secs_of_day / 3_600,
        secs_of_day % 3_600 / 60
    )
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's `civil_from_days`, for
/// non-negative days only.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    // Shift the epoch to 0000-03-01 so the leap day is the last day of a "year".
    let z = days + 719_468;
    let era = z / 146_097; // 400-year cycles
    let doe = z - era * 146_097; // day of era, [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // year of era, [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year from March 1, [0, 365]
    let mp = (5 * doy + 2) / 153; // month from March, [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// `as of block 203 (run `btcw sync` to update)`: offline views say how fresh they are.
pub fn as_of(synced_height: u32) -> String {
    if synced_height == 0 {
        "(not synced yet; run `btcw sync`)".to_owned()
    } else {
        format!("as of block {synced_height} (run `btcw sync` to update)")
    }
}

/// `1 block`, `2 blocks`
pub fn plural(n: u32, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// A table with our house style; `right` lists the columns to right-align (amounts).
pub fn table(header: &[&str], right: &[usize]) -> Table {
    let mut table = Table::new();
    table
        .load_preset(presets::UTF8_FULL_CONDENSED)
        .apply_modifier(modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(header.to_vec());
    for &index in right {
        if let Some(column) = table.column_mut(index) {
            column.set_cell_alignment(CellAlignment::Right);
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(1_000_000), "1,000,000");
        assert_eq!(group_thousands(12_345_678), "12,345,678");
        assert_eq!(group_thousands(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn sat_to_btc_strings() {
        assert_eq!(btc(0), "0.00000000");
        assert_eq!(btc(1), "0.00000001");
        assert_eq!(btc(1_000_000), "0.01000000");
        assert_eq!(btc(5_000_000_000), "50.00000000");
        assert_eq!(btc(2_100_000_000_000_000), "21000000.00000000");
        assert_eq!(amount(1_000_000), "0.01000000 BTC (1,000,000 sat)");
        assert_eq!(amount(0), "0.00000000 BTC (0 sat)");
    }

    #[test]
    fn signed_amounts() {
        assert_eq!(signed_amount(1_000_000), "+0.01000000 BTC (+1,000,000 sat)");
        assert_eq!(signed_amount(-2_141), "-0.00002141 BTC (-2,141 sat)");
        assert_eq!(signed_amount(0), "0.00000000 BTC (0 sat)");
        // `unsigned_abs` handles the one value whose negation overflows.
        assert_eq!(
            signed_amount(i64::MIN),
            "-92233720368.54775808 BTC (-9,223,372,036,854,775,808 sat)"
        );
    }

    #[test]
    fn addresses_in_groups_of_four() {
        assert_eq!(
            grouped("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx"),
            "tb1q w508 d6qe jxtd g4y5 r3za rvar y0c5 xw7k xpjz sx"
        );
        assert_eq!(grouped("bcrt1q6rz2"), "bcrt 1q6r z2");
        assert_eq!(grouped("abcd"), "abcd");
        assert_eq!(grouped(""), "");
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc_datetime(0), "1970-01-01 00:00");
        assert_eq!(utc_datetime(951_782_400), "2000-02-29 00:00"); // leap day (400-year rule)
        assert_eq!(utc_datetime(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(utc_datetime(4_102_444_799), "2099-12-31 23:59");
        assert_eq!(utc_datetime(4_107_542_400), "2100-03-01 00:00"); // 2100 is not a leap year
        // No overflow even for absurd input (a corrupt timestamp must not crash `history`).
        let far = utc_datetime(u64::MAX);
        assert!(far.starts_with("5845540"), "{far}");
    }

    #[test]
    fn error_json_shape() -> Result<()> {
        let core = anyhow::Error::from(WalletError::WeakPassword(8));
        assert_eq!(error_code(&core), "weak_password");
        let value = serde_json::to_value(ErrorJson::new(error_code(&core), format!("{core:#}")))?;
        assert_eq!(
            value,
            serde_json::json!({
                "error": { "code": "weak_password", "message": "password must be at least 8 characters" }
            })
        );

        // Context on top of a core error keeps the core's code.
        let wrapped = anyhow::Error::from(WalletError::WalletInUse).context("opening the wallet");
        assert_eq!(error_code(&wrapped), "wallet_in_use");
        let body = ErrorBody::from_error(&wrapped);
        assert_eq!(
            body.message,
            "opening the wallet: wallet is open in another btcw process"
        );

        let plain = anyhow::anyhow!("not implemented yet (Phase 2)");
        assert_eq!(error_code(&plain), CLI_ERROR_CODE);
        Ok(())
    }

    #[test]
    fn badge_is_plain_without_color() {
        let plain = Painter { enabled: false };
        assert_eq!(plain.badge(Network::Regtest).to_string(), "[regtest]");
        assert_eq!(plain.badge(Network::Testnet4).to_string(), "[testnet4]");
        assert_eq!(plain.badge(Network::Bitcoin).to_string(), "[mainnet]");
        let colored = Painter { enabled: true };
        let badge = colored.badge(Network::Signet).to_string();
        assert!(
            badge.starts_with('\u{1b}') && badge.contains("[signet]"),
            "{badge:?}"
        );
    }

    #[test]
    fn freshness_note() {
        assert_eq!(as_of(0), "(not synced yet; run `btcw sync`)");
        assert_eq!(as_of(203), "as of block 203 (run `btcw sync` to update)");
        assert_eq!(plural(1, "block", "blocks"), "1 block");
        assert_eq!(plural(0, "block", "blocks"), "0 blocks");
    }
}
