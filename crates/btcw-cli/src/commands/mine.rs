//! `btcw mine N [--to ADDR]`: regtest only, for demos and tests.

use anyhow::Result;
use btcw_core::WalletError;
use btcw_core::api;
use btcw_core::bitcoin::address::NetworkUnchecked;
use btcw_core::bitcoin::{Address, Network};
use btcw_core::chain::Node;
use btcw_core::config::Config;
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
    // `Node::mine` refuses other networks too; checking here first means a mistaken
    // `--network testnet4 mine` never needs a node or touches the wallet.
    if cfg.network != Network::Regtest {
        return Err(WalletError::Config(format!(
            "mining is only available on regtest (this wallet is on {})",
            cfg.network
        ))
        .into());
    }
    // Parse `--to` before connecting: a typo is reported without needing a node.
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

/// Parse an address and require it to belong to `network`.
///
/// `tx::parse_address` (Agent F, Phase 2) is the shared version of this; `mine` can switch to
/// it once it exists.
fn parse_address(s: &str, network: Network) -> Result<Address, WalletError> {
    let unchecked: Address<NetworkUnchecked> = s
        .trim()
        .parse()
        .map_err(|e| WalletError::InvalidAddress(format!("`{}`: {e}", s.trim())))?;
    let found = networks_of(&unchecked);
    unchecked
        .require_network(network)
        .map_err(|_| WalletError::NetworkMismatch {
            expected: network,
            found,
        })
}

/// Which supported networks an address is valid on, e.g. `"a testnet4/signet address"`.
/// (A `tb1…` address can't tell testnet4 from signet: they share the prefix.)
fn networks_of(address: &Address<NetworkUnchecked>) -> String {
    let names: Vec<&str> = [
        (Network::Bitcoin, "mainnet"),
        (Network::Testnet4, "testnet4"),
        (Network::Signet, "signet"),
        (Network::Regtest, "regtest"),
    ]
    .into_iter()
    .filter(|(network, _)| address.is_valid_for_network(*network))
    .map(|(_, name)| name)
    .collect();
    if names.is_empty() {
        "an address for another network".to_owned()
    } else {
        format!("a {} address", names.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_must_match_the_network() {
        // BIP84 test phrase (`abandon … about`), m/84'/1'/0'/0/0 on regtest and on testnet4.
        let regtest = "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk";
        assert!(parse_address(regtest, Network::Regtest).is_ok());
        assert!(parse_address(&format!("  {regtest}\n"), Network::Regtest).is_ok());

        match parse_address(
            "tb1q6rz28mcfaxtmd6v789l9rrlrusdprr9pqcpvkl",
            Network::Regtest,
        ) {
            Err(e @ WalletError::NetworkMismatch { .. }) => assert_eq!(
                e.to_string(),
                "network mismatch: expected regtest, found a testnet4/signet address"
            ),
            other => panic!("expected NetworkMismatch, got {other:?}"),
        }
        match parse_address(
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu",
            Network::Regtest,
        ) {
            Err(e @ WalletError::NetworkMismatch { .. }) => assert_eq!(
                e.to_string(),
                "network mismatch: expected regtest, found a mainnet address"
            ),
            other => panic!("expected NetworkMismatch, got {other:?}"),
        }
        match parse_address("not-an-address", Network::Regtest) {
            Err(e @ WalletError::InvalidAddress(_)) => {
                assert!(
                    e.to_string()
                        .starts_with("invalid address: `not-an-address`"),
                    "{e}"
                );
            }
            other => panic!("expected InvalidAddress, got {other:?}"),
        }
    }
}
