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
#![allow(unused_variables, dead_code)] // remove once implemented

use bdk_bitcoind_rpc::bitcoincore_rpc::Client;

use crate::bitcoin::{Address, BlockHash, FeeRate, Network, Transaction, Txid};
use crate::config::RpcConfig;
use crate::error::Result;
use crate::types::{SyncProgress, SyncReport};
use crate::wallet::WalletService;

/// Used when the node has no fee estimate (always the case on a fresh regtest chain).
pub const FALLBACK_FEE_RATE: FeeRate = FeeRate::from_sat_per_vb_u32(2);

pub struct Node {
    client: Client,
    network: Network,
}

impl Node {
    /// Connect (cookie or user/pass) and verify the node is on `network`.
    pub fn connect(rpc: &RpcConfig, network: Network) -> Result<Self> {
        todo!("Agent B")
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn tip_height(&self) -> Result<u32> {
        todo!("Agent B")
    }

    /// Bring the wallet up to the node's tip and mempool. Calls `on_progress` per block.
    pub fn sync(
        &self,
        wallet: &mut WalletService,
        on_progress: &mut dyn FnMut(SyncProgress),
    ) -> Result<SyncReport> {
        todo!("Agent B")
    }

    /// `sendrawtransaction`. Node rejections come back as `Rpc` with Core's message.
    pub fn broadcast(&self, tx: &Transaction) -> Result<Txid> {
        todo!("Agent B")
    }

    /// `estimatesmartfee` for `target_blocks`, else [`FALLBACK_FEE_RATE`] (logged at warn).
    pub fn estimate_fee_rate(&self, target_blocks: u16) -> Result<FeeRate> {
        todo!("Agent B")
    }

    /// Regtest only (`Config` error on any other network): `generatetoaddress`.
    pub fn mine(&self, blocks: u64, to: &Address) -> Result<Vec<BlockHash>> {
        todo!("Agent B")
    }
}
