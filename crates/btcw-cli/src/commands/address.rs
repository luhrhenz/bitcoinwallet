//! `btcw address new` / `btcw address list`. Watch-only: no password, no node.

use anyhow::Result;
use btcw_core::api;
use btcw_core::config::Config;
use btcw_core::types::AddressRow;
use serde::Serialize;

use crate::output::{Ui, as_of, table};

#[derive(Debug, Serialize)]
struct NewAddressJson {
    network: String,
    #[serde(flatten)]
    address: AddressRow,
}

#[derive(Debug, Serialize)]
struct AddressListJson {
    network: String,
    synced_height: u32,
    addresses: Vec<AddressRow>,
}

/// The lowest revealed address that has never been paid, so asking twice returns the same one.
pub fn new(cfg: &Config, ui: &Ui) -> Result<()> {
    let mut wallet = api::open_watch_only(cfg)?;
    let address = wallet.new_address()?;
    drop(wallet);

    if ui.json() {
        return ui.print_json(&NewAddressJson {
            network: cfg.network.to_string(),
            address,
        });
    }
    ui.println(&format!(
        "{} Receive address #{}\n{}\n{}",
        ui.out.badge(cfg.network),
        address.index,
        ui.out.bold(&address.address),
        ui.out.dim(
            "btcw hands out this address until it receives a payment, so asking again \
             returns it again."
        )
    ))
}

/// Every revealed receive address with its used/unused state as of the last sync.
pub fn list(cfg: &Config, ui: &Ui) -> Result<()> {
    let wallet = api::open_watch_only(cfg)?;
    let addresses = wallet.addresses();
    let synced_height = wallet.synced_height();
    drop(wallet);

    if ui.json() {
        return ui.print_json(&AddressListJson {
            network: cfg.network.to_string(),
            synced_height,
            addresses,
        });
    }
    let net = ui.out.badge(cfg.network);
    if addresses.is_empty() {
        return ui.println(&format!(
            "{net} No receive addresses yet; run `btcw address new`."
        ));
    }
    let mut rows = table(&["#", "Address", "Status"], &[0]);
    for row in &addresses {
        rows.add_row(vec![
            row.index.to_string(),
            row.address.clone(),
            if row.used { "used" } else { "unused" }.to_owned(),
        ]);
    }
    ui.println(&format!(
        "{net} Receive addresses {}\n{rows}",
        as_of(synced_height)
    ))
}
