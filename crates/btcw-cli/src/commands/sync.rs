//! `btcw sync`: pull new blocks and mempool transactions from bitcoind.

use anyhow::Result;
use btcw_core::api;
use btcw_core::chain::Node;
use btcw_core::config::Config;
use btcw_core::types::{SyncProgress, SyncReport};
use btcw_core::wallet::WalletService;
use serde::Serialize;

use crate::output::{Ui, plural};

#[derive(Debug, Serialize)]
struct SyncJson {
    network: String,
    #[serde(flatten)]
    report: SyncReport,
}

pub fn run(cfg: &Config, ui: &Ui) -> Result<()> {
    // Open the wallet first: "no wallet here" is a better first error than "no node".
    let mut wallet = api::open_watch_only(cfg)?;
    let node = Node::connect(&cfg.rpc, cfg.network)?;
    let report = sync_with_progress(ui, &node, &mut wallet)?;
    drop(wallet);

    if ui.json() {
        return ui.print_json(&SyncJson {
            network: cfg.network.to_string(),
            report,
        });
    }
    ui.println(&format!(
        "{} {}",
        ui.out.badge(cfg.network),
        describe(&report)
    ))
}

/// `Node::sync` with a progress bar on stderr counting the blocks this sync downloads.
pub fn sync_with_progress(
    ui: &Ui,
    node: &Node,
    wallet: &mut WalletService,
) -> btcw_core::Result<SyncReport> {
    // Mirrors where `Node::sync` starts: genesis itself is never fetched.
    let first = wallet
        .synced_height()
        .saturating_add(1)
        .max(wallet.birthday_height());
    let base = u64::from(first.saturating_sub(1));

    let bar = ui.sync_progress_bar();
    let mut on_progress = |p: SyncProgress| {
        bar.set_length(u64::from(p.tip_height).saturating_sub(base));
        bar.set_position(u64::from(p.height).saturating_sub(base));
        bar.set_message(format!("block {} of {}", p.height, p.tip_height));
    };
    let result = node.sync(wallet, &mut on_progress);
    bar.finish_and_clear();
    result
}

/// `Synced to block 203: scanned 2 blocks; 1 unconfirmed wallet transaction.`
pub fn describe(report: &SyncReport) -> String {
    let pending = match report.mempool_txs {
        0 => "no unconfirmed wallet transactions".to_owned(),
        n => format!(
            "{} waiting in the mempool",
            plural(
                n,
                "unconfirmed wallet transaction",
                "unconfirmed wallet transactions"
            )
        ),
    };
    format!(
        "Synced to block {}: scanned {}; {pending}.",
        report.tip_height,
        plural(report.blocks_scanned, "block", "blocks")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_summary() {
        let report = SyncReport {
            tip_height: 203,
            blocks_scanned: 1,
            mempool_txs: 0,
        };
        assert_eq!(
            describe(&report),
            "Synced to block 203: scanned 1 block; no unconfirmed wallet transactions."
        );
        let report = SyncReport {
            tip_height: 210,
            blocks_scanned: 7,
            mempool_txs: 2,
        };
        assert_eq!(
            describe(&report),
            "Synced to block 210: scanned 7 blocks; 2 unconfirmed wallet transactions waiting in the mempool."
        );
    }
}
