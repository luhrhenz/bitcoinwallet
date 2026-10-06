//! `btcw mine N [--to ADDR]`: regtest only, for demos and tests.

use anyhow::Result;
use btcw_core::WalletError;
use btcw_core::api;
use btcw_core::bitcoin::Network;
use btcw_core::chain::Node;
use btcw_core::config::Config;
use btcw_core::tx::parse_address;
use serde::Serialize;

use crate::output::{Ui, plural};

#[derive(Debug, Serialize)]
struct MineJson {
    network: String,
    to: String,
    /// Set when the coins went to the wallet's own receive address (no `--to`).
    wallet_address_index: Option<u32>,
    block_hashes: Vec<String>,
    tip_height: u32,
}

pub fn run(cfg: &Config, ui: &Ui, blocks: u64, to: Option<&str>) -> Result<()> {
    // `Node::mine` refuses too; checking first avoids the node and the wallet.
    if cfg.network != Network::Regtest {
        return Err(WalletError::Config(format!(
            "mining is only available on regtest (this wallet is on {})",
            cfg.network
        ))
        .into());
    }
    // Parse `--to` before connecting.
    let explicit = to.map(|s| parse_address(s, cfg.network)).transpose()?;
    let node = Node::connect(&cfg.rpc, cfg.network)?;
    let (address, wallet_address_index) = match explicit {
        Some(address) => (address, None),
        None => {
            let mut wallet = api::open_watch_only(cfg)?;
            let row = wallet.new_address()?;
            drop(wallet);
            (parse_address(&row.address, cfg.network)?, Some(row.index))
        }
    };

    let hashes = node.mine(blocks, &address)?;
    let tip_height = node.tip_height()?;

    if ui.json() {
        return ui.print_json(&MineJson {
            network: cfg.network.to_string(),
            to: address.to_string(),
            wallet_address_index,
            block_hashes: hashes.iter().map(ToString::to_string).collect(),
            tip_height,
        });
    }
    let (owner, hint) = match wallet_address_index {
        Some(index) => (
            format!(" (wallet receive address #{index})"),
            "\nRun `btcw sync` to see the reward in the wallet; mined coins can be spent after \
             100 confirmations.",
        ),
        None => (String::new(), ""),
    };
    let mined = u32::try_from(hashes.len()).unwrap_or(u32::MAX);
    ui.println(&format!(
        "{} Mined {} to {address}{owner}; the node's tip is now block {tip_height}.{hint}",
        ui.out.badge(cfg.network),
        plural(mined, "block", "blocks"),
    ))
}
