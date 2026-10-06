//! `btcw status TXID [--watch]`: where one of the wallet's transactions is (PLAN §5.9).
//!
//! Watch-only, no password. Every check is a full open → sync → read → **close**: the wallet
//! (and its lock file) is released between checks, so another `btcw` command or the desktop app
//! can use it while `--watch` sleeps, and Ctrl-C during the sleep can't interrupt a write.
//!
//! When bitcoind can't be reached, a single check still answers from the wallet database (as of
//! the last sync, with a warning); `--watch` warns and keeps trying.

use std::time::Duration;

use anyhow::{Result, anyhow};
use btcw_core::WalletError;
use btcw_core::api;
use btcw_core::bitcoin::Txid;
use btcw_core::chain::Node;
use btcw_core::config::Config;
use btcw_core::tx;
use btcw_core::types::TxStatus;
use serde::Serialize;

use crate::commands::sync::sync_with_progress;
use crate::output::{ErrorBody, Ui, error_code, plural, utc_datetime};

/// Longest piece of the user's input echoed back in the "not a txid" error.
const MAX_ECHOED_INPUT: usize = 80;

#[derive(Debug, Serialize)]
struct StatusJson {
    network: String,
    txid: String,
    /// The wallet's tip when the status was read; confirmations count up to it.
    synced_height: u32,
    status: TxStatus,
    /// Set when bitcoind could not be reached (or the sync failed): `status` is then as of the
    /// last successful sync.
    sync_error: Option<ErrorBody>,
}

/// `--watch` settings.
#[derive(Debug, Clone, Copy)]
pub struct Watch {
    pub interval: Duration,
    /// Stop once the transaction has at least this many confirmations.
    pub until: u32,
}

pub fn run(cfg: &Config, ui: &Ui, txid: &str, watch: Option<Watch>) -> Result<()> {
    let txid = parse_txid(txid)?;
    match watch {
        None => check_once(cfg, ui, txid),
        Some(watch) => watch_until(cfg, ui, txid, watch),
    }
}

/// One look at the transaction.
struct Check {
    status: Option<TxStatus>,
    synced_height: u32,
    sync_error: Option<anyhow::Error>,
}

/// Open the wallet, sync it (if bitcoind answers), read the status, close the wallet.
fn check(cfg: &Config, ui: &Ui, txid: Txid, show_progress: bool) -> Result<Check> {
    let mut wallet = api::open_watch_only(cfg)?;
    let synced = Node::connect(&cfg.rpc, cfg.network).and_then(|node| {
        if show_progress {
            sync_with_progress(ui, &node, &mut wallet)
        } else {
            node.sync(&mut wallet, &mut |_| {})
        }
    });
    let status = tx::tx_status(&wallet, txid);
    let synced_height = wallet.synced_height();
    drop(wallet);
    Ok(Check {
        status,
        synced_height,
        sync_error: synced.err().map(anyhow::Error::from),
    })
}

fn check_once(cfg: &Config, ui: &Ui, txid: Txid) -> Result<()> {
    let Check {
        status,
        synced_height,
        sync_error,
    } = check(cfg, ui, txid, true)?;
    if let Some(e) = &sync_error {
        ui.warn(format_args!(
            "could not sync with bitcoind, so this is the status as of block {synced_height} \
             (the last sync): {e:#}"
        ));
    }
    let Some(status) = status else {
        return Err(not_found(ui, txid));
    };
    if ui.json() {
        return print_json(cfg, ui, txid, synced_height, sync_error.as_ref(), status);
    }
    let offline = if sync_error.is_some() {
        " (bitcoind unreachable; may be out of date)"
    } else {
        ""
    };
    ui.println(&format!(
        "{} Transaction {txid}\n{}\n  {:<12}block {synced_height}{offline}",
        ui.out.badge(cfg.network),
        describe(&status),
        "As of",
    ))
}

/// Poll every `interval` until the transaction has `until` confirmations. Prints a line
/// whenever something changes (a new block, or the status), so a quiet minute stays quiet.
fn watch_until(cfg: &Config, ui: &Ui, txid: Txid, watch: Watch) -> Result<()> {
    let every = watch.interval.as_secs();
    if !ui.json() {
        ui.println(&format!(
            "{} Watching {txid} until it has {} (checking every {every} s; Ctrl-C to stop)",
            ui.out.badge(cfg.network),
            plural(watch.until, "confirmation", "confirmations"),
        ))?;
    }
    // Compared as printed text, not as `TxStatus`: `first_seen` is not stable across reopens
    // (bdk_chain 0.23 restores it from the persisted *last*-seen time), and it isn't shown here.
    let mut last_line = String::new();
    let mut first = true;
    loop {
        // Only the first check can have many blocks to catch up on; later ones would just
        // flash a progress bar every few seconds.
        match check(cfg, ui, txid, first) {
            Ok(Check {
                status,
                synced_height,
                sync_error,
            }) => {
                if let Some(e) = &sync_error {
                    ui.warn(format_args!(
                        "could not sync with bitcoind; trying again in {every} s: {e:#}"
                    ));
                }
                match status {
                    // Synced and still unknown: it isn't (or is no longer) this wallet's.
                    None if sync_error.is_none() => return Err(not_found(ui, txid)),
                    // Not known as of the last sync, and no node to ask: keep trying.
                    None => {}
                    Some(status) => {
                        let line = format!("  block {synced_height:<8} {}", summary(&status));
                        if !ui.json() && line != last_line {
                            ui.println(&line)?;
                        }
                        last_line = line;
                        // Confirmations only ever undercount (a stale tip), so reaching the
                        // target is final even when this check couldn't sync.
                        if confirmations(&status) >= watch.until {
                            return if ui.json() {
                                print_json(
                                    cfg,
                                    ui,
                                    txid,
                                    synced_height,
                                    sync_error.as_ref(),
                                    status,
                                )
                            } else {
                                Ok(())
                            };
                        }
                    }
                }
            }
            // Another btcw command or the desktop app has the wallet open right now. It will
            // close it again; this is exactly why we don't hold it between checks either.
            Err(e) if error_code(&e) == WalletError::WalletInUse.code() => {
                ui.warn(format_args!("{e:#}; trying again in {every} s"));
            }
            Err(e) => return Err(e),
        }
        first = false;
        std::thread::sleep(watch.interval);
    }
}

fn print_json(
    cfg: &Config,
    ui: &Ui,
    txid: Txid,
    synced_height: u32,
    sync_error: Option<&anyhow::Error>,
    status: TxStatus,
) -> Result<()> {
    ui.print_json(&StatusJson {
        network: cfg.network.to_string(),
        txid: txid.to_string(),
        synced_height,
        status,
        sync_error: sync_error.map(ErrorBody::from_error),
    })
}

/// `WalletError::TxNotFound`, after a note on what `status` can see.
fn not_found(ui: &Ui, txid: Txid) -> anyhow::Error {
    ui.note(
        "`btcw status` follows this wallet's own transactions; one that was replaced or dropped \
         from the mempool disappears from the wallet too",
    );
    WalletError::TxNotFound(txid.to_string()).into()
}

/// A txid from the command line, with an error that says what one looks like.
pub(crate) fn parse_txid(input: &str) -> Result<Txid> {
    let trimmed = input.trim();
    trimmed.parse().map_err(|e| {
        let shown: String = trimmed.chars().take(MAX_ECHOED_INPUT).collect();
        let ellipsis = if shown.len() < trimmed.len() {
            "…"
        } else {
            ""
        };
        anyhow!(
            "invalid transaction id `{shown}{ellipsis}` ({e}); expected 64 hexadecimal \
             characters, as printed by `btcw send` and `btcw history`"
        )
    })
}

fn confirmations(status: &TxStatus) -> u32 {
    match status {
        TxStatus::Unconfirmed { .. } => 0,
        TxStatus::Confirmed { confirmations, .. } => *confirmations,
    }
}

/// One line, for `--watch`.
fn summary(status: &TxStatus) -> String {
    match status {
        TxStatus::Unconfirmed { .. } => "unconfirmed, waiting in the mempool".to_owned(),
        TxStatus::Confirmed {
            height,
            confirmations,
            ..
        } => format!(
            "confirmed in block {height}: {}",
            plural(*confirmations, "confirmation", "confirmations")
        ),
    }
}

/// The detail lines of a single check.
fn describe(status: &TxStatus) -> String {
    match status {
        TxStatus::Unconfirmed { first_seen } => format!(
            "  {:<12}unconfirmed, waiting in the mempool\n  {:<12}{}",
            "Status",
            "First seen",
            first_seen.map_or_else(
                || "unknown".to_owned(),
                |t| format!("{} UTC", utc_datetime(t))
            )
        ),
        TxStatus::Confirmed {
            height,
            confirmations,
            block_time,
        } => format!(
            "  {:<12}confirmed, {}\n  {:<12}{height}, mined {} UTC",
            "Status",
            plural(*confirmations, "confirmation", "confirmations"),
            "Block",
            utc_datetime(*block_time)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txids_are_64_hex_characters() -> Result<()> {
        let txid = "5e3c1f1d2a6b7c8d9e0f11223344556677889900aabbccddeeff001122334455";
        assert_eq!(parse_txid(&format!(" {txid}\n"))?.to_string(), txid);
        for bad in [
            "",
            "xyz",
            &txid[1..],
            &format!("{txid}00"),
            &txid.replace('5', "g"),
        ] {
            let msg = format!(
                "{:#}",
                parse_txid(bad).err().ok_or_else(|| anyhow!("accepted"))?
            );
            assert!(msg.starts_with("invalid transaction id `"), "{msg}");
            assert!(msg.contains("expected 64 hexadecimal characters"), "{msg}");
        }
        let msg = format!(
            "{:#}",
            parse_txid(&"z".repeat(1_000))
                .err()
                .ok_or_else(|| anyhow!("accepted"))?
        );
        assert!(msg.len() < 300 && msg.contains('…'), "{msg}");
        Ok(())
    }

    #[test]
    fn status_wording() {
        let unconfirmed = TxStatus::Unconfirmed {
            first_seen: Some(1_700_000_000),
        };
        assert_eq!(
            describe(&unconfirmed),
            "  Status      unconfirmed, waiting in the mempool\n  First seen  2023-11-14 22:13 UTC"
        );
        assert_eq!(
            describe(&TxStatus::Unconfirmed { first_seen: None }),
            "  Status      unconfirmed, waiting in the mempool\n  First seen  unknown"
        );
        assert_eq!(summary(&unconfirmed), "unconfirmed, waiting in the mempool");
        assert_eq!(confirmations(&unconfirmed), 0);

        let confirmed = TxStatus::Confirmed {
            height: 204,
            confirmations: 1,
            block_time: 1_700_000_600,
        };
        assert_eq!(
            describe(&confirmed),
            "  Status      confirmed, 1 confirmation\n  Block       204, mined 2023-11-14 22:23 UTC"
        );
        assert_eq!(
            summary(&confirmed),
            "confirmed in block 204: 1 confirmation"
        );
        assert_eq!(confirmations(&confirmed), 1);
    }
}
