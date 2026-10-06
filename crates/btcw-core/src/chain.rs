//! [`Node`]: everything that talks to Bitcoin Core over JSON-RPC.
//!
//! OWNER: Agent B (Phase 1). Contract: PLAN §4, §5.5.
//!
//! - `connect` checks `getblockchaininfo.chain` matches the wallet network (`NetworkMismatch`).
//!   Prefer raw `client.call::<serde_json::Value>(..)` for RPCs whose response shape changed in
//!   recent Core versions; `bitcoincore-rpc` 0.19 predates Core v28+ and some typed helpers
//!   fail to parse newer responses.
//! - `bitcoincore-rpc`'s default HTTP transport times out after 15 s. Build the client with
//!   `jsonrpc::simple_http::Builder` + `Client::from_jsonrpc` and a 120 s timeout, because
//!   `generatetoaddress` is ~200 ms/block on Core v31 and large blocks take a while too.
//! - `sync` uses `bdk_bitcoind_rpc::Emitter`: start from `wallet.latest_checkpoint()` with
//!   `start_height = wallet.birthday_height()`, apply each
//!   block with `apply_block_connected_to`, persist every 100 blocks, then apply the mempool
//!   (`apply_unconfirmed_txs`, `apply_evicted_txs`) and persist.
//!
//! Every RPC error becomes `WalletError::Rpc` with what we were doing, the node's URL (with any
//! `user:pass@` removed) and, for connection problems, a hint about what to check. Credentials
//! never appear in errors or logs.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bdk_bitcoind_rpc::Emitter;
use bdk_bitcoind_rpc::bitcoincore_rpc::jsonrpc;
use bdk_bitcoind_rpc::bitcoincore_rpc::jsonrpc::{minreq_http, simple_http};
use bdk_bitcoind_rpc::bitcoincore_rpc::{self, Client, RpcApi};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::bitcoin::{Address, Amount, BlockHash, FeeRate, Network, Transaction, Txid};
use crate::config::{self, RpcAuth, RpcConfig};
use crate::error::{Result, WalletError};
use crate::types::{SyncProgress, SyncReport};
use crate::wallet::WalletService;

/// Used when the node has no fee estimate (always the case on a fresh regtest chain).
pub const FALLBACK_FEE_RATE: FeeRate = FeeRate::from_sat_per_vb_u32(2);

/// Per-request HTTP timeout. `bitcoincore-rpc`'s default is 15 s, which a single
/// `generatetoaddress` call or a large block on a slow disk can exceed (PLAN §4, Phase 0
/// finding #3).
const RPC_TIMEOUT: Duration = Duration::from_secs(120);

/// Write sync progress to SQLite this often, so an interrupted first sync of a long chain
/// resumes close to where it stopped instead of at the birthday.
const PERSIST_EVERY_BLOCKS: u32 = 100;

/// Blocks per `generatetoaddress` call: ~5 s on Core v31, far below [`RPC_TIMEOUT`].
const MINE_BATCH: u64 = 25;

pub struct Node {
    client: Client,
    network: Network,
    /// The RPC URL with any `user:pass@` part removed, for error messages.
    url: String,
}

impl Node {
    /// Connect (cookie or user/pass) and verify the node is on `network`.
    pub fn connect(rpc: &RpcConfig, network: Network) -> Result<Self> {
        let url = redact_url(&rpc.url);
        let credentials = match &rpc.auth {
            RpcAuth::Cookie(path) => Some(read_cookie(path, network)?),
            RpcAuth::UserPass { user, pass } => Some((
                user.clone(),
                Zeroizing::new(pass.expose_secret().to_owned()),
            )),
            RpcAuth::None => None,
        };
        let client = if config::is_https(&rpc.url) {
            // Hosted providers and remote nodes: TLS via minreq (rustls, Mozilla's root store).
            // simple_http only speaks plain HTTP, which would send credentials and the API key
            // in the URL unencrypted.
            let mut builder = minreq_http::Builder::new()
                .timeout(RPC_TIMEOUT)
                .url(&rpc.url)
                .map_err(|_| WalletError::Config(format!("invalid RPC URL `{url}`")))?;
            if let Some((user, pass)) = &credentials {
                builder = builder.basic_auth(user.clone(), Some(pass.to_string()));
            }
            Client::from_jsonrpc(jsonrpc::Client::with_transport(builder.build()))
        } else {
            let mut builder = simple_http::Builder::new()
                .timeout(RPC_TIMEOUT)
                .url(&rpc.url)
                .map_err(|e| url_error(&url, e))?;
            if let Some((user, pass)) = &credentials {
                builder = builder.auth(user.as_str(), Some(pass.as_str()));
            }
            Client::from_jsonrpc(jsonrpc::Client::with_transport(builder.build()))
        };
        let node = Self {
            client,
            network,
            url,
        };

        // The first request is what actually opens the TCP connection, so this is also where
        // "bitcoind isn't running" and "wrong credentials" surface.
        let info = node.blockchain_info()?;
        let chain = json_str(&info, "chain", "getblockchaininfo")?;
        // Core says "main"/"test"; report those as our names ("bitcoin"/"testnet"), and an
        // unknown chain verbatim.
        let found = Network::from_core_arg(chain).ok();
        if found != Some(network) {
            return Err(WalletError::NetworkMismatch {
                expected: network,
                found: found.map_or_else(|| chain.to_owned(), |n| n.to_string()),
            });
        }
        if info.get("initialblockdownload").and_then(Value::as_bool) == Some(true) {
            tracing::warn!(
                url = %node.url,
                "bitcoind is still in initial block download; sync works but stops at the \
                 node's current height, which is behind the real chain tip"
            );
        }
        Ok(node)
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn tip_height(&self) -> Result<u32> {
        let height: u64 = self
            .client
            .call("getblockcount", &[])
            .map_err(|e| self.rpc_error("getblockcount", e))?;
        u32::try_from(height)
            .map_err(|_| WalletError::Rpc(format!("getblockcount: height {height} is too large")))
    }

    /// Bring the wallet up to the node's tip and mempool. Calls `on_progress` per block.
    ///
    /// `SyncReport::mempool_txs` is the number of the wallet's transactions that are still
    /// unconfirmed after the sync, i.e. waiting in the node's mempool.
    pub fn sync(
        &self,
        wallet: &mut WalletService,
        on_progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncReport> {
        if wallet.network() != self.network {
            return Err(WalletError::NetworkMismatch {
                expected: wallet.network(),
                found: self.network.to_string(),
            });
        }

        // One call gives both the tip for progress reporting and the pruning state.
        let info = self.blockchain_info()?;
        let mut tip_height = json_u32(&info, "blocks", "getblockchaininfo")?;
        let first_needed = first_needed_height(wallet.synced_height(), wallet.birthday_height());
        check_pruned(&info, first_needed, tip_height)?;

        // How the Emitter picks its first block (bdk_bitcoind_rpc 0.22, `poll_once`):
        // 1. It walks `last_cp` (our latest checkpoint) backwards until it finds a block that is
        //    still on the node's best chain: the "agreement point". Checkpoints above it were
        //    reorged out.
        // 2. If the agreement point is below `start_height`, it jumps straight to
        //    `start_height` and emits *that* block (so the birthday block itself is scanned).
        //    Otherwise it follows `nextblockhash` from the agreement point.
        // So the birthday only matters until the first sync; after that the persisted
        // checkpoint is above it and sync resumes right after the last block we saw.
        //
        // The wallet's unconfirmed txs are passed in so the Emitter can report the ones that
        // have since disappeared from the mempool (replaced, expired) as evicted.
        // Plus every mempool transaction a previous sync already downloaded (see
        // `mempool_cache`): the Emitter only fetches txids it doesn't hold yet, one
        // `getrawtransaction` round trip each, which is most of a sync against a remote node.
        let cache_path = wallet.mempool_cache_path();
        let mut expected = unconfirmed_txs(wallet);
        expected.extend(mempool_cache::load(&cache_path));
        let mut emitter = Emitter::new(
            &self.client,
            wallet.bdk().latest_checkpoint(),
            wallet.birthday_height(),
            expected,
        );

        // A birthday above the node's tip: the birthday block was reorged away (before or after
        // we scanned it), a restore was given a height the chain hasn't reached, or the node is
        // still catching up. No block on the node's chain can hold our transactions, so there is
        // nothing to scan; the Emitter would ask for the birthday block by height and fail
        // ("Block height out of range"). Skip the blocks, still read the mempool, and scan once
        // the chain gets there (a replaced birthday block is then handled like any reorg).
        let birthday = wallet.birthday_height();
        let waiting_for_birthday = birthday > tip_height;
        if waiting_for_birthday {
            tracing::warn!(
                birthday,
                node_height = tip_height,
                "the wallet's birthday is above the node's tip; no blocks to scan until the \
                 node reaches it"
            );
        }

        let mut blocks_scanned: u32 = 0;
        while !waiting_for_birthday
            && let Some(event) = emitter
                .next_block()
                .map_err(|e| self.rpc_error("fetching the next block", e))?
        {
            let height = event.block_height();
            // `connected_to` is the previous emitted block, or the agreement point after a
            // reorg. BDK's `LocalChain` uses it to drop our checkpoints above that point that
            // conflict with this block, which un-confirms the transactions anchored in them.
            wallet
                .bdk_mut()
                .apply_block_connected_to(&event.block, height, event.connected_to())
                .map_err(|e| {
                    WalletError::Rpc(format!(
                        "block {height} ({}) does not connect to the wallet's chain: {e}",
                        event.block_hash()
                    ))
                })?;
            blocks_scanned = blocks_scanned.saturating_add(1);
            // New blocks may arrive while we sync; never report a height above the "tip".
            tip_height = tip_height.max(height);
            on_progress(SyncProgress { height, tip_height });
            if blocks_scanned.is_multiple_of(PERSIST_EVERY_BLOCKS) {
                wallet.persist()?;
            }
        }

        let mempool = emitter
            .mempool()
            .map_err(|e| self.rpc_error("reading the mempool", e))?;
        mempool_cache::save(&cache_path, mempool.update.iter().map(|(tx, _)| tx));
        let bdk = wallet.bdk_mut();
        bdk.apply_unconfirmed_txs(mempool.update);
        // Empty unless the Emitter has reached the node's tip (it can't tell "evicted" from
        // "confirmed in a block we haven't fetched yet" before that). Cached strangers' txs show
        // up here too once they leave the mempool; only the wallet's own evictions matter, and
        // passing the rest would just add rows for unrelated txids to the database.
        let evicted: Vec<(Txid, u64)> = mempool
            .evicted
            .into_iter()
            .filter(|(txid, _)| bdk.tx_graph().get_tx(*txid).is_some())
            .collect();
        bdk.apply_evicted_txs(evicted);
        wallet.persist()?;

        let synced = wallet.synced_height();
        if synced > tip_height {
            // The node's chain got shorter (`invalidateblock`, or we switched to a node that is
            // still catching up). The Emitter only reports blocks that *connect*, and BDK has no
            // public way to drop checkpoints without a replacement block, so the wallet keeps its
            // higher tip until the node has a block at that height again; that block then
            // replaces ours like any other reorg.
            tracing::warn!(
                wallet_height = synced,
                node_height = tip_height,
                "the node's chain is shorter than the wallet's; transactions in the missing \
                 blocks stay confirmed until the node reaches that height again"
            );
        }

        Ok(SyncReport {
            tip_height: synced,
            blocks_scanned,
            mempool_txs: u32::try_from(unconfirmed_txs(wallet).len()).unwrap_or(u32::MAX),
        })
    }

    /// `sendrawtransaction`. Node rejections come back as `Rpc` with Core's message.
    pub fn broadcast(&self, tx: &Transaction) -> Result<Txid> {
        self.client
            .send_raw_transaction(tx)
            .map_err(|e| self.rpc_error(&format!("broadcasting {}", tx.compute_txid()), e))
    }

    /// `estimatesmartfee` for `target_blocks`, else [`FALLBACK_FEE_RATE`] (logged at warn).
    pub fn estimate_fee_rate(&self, target_blocks: u16) -> Result<FeeRate> {
        // Raw call: the typed helper's result struct has changed shape across Core versions.
        let estimate: Value = self
            .client
            .call("estimatesmartfee", &[json!(target_blocks)])
            .map_err(|e| self.rpc_error("estimatesmartfee", e))?;
        match fee_rate_from_estimate(&estimate) {
            Ok(rate) => Ok(rate),
            Err(reason) => {
                tracing::warn!(
                    target_blocks,
                    reason = %reason,
                    fallback_sat_per_vb = FALLBACK_FEE_RATE.to_sat_per_vb_ceil(),
                    "no fee estimate from bitcoind; using the fallback fee rate"
                );
                Ok(FALLBACK_FEE_RATE)
            }
        }
    }

    /// Regtest only (`Config` error on any other network): `generatetoaddress`.
    pub fn mine(&self, blocks: u64, to: &Address) -> Result<Vec<BlockHash>> {
        if self.network != Network::Regtest {
            return Err(WalletError::Config(format!(
                "mining is only available on regtest (this node is on {})",
                self.network
            )));
        }
        let to = to.to_string();
        let mut hashes = Vec::new();
        let mut left = blocks;
        while left > 0 {
            let batch = left.min(MINE_BATCH);
            let value: Value = self
                .client
                .call("generatetoaddress", &[json!(batch), json!(to)])
                .map_err(|e| self.rpc_error("generatetoaddress", e))?;
            let batch_hashes: Vec<BlockHash> = serde_json::from_value(value).map_err(|e| {
                WalletError::Rpc(format!("generatetoaddress: unexpected response: {e}"))
            })?;
            hashes.extend(batch_hashes);
            left -= batch;
        }
        Ok(hashes)
    }

    fn blockchain_info(&self) -> Result<Value> {
        // Raw call: `bitcoincore-rpc` 0.19's typed result predates fields Core v28+ changed.
        self.client
            .call("getblockchaininfo", &[])
            .map_err(|e| self.rpc_error("getblockchaininfo", e))
    }

    fn rpc_error(&self, what: &str, e: bitcoincore_rpc::Error) -> WalletError {
        rpc_error(&self.url, self.network, what, e)
    }
}

/// Mempool transactions already downloaded by a previous sync, so the next one only fetches new
/// ones. Public data (it's the node's mempool), so a plain file next to the wallet database.
/// Best effort both ways: an unreadable or missing cache just means a slower sync.
mod mempool_cache {
    use std::io::Write;
    use std::path::Path;
    use std::sync::Arc;

    use crate::bitcoin::Transaction;
    use crate::bitcoin::consensus::encode::{deserialize_hex, serialize_hex};

    /// Mainnet's mempool can hold hundreds of thousands of transactions; keep the file small.
    const MAX_TXS: usize = 5_000;
    const MAX_BYTES: usize = 8 * 1024 * 1024;

    /// One transaction per line, consensus-encoded hex. Bad lines are skipped.
    pub fn load(path: &Path) -> Vec<Arc<Transaction>> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .take(MAX_TXS)
            .filter_map(|line| deserialize_hex::<Transaction>(line.trim()).ok())
            .map(Arc::new)
            .collect()
    }

    /// Replace the cache with the current mempool (written to a temp file, then renamed).
    pub fn save<'a>(path: &Path, txs: impl Iterator<Item = &'a Arc<Transaction>>) {
        let mut text = String::new();
        for tx in txs.take(MAX_TXS) {
            let line = serialize_hex(tx.as_ref());
            if text.len() + line.len() + 1 > MAX_BYTES {
                break;
            }
            text.push_str(&line);
            text.push('\n');
        }
        let tmp = path.with_extension("tmp");
        let written = std::fs::File::create(&tmp)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            tracing::debug!(error = %e, "could not save the mempool cache");
        }
    }
}

/// The wallet's transactions that are not in a block (yet).
fn unconfirmed_txs(wallet: &WalletService) -> Vec<Arc<Transaction>> {
    wallet
        .bdk()
        .transactions()
        .filter(|wtx| wtx.chain_position.is_unconfirmed())
        .map(|wtx| wtx.tx_node.tx.clone())
        .collect()
}

/// Lowest block height the next sync will download (ignoring reorgs, which only reach a few
/// blocks back and stay well inside the 288 blocks a pruned node always keeps).
fn first_needed_height(synced_height: u32, birthday: u32) -> u32 {
    synced_height.saturating_add(1).max(birthday)
}

/// A pruned node has deleted old blocks. If we need one of them, `getblock` fails with an
/// obscure "Block not available (pruned data)"; say what is going on instead.
fn check_pruned(info: &Value, first_needed: u32, tip_height: u32) -> Result<()> {
    let pruned = info.get("pruned").and_then(Value::as_bool) == Some(true);
    if !pruned || first_needed > tip_height {
        return Ok(());
    }
    // `pruneheight`: the lowest height whose block is still stored.
    let Some(prune_height) = info.get("pruneheight").and_then(Value::as_u64) else {
        return Ok(());
    };
    if u64::from(first_needed) < prune_height {
        return Err(WalletError::Rpc(format!(
            "node pruned blocks below {prune_height}; the wallet needs blocks from \
             {first_needed} — use a non-pruned node or a later --birthday"
        )));
    }
    Ok(())
}

/// `estimatesmartfee` → fee rate, or why there is none (then the caller falls back).
fn fee_rate_from_estimate(estimate: &Value) -> std::result::Result<FeeRate, String> {
    if let Some(btc_per_kvb) = estimate.get("feerate").and_then(Value::as_f64) {
        return fee_rate_from_btc_per_kvb(btc_per_kvb)
            .ok_or_else(|| format!("unusable feerate {btc_per_kvb} BTC/kvB"));
    }
    // Core sends `errors` instead of `feerate` when it hasn't seen enough transactions yet.
    let errors: Vec<&str> = estimate
        .get("errors")
        .and_then(Value::as_array)
        .map(|errors| errors.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if errors.is_empty() {
        Err("no feerate in the response".into())
    } else {
        Err(errors.join("; "))
    }
}

/// Core quotes fee rates in BTC per 1000 *virtual* bytes; `FeeRate` counts sat per 1000
/// *weight units*, and 1 vB = 4 WU. So sat/kwu = BTC/kvB × 1e8 / 4.
///
/// Rounded **up**, and never below 1 sat/vB (the default minimum relay fee): rounding down could
/// put us just under the rate the node asked for, or under the relay minimum, and the
/// transaction would be rejected or never confirm. `None` for non-positive or absurd values.
fn fee_rate_from_btc_per_kvb(btc_per_kvb: f64) -> Option<FeeRate> {
    const SAT_PER_BTC: f64 = 100_000_000.0;
    if !btc_per_kvb.is_finite() || btc_per_kvb <= 0.0 {
        return None;
    }
    // Core prints amounts with exactly 8 decimals, so this is a whole number of satoshis up to
    // float error (0.29 BTC × 1e8 = 28999999.999999996): `round` recovers it exactly.
    let sat_per_kvb = (btc_per_kvb * SAT_PER_BTC).round();
    // More than all bitcoin per kvB is nonsense. The bound also keeps the cast below exact:
    // 2.1e15 < 2^53, so every value up to it is an integer an f64 represents exactly.
    if sat_per_kvb > Amount::MAX_MONEY.to_sat() as f64 {
        return None;
    }
    let sat_per_kvb = sat_per_kvb as u64;
    let sat_per_kwu = sat_per_kvb.div_ceil(4);
    Some(FeeRate::from_sat_per_kwu(
        sat_per_kwu.max(FeeRate::BROADCAST_MIN.to_sat_per_kwu()),
    ))
}

/// Bitcoin Core's `.cookie` file: one line `__cookie__:<random password>`, rewritten each time
/// bitcoind starts. The password is a secret, so it lives in a wiped buffer and never appears
/// in an error.
fn read_cookie(path: &Path, network: Network) -> Result<(String, Zeroizing<String>)> {
    let contents = Zeroizing::new(std::fs::read_to_string(path).map_err(|e| {
        WalletError::Rpc(format!(
            "cannot read the RPC cookie file {}: {e}; is bitcoind running for {network}? \
             set --rpc-cookie (or --rpc-user and --rpc-pass)",
            path.display()
        ))
    })?);
    let line = contents.lines().next().unwrap_or_default();
    let Some((user, pass)) = line.split_once(':') else {
        return Err(WalletError::Rpc(format!(
            "RPC cookie file {} is malformed (expected `user:password` on the first line)",
            path.display()
        )));
    };
    Ok((user.to_owned(), Zeroizing::new(pass.to_owned())))
}

/// `https://user:pass@host:port/v2/<API key>?x=y` → `https://host:port/…`, for messages and logs.
///
/// Both the userinfo *and* the path go: hosted providers (Alchemy, …) put the API key in the
/// path, and Core's own `/wallet/<name>` paths are no help in an error message anyway.
fn redact_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, url),
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    let elided = if authority_end < rest.len() {
        "/…"
    } else {
        ""
    };
    match scheme {
        Some(scheme) => format!("{scheme}://{host}{elided}"),
        None => format!("{host}{elided}"),
    }
}

/// Errors from parsing the URL (and resolving its host) before any request is sent.
fn url_error(url: &str, e: simple_http::Error) -> WalletError {
    match e {
        // `InvalidUrl`'s own message repeats the raw URL, which may contain credentials.
        simple_http::Error::InvalidUrl { reason, .. } => {
            WalletError::Config(format!("invalid RPC URL `{url}`: {reason}"))
        }
        other => WalletError::Rpc(format!("cannot resolve the RPC host in {url}: {other}")),
    }
}

/// Turn a `bitcoincore-rpc` error into a message that says what failed and what to check.
fn rpc_error(url: &str, network: Network, what: &str, e: bitcoincore_rpc::Error) -> WalletError {
    use bitcoincore_rpc::Error as RpcError;

    let detail = match &e {
        // Core answered with an error: its message is the useful part (e.g. "insufficient fee",
        // "bad-txns-inputs-missingorspent", "Block not found").
        RpcError::JsonRpc(jsonrpc::Error::Rpc(rejection)) => {
            format!("{} (code {})", rejection.message, rejection.code)
        }
        RpcError::JsonRpc(jsonrpc::Error::Transport(transport)) => {
            match transport.downcast_ref::<simple_http::Error>() {
                Some(http) => transport_detail(url, network, http),
                None => format!("cannot talk to bitcoind at {url}: {transport}"),
            }
        }
        other => format!("{other} (bitcoind at {url})"),
    };
    WalletError::Rpc(format!("{what}: {detail}"))
}

fn transport_detail(url: &str, network: Network, e: &simple_http::Error) -> String {
    use std::io::ErrorKind;
    match e {
        simple_http::Error::SocketError(io) => match io.kind() {
            ErrorKind::ConnectionRefused => format!(
                "cannot connect to bitcoind at {url} (connection refused); is bitcoind running \
                 for {network}? check --rpc-url"
            ),
            // Linux reports an expired read timeout as EAGAIN (`WouldBlock`), not `TimedOut`.
            ErrorKind::TimedOut | ErrorKind::WouldBlock => format!(
                "bitcoind at {url} did not answer within {} s",
                RPC_TIMEOUT.as_secs()
            ),
            _ => format!("cannot connect to bitcoind at {url}: {io}"),
        },
        simple_http::Error::HttpErrorCode(401) => format!(
            "bitcoind at {url} rejected the RPC credentials (HTTP 401); check --rpc-user and \
             --rpc-pass, or --rpc-cookie (bitcoind writes a new cookie each time it starts)"
        ),
        simple_http::Error::HttpErrorCode(403) => format!(
            "bitcoind at {url} refused this client (HTTP 403); check its -rpcallowip setting"
        ),
        other => format!("bad response from bitcoind at {url}: {other}"),
    }
}

fn json_str<'a>(value: &'a Value, field: &str, rpc: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| WalletError::Rpc(format!("{rpc}: missing or invalid `{field}`")))
}

fn json_u32(value: &Value, field: &str, rpc: &str) -> Result<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| WalletError::Rpc(format!("{rpc}: missing or invalid `{field}`")))
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;
    use crate::bitcoin::address::NetworkUnchecked;

    type TestResult = std::result::Result<(), Box<dyn Error>>;

    fn sat_per_kwu(btc_per_kvb: f64) -> Option<u64> {
        fee_rate_from_btc_per_kvb(btc_per_kvb).map(FeeRate::to_sat_per_kwu)
    }

    #[test]
    fn fee_rate_conversion_is_exact_and_rounds_up() {
        // 0.00001 BTC/kvB = 1000 sat/kvB = 1 sat/vB = 250 sat/kwu.
        assert_eq!(sat_per_kwu(0.000_010_00), Some(250));
        // 20 sat/vB.
        assert_eq!(sat_per_kwu(0.000_200_00), Some(5_000));
        // 12345 sat/kvB / 4 = 3086.25 → 3087: rounding down would undercut the estimate.
        assert_eq!(sat_per_kwu(0.000_123_45), Some(3_087));
        // 1001 sat/kvB = 250.25 sat/kwu → 251, just above the 1 sat/vB relay minimum.
        assert_eq!(sat_per_kwu(0.000_010_01), Some(251));
        // 0.29 × 1e8 is 28999999.999999996 in f64; truncating would lose a satoshi.
        assert_eq!(sat_per_kwu(0.29), Some(7_250_000));
    }

    #[test]
    fn fee_rate_has_a_floor_and_rejects_nonsense() {
        // 0.5 sat/vB is below the default relay minimum: clamp to 1 sat/vB.
        assert_eq!(sat_per_kwu(0.000_005_00), Some(250));
        assert_eq!(sat_per_kwu(0.000_000_01), Some(250));
        assert_eq!(sat_per_kwu(0.0), None);
        assert_eq!(sat_per_kwu(-0.0001), None);
        assert_eq!(sat_per_kwu(f64::NAN), None);
        assert_eq!(sat_per_kwu(f64::INFINITY), None);
        assert_eq!(sat_per_kwu(21_000_001.0), None);
    }

    #[test]
    fn estimate_response_parsing() {
        let ok = json!({ "feerate": 0.0002, "blocks": 2 });
        assert_eq!(
            fee_rate_from_estimate(&ok),
            Ok(FeeRate::from_sat_per_vb_u32(20))
        );

        // What a fresh regtest node answers.
        let none = json!({ "errors": ["Insufficient data or no feerate found"], "blocks": 0 });
        assert_eq!(
            fee_rate_from_estimate(&none),
            Err("Insufficient data or no feerate found".to_string())
        );
        assert!(fee_rate_from_estimate(&json!({ "blocks": 0 })).is_err());
        assert!(fee_rate_from_estimate(&json!({ "feerate": -1 })).is_err());
    }

    #[test]
    fn pruned_node_check() {
        let pruned = json!({ "blocks": 5000, "pruned": true, "pruneheight": 4000 });
        let err = check_pruned(&pruned, 1, 5000);
        match err {
            Err(WalletError::Rpc(msg)) => {
                assert!(msg.contains("pruned blocks below 4000"), "{msg}");
                assert!(msg.contains("needs blocks from 1"), "{msg}");
            }
            other => panic!("expected an Rpc error, got {other:?}"),
        }
        // Everything we need is still stored, or nothing needs downloading.
        assert!(check_pruned(&pruned, 4000, 5000).is_ok());
        assert!(check_pruned(&pruned, 5001, 5000).is_ok());
        let full = json!({ "blocks": 5000, "pruned": false });
        assert!(check_pruned(&full, 1, 5000).is_ok());
    }

    #[test]
    fn first_needed_height_resumes_or_starts_at_birthday() {
        assert_eq!(first_needed_height(0, 0), 1); // genesis is never fetched
        assert_eq!(first_needed_height(0, 800), 800); // fresh wallet: the birthday block
        assert_eq!(first_needed_height(900, 800), 901); // resume after the checkpoint
    }

    #[test]
    fn urls_lose_their_credentials_and_api_keys() {
        assert_eq!(
            redact_url("http://alice:s3cret@127.0.0.1:8332/wallet/x"),
            "http://127.0.0.1:8332/…"
        );
        assert_eq!(redact_url("127.0.0.1:18443"), "127.0.0.1:18443");
        assert_eq!(
            redact_url("http://127.0.0.1:18443"),
            "http://127.0.0.1:18443"
        );
        assert_eq!(redact_url("http://host/a@b"), "http://host/…");
        // Hosted provider: the API key lives in the path (or the query).
        assert_eq!(
            redact_url("https://bitcoin-testnet4.g.alchemy.com/v2/SECRETKEY"),
            "https://bitcoin-testnet4.g.alchemy.com/…"
        );
        assert_eq!(
            redact_url("https://h.example?apikey=SECRET"),
            "https://h.example/…"
        );
    }

    #[test]
    fn cookie_file_parsing() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(".cookie");
        std::fs::write(&path, "__cookie__:abc:def\nignored")?;
        let (user, pass) = read_cookie(&path, Network::Regtest)?;
        assert_eq!(user, "__cookie__");
        assert_eq!(pass.as_str(), "abc:def"); // split on the first colon only

        std::fs::write(&path, "no-colon-secret")?;
        match read_cookie(&path, Network::Regtest) {
            Err(WalletError::Rpc(msg)) => {
                assert!(msg.contains("malformed"), "{msg}");
                assert!(!msg.contains("no-colon-secret"), "{msg}");
            }
            other => return Err(format!("expected Rpc error, got {other:?}").into()),
        }

        let missing = dir.path().join("nope").join(".cookie");
        match read_cookie(&missing, Network::Signet) {
            Err(WalletError::Rpc(msg)) => {
                assert!(msg.contains(&missing.display().to_string()), "{msg}");
                assert!(msg.contains("running for signet"), "{msg}");
            }
            other => return Err(format!("expected Rpc error, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn mining_is_refused_off_regtest() -> TestResult {
        // Never connected: the network check must come before any RPC.
        let node = Node {
            client: Client::from_jsonrpc(jsonrpc::Client::with_transport(
                simple_http::Builder::new().build(),
            )),
            network: Network::Testnet4,
            url: "http://127.0.0.1:1".into(),
        };
        let to = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx"
            .parse::<Address<NetworkUnchecked>>()?
            .require_network(Network::Testnet4)?;
        match node.mine(1, &to) {
            Err(WalletError::Config(msg)) => assert!(msg.contains("testnet4"), "{msg}"),
            other => return Err(format!("expected Config error, got {other:?}").into()),
        }
        Ok(())
    }

    /// Broadcasting needs a real signed transaction, and building one needs `bdk_mut()`, which
    /// integration tests can't reach; so this lives here. Skipped when no `bitcoind` exists.
    #[cfg(feature = "test-utils")]
    mod node {
        use bdk_wallet::SignOptions;

        use super::*;
        use crate::config::{Config, Overrides};
        use crate::keys;
        use crate::testnode::TestNode;

        const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

        fn signed_payment(
            wallet: &mut WalletService,
            signer: &keys::Signer,
            to: &Address,
            sat: u64,
        ) -> std::result::Result<Transaction, Box<dyn Error>> {
            let mut builder = wallet.bdk_mut().build_tx();
            builder
                .add_recipient(to.script_pubkey(), Amount::from_sat(sat))
                .fee_rate(FALLBACK_FEE_RATE);
            let mut psbt = builder.finish()?;
            assert_eq!(signer.sign_psbt(&mut psbt)?, 1);
            assert!(
                wallet
                    .bdk()
                    .finalize_psbt(&mut psbt, SignOptions::default())?
            );
            Ok(psbt.extract_tx()?)
        }

        fn rpc_message(result: Result<Txid>) -> std::result::Result<String, Box<dyn Error>> {
            match result {
                Err(WalletError::Rpc(msg)) => Ok(msg),
                other => Err(format!("expected an Rpc error, got {other:?}").into()),
            }
        }

        #[test]
        fn broadcast_and_double_spend_rejections() -> TestResult {
            if !TestNode::available() {
                eprintln!("skipping: no bitcoind (set BITCOIND_EXE)");
                return Ok(());
            }
            let test_node = TestNode::start()?;
            let node = Node::connect(&test_node.rpc_config(), Network::Regtest)?;

            let dir = tempfile::tempdir()?;
            let cfg = Config::load_with(
                Overrides {
                    datadir: Some(dir.path().to_path_buf()),
                    network: Some("regtest".into()),
                    ..Default::default()
                },
                |_| None,
            )?;
            let (descriptors, signer) =
                keys::derive_account(&keys::parse_mnemonic(ABANDON)?, "", Network::Regtest, 0)?;
            let mut wallet = WalletService::create(&cfg, &descriptors, Some(node.tip_height()?))?;

            // One confirmed 1 000 000 sat coin.
            let address = wallet
                .new_address()?
                .address
                .parse::<Address<NetworkUnchecked>>()?
                .require_network(Network::Regtest)?;
            test_node.fund(&address, Amount::from_sat(1_000_000))?;
            test_node.mine(1)?;
            node.sync(&mut wallet, &mut |_| {})?;
            assert_eq!(wallet.balance().confirmed_sat, 1_000_000);

            // Two payments spending that same coin. The wallet doesn't learn about the first
            // before building the second, so both select it.
            let faucet = test_node.faucet_address()?;
            let first = signed_payment(&mut wallet, &signer, &faucet, 100_000)?;
            let second = signed_payment(&mut wallet, &signer, &faucet, 200_000)?;
            assert_eq!(
                first.input[0].previous_output,
                second.input[0].previous_output
            );

            let txid = node.broadcast(&first)?;
            assert_eq!(txid, first.compute_txid());
            let mempool = test_node.call("getrawmempool", &[])?;
            assert!(
                mempool
                    .as_array()
                    .is_some_and(|txids| txids.contains(&json!(txid.to_string()))),
                "{txid} not in the node's mempool: {mempool}"
            );

            // Same fee rate, so Core refuses it as a replacement (full RBF is on by default
            // since Core 28; it would need a higher fee).
            let msg = rpc_message(node.broadcast(&second))?;
            assert!(msg.contains("insufficient fee"), "{msg}");
            assert!(msg.contains(&second.compute_txid().to_string()), "{msg}");

            // Once the first payment is mined, the coin is gone for good.
            test_node.mine(1)?;
            let msg = rpc_message(node.broadcast(&second))?;
            assert!(msg.contains("bad-txns-inputs-missingorspent"), "{msg}");

            // The wallet sees its own payment confirmed: change = 1 000 000 - 100 000 - fee.
            node.sync(&mut wallet, &mut |_| {})?;
            let fee = wallet.bdk().calculate_fee(&first)?;
            assert_eq!(
                wallet.balance().confirmed_sat,
                1_000_000 - 100_000 - fee.to_sat()
            );
            Ok(())
        }
    }
}
