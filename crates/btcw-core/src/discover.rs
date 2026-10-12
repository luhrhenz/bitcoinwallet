//! Find a restored wallet's birthday by asking an Esplora block explorer (mempool.space by
//! default) when the wallet's addresses were first used.
//!
//! Without a birthday a restore scans the whole chain, which takes hours through a hosted node.
//! The explorer indexes addresses, so this takes seconds, at a privacy cost: the explorer learns
//! the addresses being looked up. Frontends ask before calling it.

use std::thread::sleep;
use std::time::Duration;

use bdk_wallet::{KeychainKind, Wallet};
use serde::{Deserialize, Serialize};

use crate::bitcoin::Network;
use crate::error::{Result, WalletError};
use crate::keys::Descriptors;

/// Unused addresses in a row after which a keychain is considered fully checked (BIP44's gap limit).
pub const GAP_LIMIT: u32 = 20;
/// Hard cap on addresses checked per keychain.
const MAX_INDEX: u32 = 500;
/// Blocks subtracted from the first use, so the scan surely includes it.
const MARGIN: u32 = 6;
/// Esplora returns an address's confirmed history 25 transactions per page.
const PAGE: usize = 25;
/// Pages read per address before giving up (1,000 transactions).
const MAX_PAGES: usize = 40;
const TIMEOUT_SECS: u64 = 20;
/// Budget for opening a connection, shared across the host's addresses.
const CONNECT_SECS: u64 = 10;
/// Pauses before retrying a rate-limited or failed request.
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(8),
];

/// What the lookup found. `birthday` is the height to start scanning from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BirthdayLookup {
    pub birthday: u32,
    /// Block of the earliest confirmed transaction; `None` if the wallet was never used.
    pub first_used: Option<u32>,
    pub addresses_checked: u32,
    /// Scheme and host of the explorer that was asked.
    pub explorer: String,
}

/// Check the wallet's receive and change addresses, in order, until [`GAP_LIMIT`] unused ones in a
/// row, and return the block to start scanning from.
pub fn find_birthday(
    descriptors: &Descriptors,
    network: Network,
    explorer_base: &str,
) -> Result<BirthdayLookup> {
    let explorer = Explorer::new(explorer_base)?;
    let wallet = Wallet::create(
        descriptors.external().to_owned(),
        descriptors.internal().to_owned(),
    )
    .network(network)
    .create_wallet_no_persist()
    .map_err(|e| WalletError::Config(format!("invalid wallet descriptors: {e}")))?;

    let mut first_used: Option<u32> = None;
    let mut checked = 0;
    for keychain in [KeychainKind::External, KeychainKind::Internal] {
        let mut unused_in_a_row = 0;
        let mut index = 0;
        while unused_in_a_row < GAP_LIMIT && index < MAX_INDEX {
            let address = wallet.peek_address(keychain, index).address.to_string();
            let stats: AddressStats = explorer.get_json(&format!("/address/{address}"))?;
            checked += 1;
            if stats.chain_stats.tx_count > 0 {
                let earliest = explorer.earliest_height(&address)?;
                first_used = Some(first_used.map_or(earliest, |h| h.min(earliest)));
                unused_in_a_row = 0;
            } else if stats.mempool_stats.tx_count > 0 {
                // Only unconfirmed so far: it will confirm at or after the tip.
                unused_in_a_row = 0;
            } else {
                unused_in_a_row += 1;
            }
            index += 1;
        }
    }

    let birthday = match first_used {
        Some(height) => height.saturating_sub(MARGIN),
        None => explorer.tip_height()?,
    };
    Ok(BirthdayLookup {
        birthday,
        first_used,
        addresses_checked: checked,
        explorer: explorer.host,
    })
}

#[derive(Deserialize)]
struct AddressStats {
    chain_stats: TxCount,
    mempool_stats: TxCount,
}

#[derive(Deserialize)]
struct TxCount {
    tx_count: u64,
}

#[derive(Deserialize)]
struct EsploraTx {
    txid: String,
    status: EsploraStatus,
}

#[derive(Deserialize)]
struct EsploraStatus {
    block_height: Option<u32>,
}

struct Explorer {
    /// Keeps the connection alive between requests, and tries each of the host's addresses
    /// in turn when connecting (some of an explorer's addresses may be unreachable).
    agent: ureq::Agent,
    base: String,
    /// `https://host`, for messages (no path: some explorers put keys there).
    host: String,
}

impl Explorer {
    fn new(base: &str) -> Result<Self> {
        let base = base.trim().trim_end_matches('/').to_owned();
        let (scheme, rest) = base
            .split_once("://")
            .ok_or_else(|| WalletError::Config(format!("invalid explorer URL `{base}`")))?;
        let authority = rest.split('/').next().unwrap_or_default();
        let host_name = authority.rsplit('@').next().unwrap_or_default();
        let local = host_name.starts_with("127.0.0.1") || host_name.starts_with("localhost");
        // The lookup reveals which addresses belong together; never send that in clear text
        // over the network.
        let allowed = scheme.eq_ignore_ascii_case("https") || (scheme == "http" && local);
        if !allowed {
            return Err(WalletError::Config(
                "the block explorer URL must start with https://".into(),
            ));
        }
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(CONNECT_SECS)))
            .timeout_global(Some(Duration::from_secs(TIMEOUT_SECS)))
            .http_status_as_error(false)
            .user_agent("btcw")
            .build()
            .into();
        Ok(Self {
            agent,
            host: format!("{scheme}://{host_name}"),
            base,
        })
    }

    fn get(&self, path: &str) -> Result<String> {
        let url = format!("{}{path}", self.base);
        let mut attempt = 0;
        loop {
            let outcome = self.agent.get(&url).call();
            let retry = match &outcome {
                Ok(response) => {
                    let status = response.status().as_u16();
                    status == 429 || (500..600).contains(&status)
                }
                Err(_) => true,
            };
            if retry && attempt < RETRY_DELAYS.len() {
                tracing::debug!(attempt, "block explorer request failed; retrying");
                sleep(RETRY_DELAYS[attempt]);
                attempt += 1;
                continue;
            }
            let mut response = outcome
                .map_err(|e| WalletError::Explorer(format!("cannot reach {}: {e}", self.host)))?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(WalletError::Explorer(format!(
                    "{} answered HTTP {status}",
                    self.host
                )));
            }
            return response.body_mut().read_to_string().map_err(|e| {
                WalletError::Explorer(format!("{} sent an unreadable reply: {e}", self.host))
            });
        }
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let text = self.get(path)?;
        serde_json::from_str(&text).map_err(|e| {
            WalletError::Explorer(format!("{} sent an unexpected reply: {e}", self.host))
        })
    }

    fn tip_height(&self) -> Result<u32> {
        let text = self.get("/blocks/tip/height")?;
        text.trim().parse().map_err(|_| {
            WalletError::Explorer(format!("{} sent an unexpected tip height", self.host))
        })
    }

    /// Lowest block height in the address's confirmed history (pages run newest first).
    fn earliest_height(&self, address: &str) -> Result<u32> {
        let mut earliest: Option<u32> = None;
        let mut path = format!("/address/{address}/txs/chain");
        for _ in 0..MAX_PAGES {
            let page: Vec<EsploraTx> = self.get_json(&path)?;
            for tx in &page {
                if let Some(height) = tx.status.block_height {
                    earliest = Some(earliest.map_or(height, |h| h.min(height)));
                }
            }
            match page.last() {
                Some(last) if page.len() == PAGE => {
                    path = format!("/address/{address}/txs/chain/{}", last.txid);
                }
                _ => {
                    return earliest.ok_or_else(|| {
                        WalletError::Explorer(format!(
                            "{} listed no confirmed transactions for a used address",
                            self.host
                        ))
                    });
                }
            }
        }
        Err(WalletError::Explorer(format!(
            "an address of this wallet has more than {} transactions; enter a birthday instead",
            PAGE * MAX_PAGES
        )))
    }
}
