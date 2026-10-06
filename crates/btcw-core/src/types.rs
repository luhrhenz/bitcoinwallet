//! Plain, serializable views of wallet state.
//!
//! The only shapes in the CLI's `--json` output and the desktop UI; mirrored by hand in
//! `apps/desktop/src/lib/types.ts`. Amounts are integer satoshis.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Keychain {
    /// Receive addresses, `m/84'/c'/0'/0/*`.
    External,
    /// Change addresses, `m/84'/c'/0'/1/*`.
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressRow {
    pub index: u32,
    pub address: String,
    pub keychain: Keychain,
    /// True once any transaction output has paid to this address.
    pub used: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalanceView {
    pub confirmed_sat: u64,
    /// Mempool funds: BDK's `trusted_pending` (our own change) + `untrusted_pending` (incoming).
    pub unconfirmed_sat: u64,
    /// Coinbase outputs with fewer than 100 confirmations (appears on regtest).
    pub immature_sat: u64,
    pub total_sat: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TxStatus {
    Unconfirmed {
        /// Unix time we first saw it in the mempool, if known.
        first_seen: Option<u64>,
    },
    Confirmed {
        height: u32,
        /// `tip_height - height + 1`
        confirmations: u32,
        /// Block timestamp (unix seconds).
        block_time: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxRow {
    pub txid: String,
    pub received_sat: u64,
    pub sent_sat: u64,
    /// `received - sent` from this wallet's point of view (negative = outgoing).
    pub net_sat: i64,
    /// Known only when all inputs are ours or were seen by the wallet.
    pub fee_sat: Option<u64>,
    pub status: TxStatus,
    /// The user's own note for this transaction (`WalletService::set_label`), if any.
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UtxoRow {
    /// `txid:vout`
    pub outpoint: String,
    pub value_sat: u64,
    pub address: Option<String>,
    pub keychain: Keychain,
    pub derivation_index: u32,
    pub confirmed: bool,
}

/// Emitted while syncing so frontends can show progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncProgress {
    pub height: u32,
    pub tip_height: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub tip_height: u32,
    pub blocks_scanned: u32,
    pub mempool_txs: u32,
}

/// What the user confirms before a transaction is signed and broadcast.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendPreview {
    pub to: String,
    pub amount_sat: u64,
    pub fee_sat: u64,
    pub fee_rate_sat_vb: f64,
    pub vsize: u64,
    pub change_sat: Option<u64>,
    /// `amount + fee`, what leaves the wallet.
    pub total_sat: u64,
    /// The contact name the user typed, if any. Shown next to `to`, never instead of it.
    #[serde(default)]
    pub contact: Option<String>,
    /// For a fee bump: the txid of the payment this transaction replaces.
    #[serde(default)]
    pub replaces: Option<String>,
}

/// One address-book entry (`WalletService::contacts`). Names are unique ignoring case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    pub name: String,
    /// Validated for the wallet's network; canonical form.
    pub address: String,
    pub note: Option<String>,
}
