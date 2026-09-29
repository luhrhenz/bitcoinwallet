//! Sending: address validation → PSBT build → preview → sign → extract; plus tx status.
//!
//! OWNER: Agent F (Phase 2). Contract: PLAN §5.8–5.9.
//!
//! Flow used by both frontends:
//! ```text
//! parse_address ─▶ build_psbt ─▶ preview ─▶ (user confirms?) ─┬─▶ sign_psbt ─▶ extract_tx
//!                                                            └─▶ cancel (releases change addr)
//! extract_tx ─▶ Node::broadcast ─▶ record_broadcast
//! ```
#![allow(unused_variables, dead_code)] // remove once implemented

use crate::bitcoin::{Address, Amount, FeeRate, Network, Psbt, Transaction, Txid};
use crate::error::Result;
use crate::keys::Signer;
use crate::types::{SendPreview, TxStatus};
use crate::wallet::WalletService;

/// Dust threshold for a P2WPKH output at the default relay fee.
pub const DUST_LIMIT_P2WPKH: Amount = Amount::from_sat(294);

/// Parse an address and require it to be for `network` (`InvalidAddress` / `NetworkMismatch`).
pub fn parse_address(s: &str, network: Network) -> Result<Address> {
    todo!("Agent F")
}

/// Unsigned PSBT paying `amount` to `to` at `fee_rate`; BDK picks coins and adds change.
/// `DustAmount` / `InsufficientFunds` on bad input.
pub fn build_psbt(
    wallet: &mut WalletService,
    to: &Address,
    amount: Amount,
    fee_rate: FeeRate,
) -> Result<Psbt> {
    todo!("Agent F")
}

/// Summarise a built PSBT for the confirmation step.
pub fn preview(
    wallet: &WalletService,
    psbt: &Psbt,
    to: &Address,
    amount: Amount,
) -> Result<SendPreview> {
    todo!("Agent F")
}

/// User declined: un-reserve the change address BDK revealed while building.
pub fn cancel(wallet: &mut WalletService, psbt: &Psbt) {
    todo!("Agent F")
}

/// `signer.sign_psbt` then BDK's `finalize_psbt` (builds the witnesses).
/// `Sign` error unless every input ends up finalized.
pub fn sign_psbt(wallet: &WalletService, signer: &Signer, psbt: &mut Psbt) -> Result<()> {
    todo!("Agent F")
}

/// Extract the network-ready transaction (rejects absurd fee rates).
pub fn extract_tx(psbt: Psbt) -> Result<Transaction> {
    todo!("Agent F")
}

/// After a successful broadcast: add the tx to the wallet as unconfirmed and persist,
/// so the balance reflects the spend before the next sync.
pub fn record_broadcast(wallet: &mut WalletService, tx: &Transaction) -> Result<()> {
    todo!("Agent F")
}

/// Status from the wallet's view (call `Node::sync` first for fresh data).
pub fn tx_status(wallet: &WalletService, txid: Txid) -> Option<TxStatus> {
    todo!("Agent F")
}
