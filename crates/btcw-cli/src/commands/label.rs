//! `btcw label TXID TEXT` / `btcw label TXID --clear`. Watch-only and offline; shown by
//! `btcw history`.

use anyhow::Result;
use btcw_core::api;
use btcw_core::config::Config;
use serde::Serialize;

use crate::commands::status::parse_txid;
use crate::output::Ui;

#[derive(Debug, Serialize)]
struct LabelJson {
    network: String,
    txid: String,
    /// The label now stored (`null` after `--clear`).
    label: Option<String>,
}

/// `text`: the new label, or `None` to clear it.
pub fn run(cfg: &Config, ui: &Ui, txid: &str, text: Option<&str>) -> Result<()> {
    let txid = parse_txid(txid)?;
    let mut wallet = api::open_watch_only(cfg)?;
    let (label, had_one) = match text {
        Some(text) => (Some(wallet.set_label(txid, text)?), true),
        None => (None, wallet.clear_label(txid)?),
    };
    drop(wallet);

    if ui.json() {
        return ui.print_json(&LabelJson {
            network: cfg.network.to_string(),
            txid: txid.to_string(),
            label,
        });
    }
    let net = ui.out.badge(cfg.network);
    ui.println(&match (&label, had_one) {
        (Some(label), _) => format!("{net} Labelled {txid}: {}", ui.out.bold(label)),
        (None, true) => format!("{net} Removed the label of {txid}"),
        (None, false) => format!("{net} {txid} had no label"),
    })
}
