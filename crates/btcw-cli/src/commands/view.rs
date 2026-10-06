//! `btcw balance`, `btcw history`, `btcw utxos` (PLAN §5.6–5.7). `history` shows transaction
//! labels (`btcw label`) in a last column, when at least one transaction has one.
//!
//! All three are watch-only and offline: they read the wallet's SQLite file as of the last
//! `btcw sync`, which is why each one says which block its numbers are from.

use anyhow::Result;
use btcw_core::api;
use btcw_core::config::Config;
use btcw_core::types::{BalanceView, Keychain, TxRow, TxStatus, UtxoRow};
use serde::Serialize;

use crate::output::{
    Painter, Ui, amount, as_of, btc, group_thousands, plural, signed_amount, table, utc_datetime,
};

#[derive(Debug, Serialize)]
struct BalanceJson {
    network: String,
    synced_height: u32,
    balance: BalanceView,
}

#[derive(Debug, Serialize)]
struct HistoryJson {
    network: String,
    synced_height: u32,
    transactions: Vec<TxRow>,
}

#[derive(Debug, Serialize)]
struct UtxosJson {
    network: String,
    synced_height: u32,
    utxos: Vec<UtxoRow>,
}

pub fn balance(cfg: &Config, ui: &Ui) -> Result<()> {
    let wallet = api::open_watch_only(cfg)?;
    let balance = wallet.balance();
    let synced_height = wallet.synced_height();
    drop(wallet);

    if ui.json() {
        return ui.print_json(&BalanceJson {
            network: cfg.network.to_string(),
            synced_height,
            balance,
        });
    }
    ui.println(&format!(
        "{} Balance {}\n{}",
        ui.out.badge(cfg.network),
        as_of(synced_height),
        balance_lines(&balance, ui.out)
    ))
}

/// ```text
///   Confirmed       0.01000000 BTC (1,000,000 sat)
///   Unconfirmed     0.00000000 BTC (0 sat)
///   Immature       50.00002820 BTC (5,000,002,820 sat)  mined; spendable after 100 confirmations
///   Total          50.01002820 BTC (5,001,002,820 sat)
/// ```
/// Immature is only listed when there is some (it only happens to miners, i.e. on regtest).
pub fn balance_lines(balance: &BalanceView, paint: Painter) -> String {
    let mut rows = vec![
        ("Confirmed", balance.confirmed_sat, ""),
        ("Unconfirmed", balance.unconfirmed_sat, ""),
    ];
    if balance.immature_sat > 0 {
        rows.push((
            "Immature",
            balance.immature_sat,
            "  mined; spendable after 100 confirmations",
        ));
    }
    let width = rows
        .iter()
        .chain(std::iter::once(&("Total", balance.total_sat, "")))
        .map(|(_, sat, _)| btc(*sat).len())
        .max()
        .unwrap_or(0);
    let line = |label: &str, sat: u64| {
        format!(
            "  {label:<12}{:>width$} BTC ({} sat)",
            btc(sat),
            group_thousands(sat)
        )
    };
    let mut lines: Vec<String> = rows
        .iter()
        .map(|(label, sat, note)| match *note {
            "" => line(label, *sat),
            note => format!("{}{}", line(label, *sat), paint.dim(note)),
        })
        .collect();
    lines.push(paint.bold(line("Total", balance.total_sat)).to_string());
    lines.join("\n")
}

pub fn history(cfg: &Config, ui: &Ui) -> Result<()> {
    let wallet = api::open_watch_only(cfg)?;
    let transactions = wallet.history();
    let synced_height = wallet.synced_height();
    drop(wallet);

    if ui.json() {
        return ui.print_json(&HistoryJson {
            network: cfg.network.to_string(),
            synced_height,
            transactions,
        });
    }
    let net = ui.out.badge(cfg.network);
    if transactions.is_empty() {
        return ui.println(&format!(
            "{net} No transactions yet {}",
            as_of(synced_height)
        ));
    }
    // Without any label, the table looks exactly as it did before labels existed.
    let labelled = transactions.iter().any(|tx| tx.label.is_some());
    let mut header = vec!["Date (UTC)", "Type", "Amount", "Fee", "Status", "Txid"];
    if labelled {
        header.push("Label");
    }
    let mut rows = table(&header, &[2, 3]);
    for tx in &transactions {
        let (date, status) = match tx.status {
            TxStatus::Confirmed {
                confirmations,
                block_time,
                ..
            } => (
                utc_datetime(block_time),
                plural(confirmations, "confirmation", "confirmations"),
            ),
            // Unconfirmed: the date is when our node first saw it in its mempool.
            TxStatus::Unconfirmed { first_seen } => (
                first_seen.map_or_else(|| "—".to_owned(), utc_datetime),
                "unconfirmed".to_owned(),
            ),
        };
        let mut row = vec![
            date,
            direction(tx.net_sat).to_owned(),
            signed_amount(tx.net_sat),
            // Unknown for incoming payments: the sender's inputs aren't ours.
            tx.fee_sat.map_or_else(
                || "—".to_owned(),
                |fee| format!("{} sat", group_thousands(fee)),
            ),
            status,
            tx.txid.clone(),
        ];
        if labelled {
            row.push(tx.label.clone().unwrap_or_default());
        }
        rows.add_row(row);
    }
    ui.println(&format!(
        "{net} {} {}\n{rows}",
        plural(
            u32::try_from(transactions.len()).unwrap_or(u32::MAX),
            "transaction",
            "transactions"
        ),
        as_of(synced_height)
    ))
}

/// From the wallet's point of view: did this transaction add or remove coins?
fn direction(net_sat: i64) -> &'static str {
    match net_sat.signum() {
        1 => "received",
        -1 => "sent",
        _ => "self",
    }
}

pub fn utxos(cfg: &Config, ui: &Ui) -> Result<()> {
    let wallet = api::open_watch_only(cfg)?;
    let utxos = wallet.utxos();
    let synced_height = wallet.synced_height();
    drop(wallet);

    if ui.json() {
        return ui.print_json(&UtxosJson {
            network: cfg.network.to_string(),
            synced_height,
            utxos,
        });
    }
    let net = ui.out.badge(cfg.network);
    if utxos.is_empty() {
        return ui.println(&format!(
            "{net} No unspent outputs {}",
            as_of(synced_height)
        ));
    }
    let mut rows = table(&["Outpoint", "Amount", "Address", "Key", "Status"], &[1]);
    let mut total: u64 = 0;
    for utxo in &utxos {
        total = total.saturating_add(utxo.value_sat);
        let keychain = match utxo.keychain {
            Keychain::External => "receive",
            Keychain::Internal => "change",
        };
        rows.add_row(vec![
            utxo.outpoint.clone(),
            amount(utxo.value_sat),
            utxo.address.clone().unwrap_or_else(|| "—".to_owned()),
            format!("{keychain} #{}", utxo.derivation_index),
            if utxo.confirmed {
                "confirmed"
            } else {
                "unconfirmed"
            }
            .to_owned(),
        ]);
    }
    ui.println(&format!(
        "{net} Unspent outputs {}\n{rows}\nTotal: {} in {}",
        as_of(synced_height),
        amount(total),
        plural(
            u32::try_from(utxos.len()).unwrap_or(u32::MAX),
            "output",
            "outputs"
        )
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_lines_align_and_hide_zero_immature() {
        let balance = BalanceView {
            confirmed_sat: 1_000_000,
            unconfirmed_sat: 0,
            immature_sat: 0,
            total_sat: 1_000_000,
        };
        assert_eq!(
            balance_lines(&balance, Painter::plain()),
            "  Confirmed   0.01000000 BTC (1,000,000 sat)\n  \
             Unconfirmed 0.00000000 BTC (0 sat)\n  \
             Total       0.01000000 BTC (1,000,000 sat)"
        );

        let balance = BalanceView {
            confirmed_sat: 1_000_000,
            unconfirmed_sat: 0,
            immature_sat: 5_000_002_820,
            total_sat: 5_001_002_820,
        };
        let lines = balance_lines(&balance, Painter::plain());
        assert!(
            lines.contains(
                "  Immature    50.00002820 BTC (5,000,002,820 sat)  mined; spendable after 100 confirmations"
            ),
            "{lines}"
        );
        assert!(lines.contains("  Confirmed    0.01000000 BTC"), "{lines}");
    }

    #[test]
    fn direction_from_net_amount() {
        assert_eq!(direction(5), "received");
        assert_eq!(direction(-5), "sent");
        assert_eq!(direction(0), "self");
    }
}
