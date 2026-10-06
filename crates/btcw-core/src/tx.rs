//! Sending: address validation → PSBT build → preview → sign → extract; plus tx status.
//!
//! ```text
//! parse_address ─▶ build_psbt ─▶ preview ─▶ (user confirms?) ─┬─▶ sign_psbt ─▶ extract_tx
//!                                                            └─▶ cancel (releases change addr)
//! extract_tx ─▶ Node::broadcast ─▶ record_broadcast
//! ```
//! [`prepare_send`] and [`complete_send`] bundle those steps so the CLI and the desktop bridge
//! run the same sequence; the bridge keeps the PSBT in Rust and the UI only sees the
//! [`SendPreview`].
//!
//! Guarantees:
//! - The preview accounts for every output: the recipient's, and the rest must be this wallet's
//!   change. Anything else is an error.
//! - [`sign_psbt`] fails unless every input was signed and finalized.
//! - Every error before a successful broadcast calls [`cancel`], so a failed send never burns a
//!   change address.
//!
//! Fee bumps ([`prepare_fee_bump`]) reuse the same [`preview`], [`complete_send`] and
//! [`cancel`]; the BIP125 rules and fee math are in the "Fee bump" section below.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bdk_wallet::chain::ChainPosition;
use bdk_wallet::coin_selection::InsufficientFunds;
use bdk_wallet::error::CreateTxError;
use bdk_wallet::{KeychainKind, SignOptions};

use crate::bitcoin::address::NetworkUnchecked;
use crate::bitcoin::psbt::ExtractTxError;
use crate::bitcoin::{
    Address, Amount, FeeRate, Network, OutPoint, Psbt, ScriptBuf, Transaction, TxOut, Txid, Weight,
};
use crate::chain::Node;
use crate::error::{Result, WalletError};
use crate::keys::Signer;
use crate::types::{SendPreview, TxStatus};
use crate::wallet::WalletService;

/// Dust threshold for a P2WPKH output at the default relay fee.
pub const DUST_LIMIT_P2WPKH: Amount = Amount::from_sat(294);

/// Confirmation target (in blocks, about an hour) for the node's fee estimate.
pub const FEE_TARGET_BLOCKS: u16 = 6;

/// BIP144 serializes a transaction with witnesses as `version | 0x00 marker | 0x01 flag | ...`.
/// The two extra bytes are witness data, so they weigh 1 WU each rather than 4.
const SEGWIT_MARKER_AND_FLAG: Weight = Weight::from_wu(2);

/// Once a transaction has witnesses, *every* input gets a witness field that starts with a
/// varint item count (1 byte for fewer than 253 items). An unsigned transaction has no witness
/// fields at all, so this byte is missing from its weight.
const WITNESS_ITEM_COUNT: Weight = Weight::from_wu(1);

/// Longest piece of user input echoed back in an `InvalidAddress` error.
const MAX_ECHOED_INPUT: usize = 100;

// ── Address ─────────────────────────────────────────────────────────────────────────────────

/// Parse an address and require it to be for `network` (`InvalidAddress` / `NetworkMismatch`).
pub fn parse_address(s: &str, network: Network) -> Result<Address> {
    let s = s.trim();
    let unchecked: Address<NetworkUnchecked> = s.parse().map_err(|e| {
        // Addresses are public, so echoing the input is fine; it is capped so a stray paste of
        // a whole document doesn't become a whole-document error message.
        let shown: String = s.chars().take(MAX_ECHOED_INPUT).collect();
        let ellipsis = if shown.len() < s.len() { "…" } else { "" };
        WalletError::InvalidAddress(format!("`{shown}{ellipsis}`: {e}"))
    })?;
    let found = networks_of(&unchecked);
    unchecked
        .require_network(network)
        .map_err(|_| WalletError::NetworkMismatch {
            expected: network,
            found,
        })
}

/// Which supported networks an address is valid on, e.g. `"a testnet4/signet address"`
/// (testnet4 and signet share the `tb1` prefix).
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

// ── Build and preview ───────────────────────────────────────────────────────────────────────

/// Unsigned PSBT paying `amount` to `to` at `fee_rate`; BDK picks coins and adds change.
///
/// BDK's defaults are kept: change to the next unused internal address, RBF signalled on every
/// input (`0xFFFFFFFD`), and `nLockTime` at the synced tip (anti fee-sniping). Building marks
/// the change address used in memory; [`cancel`] undoes that.
pub fn build_psbt(
    wallet: &mut WalletService,
    to: &Address,
    amount: Amount,
    fee_rate: FeeRate,
) -> Result<Psbt> {
    // A checked `Address` only records *that* it was checked, not for which network.
    let network = wallet.network();
    if !to.as_unchecked().is_valid_for_network(network) {
        return Err(WalletError::NetworkMismatch {
            expected: network,
            found: networks_of(to.as_unchecked()),
        });
    }

    check_amount(to, amount)?;
    check_fee_rate(fee_rate)?;

    let mut builder = wallet.bdk_mut().build_tx();
    builder
        .add_recipient(to.script_pubkey(), amount)
        .fee_rate(fee_rate);
    builder.finish().map_err(|e| create_tx_error(e, amount))
}

/// The dust rule [`build_psbt`] applies, exposed so a frontend can check it before unlocking.
///
/// Dust is an output worth less than it costs to spend at 3 sat/vB: 294 sat for P2WPKH, 546 for
/// P2PKH, 330 for taproot.
pub fn check_amount(to: &Address, amount: Amount) -> Result<()> {
    if amount < DUST_LIMIT_P2WPKH.max(to.script_pubkey().minimal_non_dust()) {
        return Err(WalletError::DustAmount(amount));
    }
    Ok(())
}

/// The fee-rate rule [`build_psbt`] applies, exposed so a frontend can check it before unlocking.
///
/// Below 1 sat/vB (`minrelaytxfee`) nodes drop the transaction; above 25 000 sat/vB
/// rust-bitcoin's `extract_tx` refuses it.
pub fn check_fee_rate(rate: FeeRate) -> Result<()> {
    if rate < FeeRate::BROADCAST_MIN {
        return Err(WalletError::TxBuild(format!(
            "fee rate {} sat/vB is below the 1 sat/vB minimum that nodes relay",
            sat_per_vb(rate)
        )));
    }
    if rate > Psbt::DEFAULT_MAX_FEE_RATE {
        return Err(WalletError::TxBuild(format!(
            "fee rate {} sat/vB is above the {} sat/vB safety limit",
            sat_per_vb(rate),
            sat_per_vb(Psbt::DEFAULT_MAX_FEE_RATE)
        )));
    }
    Ok(())
}

/// Summarise a built PSBT for the confirmation step.
///
/// Fails unless the PSBT pays exactly `amount` to `to` and every other output is this wallet's
/// change, so the preview can never hide an output.
pub fn preview(
    wallet: &WalletService,
    psbt: &Psbt,
    to: &Address,
    amount: Amount,
) -> Result<SendPreview> {
    check_shape(psbt)?;
    let outputs = &psbt.unsigned_tx.output;
    let to_script = to.script_pubkey();
    let recipient = outputs
        .iter()
        .position(|out| out.script_pubkey == to_script && out.value == amount)
        .ok_or_else(|| {
            WalletError::TxBuild(format!(
                "the transaction does not pay {} sat to {to}",
                amount.to_sat()
            ))
        })?;

    let bdk = wallet.bdk();
    let mut change: Option<Amount> = None;
    for (i, out) in outputs.iter().enumerate() {
        if i == recipient {
            continue;
        }
        match bdk.derivation_of_spk(out.script_pubkey.clone()) {
            Some((KeychainKind::Internal, _)) => {
                let sum = change.unwrap_or(Amount::ZERO).checked_add(out.value);
                change = Some(sum.ok_or_else(|| amount_overflow("change"))?);
            }
            _ => {
                return Err(WalletError::TxBuild(format!(
                    "output {i} pays neither {to} nor this wallet's change; refusing to \
                     summarise a transaction with an unexplained output"
                )));
            }
        }
    }

    // Fee = inputs − outputs; each input's `witness_utxo` gives its value before signing.
    let fee = psbt
        .fee()
        .map_err(|e| WalletError::TxBuild(format!("cannot work out the fee: {e}")))?;
    let vsize = estimated_signed_weight(wallet, psbt)?.to_vbytes_ceil();
    let total = amount
        .checked_add(fee)
        .ok_or_else(|| amount_overflow("amount + fee"))?;
    Ok(SendPreview {
        to: to.to_string(),
        amount_sat: amount.to_sat(),
        fee_sat: fee.to_sat(),
        // Display only; amounts stay integer satoshis.
        fee_rate_sat_vb: fee.to_sat() as f64 / vsize.max(1) as f64,
        vsize,
        change_sat: change.map(Amount::to_sat),
        total_sat: total.to_sat(),
        contact: None,
        replaces: None,
    })
}

/// User declined: un-reserve the change address BDK revealed while building.
///
/// In-memory only (BDK doesn't persist "used" marks). Harmless for an address that was paid:
/// BDK ignores `unmark_used` for scripts with outputs.
pub fn cancel(wallet: &mut WalletService, psbt: &Psbt) {
    let bdk = wallet.bdk_mut();
    for out in &psbt.unsigned_tx.output {
        if let Some((KeychainKind::Internal, index)) =
            bdk.derivation_of_spk(out.script_pubkey.clone())
        {
            // So the next build reuses this index instead of revealing a new one.
            bdk.unmark_used(KeychainKind::Internal, index);
        }
    }
}

/// For a PSBT built before the wallet was closed and reopened: every input must still be an
/// unspent coin of this wallet, and no change address may have been paid since.
///
/// Another process can spend those coins or take that change address while the preview is on
/// screen; signing anyway would RBF-replace its payment or reuse an address.
pub fn check_prepared(wallet: &WalletService, psbt: &Psbt) -> Result<()> {
    let bdk = wallet.bdk();
    for input in &psbt.unsigned_tx.input {
        if bdk.get_utxo(input.previous_output).is_none() {
            return Err(WalletError::TxBuild(format!(
                "coin {} was spent after this payment was prepared (by another payment, \
                 perhaps from btcw in a terminal); review the payment again",
                input.previous_output
            )));
        }
    }
    check_change_unused(wallet, psbt, None)
}

/// No change output of `psbt` may pay an address that a transaction other than `replaces` (the
/// payment a fee bump replaces, which legitimately paid the same change address) has paid.
fn check_change_unused(wallet: &WalletService, psbt: &Psbt, replaces: Option<Txid>) -> Result<()> {
    let bdk = wallet.bdk();
    for out in &psbt.unsigned_tx.output {
        let is_change = matches!(
            bdk.derivation_of_spk(out.script_pubkey.clone()),
            Some((KeychainKind::Internal, _))
        );
        if is_change
            && bdk.list_output().any(|output| {
                output.txout.script_pubkey == out.script_pubkey
                    && Some(output.outpoint.txid) != replaces
            })
        {
            return Err(WalletError::TxBuild(
                "the change address of this payment was used by another payment after it was \
                 prepared; review the payment again"
                    .into(),
            ));
        }
    }
    Ok(())
}

// ── Sign, extract, record ───────────────────────────────────────────────────────────────────

/// Sign with `signer`, then finalize with BDK. Fails unless every input ends up finalized.
///
/// rust-bitcoin's `Psbt::sign` makes the signatures (BDK 3.2 deprecated its own signers), and
/// BDK, which knows the descriptors, builds the witnesses.
pub fn sign_psbt(wallet: &WalletService, signer: &Signer, psbt: &mut Psbt) -> Result<()> {
    check_shape(psbt)?;
    let inputs = psbt.inputs.len();
    let signed = signer.sign_psbt(psbt)?;
    if signed != inputs {
        return Err(WalletError::Sign(format!(
            "this wallet's key signed {signed} of {inputs} inputs; the transaction spends coins \
             this seed does not control"
        )));
    }
    let finalized = wallet
        .bdk()
        .finalize_psbt(psbt, SignOptions::default())
        .map_err(|e| WalletError::Sign(format!("finalizing the transaction: {e}")))?;
    if !finalized {
        return Err(WalletError::Sign(
            "every input was signed, but not every input could be finalized".into(),
        ));
    }
    Ok(())
}

/// Extract the network-ready transaction (rejects absurd fee rates).
pub fn extract_tx(psbt: Psbt) -> Result<Transaction> {
    check_shape(&psbt)?;
    if let Some(i) = psbt
        .inputs
        .iter()
        .position(|input| input.final_script_witness.is_none() && input.final_script_sig.is_none())
    {
        return Err(WalletError::Sign(format!(
            "input {i} is not signed and finalized; call sign_psbt first"
        )));
    }
    // `Psbt::extract_tx` panics on one fee error (a `non_witness_utxo` missing the spent
    // output); computing the fee first turns it into an error.
    psbt.fee()
        .map_err(|e| WalletError::TxBuild(format!("cannot work out the fee: {e}")))?;
    // Keeps rust-bitcoin's absurd-fee check (> 25 000 sat/vB).
    psbt.extract_tx().map_err(|e| match e {
        ExtractTxError::AbsurdFeeRate { fee_rate, .. } => WalletError::TxBuild(format!(
            "refusing to extract a transaction paying {} sat/vB (the limit is {} sat/vB)",
            sat_per_vb(fee_rate),
            sat_per_vb(Psbt::DEFAULT_MAX_FEE_RATE)
        )),
        other => WalletError::TxBuild(format!("cannot extract the transaction: {other}")),
    })
}

/// After a successful broadcast: add the tx as unconfirmed and persist, so the balance reflects
/// it before the next sync. A fee bump's replacement takes over the original's label.
pub fn record_broadcast(wallet: &mut WalletService, tx: &Transaction) -> Result<()> {
    let txid = tx.compute_txid();
    // BDK picks between conflicting unconfirmed txs by last-seen time (ties go to the higher
    // txid), so seen time must be strictly after anything this tx replaces, even within the
    // same second.
    let replaced = conflicting_txids(wallet, tx);
    let graph = wallet.bdk().tx_graph();
    let seen = replaced
        .iter()
        .filter_map(|old| graph.get_tx_node(*old)?.last_seen)
        .map(|last| last.saturating_add(1))
        .fold(unix_now(), u64::max);
    wallet
        .bdk_mut()
        .apply_unconfirmed_txs([(Arc::new(tx.clone()), seen)]);
    wallet.inherit_label(txid, &replaced);
    wallet.persist()
}

/// Transactions in the wallet's graph that spend a coin `tx` also spends (other than `tx`).
fn conflicting_txids(wallet: &WalletService, tx: &Transaction) -> Vec<Txid> {
    let txid = tx.compute_txid();
    let graph = wallet.bdk().tx_graph();
    let mut conflicts: Vec<Txid> = tx
        .input
        .iter()
        .flat_map(|input| graph.outspends(input.previous_output).iter().copied())
        .filter(|other| *other != txid)
        .collect();
    conflicts.sort_unstable();
    conflicts.dedup();
    conflicts
}

/// Status from the wallet's view (sync first for fresh data). `None` if unknown or no longer
/// canonical (e.g. replaced).
pub fn tx_status(wallet: &WalletService, txid: Txid) -> Option<TxStatus> {
    let wtx = wallet.bdk().get_tx(txid)?;
    Some(crate::wallet::tx_status(
        &wtx.chain_position,
        wallet.synced_height(),
    ))
}

// ── One code path for both frontends ────────────────────────────────────────────────────────

/// Resolve `to` (address or contact name), pick the fee rate (else the node's estimate for
/// [`FEE_TARGET_BLOCKS`]), build the PSBT and preview it.
///
/// Sync first so coin selection sees current UTXOs. Then [`complete_send`] or [`cancel`].
pub fn prepare_send(
    wallet: &mut WalletService,
    node: &Node,
    to: &str,
    amount_sat: u64,
    fee_rate: Option<FeeRate>,
) -> Result<(Psbt, SendPreview)> {
    let recipient = wallet.resolve_recipient(to)?;
    let amount = Amount::from_sat(amount_sat);
    let fee_rate = match fee_rate {
        Some(rate) => rate,
        None => node.estimate_fee_rate(FEE_TARGET_BLOCKS)?,
    };
    let psbt = build_psbt(wallet, &recipient.address, amount, fee_rate)?;
    match preview(wallet, &psbt, &recipient.address, amount) {
        Ok(mut preview) => {
            preview.contact = recipient.contact;
            Ok((psbt, preview))
        }
        Err(e) => {
            cancel(wallet, &psbt);
            Err(e)
        }
    }
}

/// The user confirmed: [`sign_psbt`], then [`broadcast_signed`]. Returns the txid.
pub fn complete_send(
    wallet: &mut WalletService,
    signer: &Signer,
    node: &Node,
    mut psbt: Psbt,
) -> Result<Txid> {
    if let Err(e) = sign_psbt(wallet, signer, &mut psbt) {
        cancel(wallet, &psbt);
        return Err(e);
    }
    broadcast_signed(wallet, node, psbt)
}

/// Second half of [`complete_send`], for callers that drop the [`Signer`] right after signing.
///
/// Once the node has accepted the transaction, a failure to record it is only logged: the next
/// sync finds it in the mempool anyway.
pub fn broadcast_signed(wallet: &mut WalletService, node: &Node, psbt: Psbt) -> Result<Txid> {
    // Copy: on failure the outputs are needed to release the change address.
    let tx = match extract_tx(psbt.clone()) {
        Ok(tx) => tx,
        Err(e) => {
            cancel(wallet, &psbt);
            return Err(e);
        }
    };
    let txid = match node.broadcast(&tx) {
        Ok(txid) => txid,
        Err(e) => {
            // If the node accepted it anyway (reply timed out), the next sync re-marks the change.
            cancel(wallet, &psbt);
            return Err(e);
        }
    };
    if let Err(e) = record_broadcast(wallet, &tx) {
        tracing::warn!(
            %txid,
            error = %e,
            "the transaction was broadcast but could not be saved to the wallet database; \
             the next sync will pick it up"
        );
    }
    Ok(txid)
}

// ── Fee bump (RBF) ──────────────────────────────────────────────────────────────────────────
//
// A replacement spends the same coins, pays the same recipient the same amount, and takes a
// higher fee out of the change. BIP125 rules (Core's `policy/rbf.cpp`):
//
//   1. The original signals RBF (an input with nSequence < 0xFFFFFFFE). Ours always do (BDK's
//      0xFFFFFFFD). Core 28+ does full RBF, but other nodes go by the signal, so we require it.
//   2. No new *unconfirmed* inputs. BDK only adds confirmed coins; we check again anyway.
//   3. Pays at least the absolute fee of everything evicted. We refuse payments with unconfirmed
//      descendants, so that is the original's fee.
//   4. New fee − old fee ≥ incremental relay fee × replacement size. We use 1 sat/vB (newer Core
//      lowers it to 0.1).
//   5. At most 100 evictions: trivially true here.
//
//   Core also wants a higher fee rate; BDK's `build_fee_bump` requires old rate + 1 sat/vB,
//   with the old rate = fee ÷ signed weight, rounded down to sat/kwu.
//
// Worked example (1 P2WPKH input, recipient + change; 562 WU = 141 vB estimated):
//
//   original at 2 sat/vB (500 sat/kwu): fee = ⌈500 × 562 / 1000⌉ = 281 sat. Signed, it weighs
//     561 or 562 WU (signatures vary by a byte), so its rate is ⌊281 000 / 561⌋ = 500 sat/kwu.
//   BDK's minimum:  500 + 250 = 750 sat/kwu (3 sat/vB).
//   rule 4 minimum: the replacement has the same 562 WU (141 vB), so it needs a fee of at least
//                   281 + 1 × 141 = 422 sat: ⌈422 000 / 562⌉ = 751 sat/kwu (3.004 sat/vB).
//   [`min_fee_bump_rate`] is the larger, 751; shown rounded up as 3.01 sat/vB.
//   at 5 sat/vB:  fee = ⌈1250 × 562 / 1000⌉ = 703 sat ≥ 422 ✓; the change shrinks by
//                 703 − 281 = 422 sat, the recipient still gets the same amount, and the
//                 wallet's balance drops by exactly those 422 sat.
//
// The replacement keeps the original's change address (`drain_to`); only one of the two can
// confirm, so nothing new is linked. Without change, BDK adds one at a fresh address.

/// Incremental relay fee rate for BIP125 rule 4 (see above): 1 sat/vB.
pub const INCREMENTAL_RELAY_FEE: FeeRate = FeeRate::BROADCAST_MIN;

/// What [`bump_target`] found out about a payment that may be replaced.
struct BumpTarget {
    txid: Txid,
    tx: Arc<Transaction>,
    fee: Amount,
    /// The original's fee rate as BDK measures it: fee ÷ signed weight, rounded down.
    rate: FeeRate,
    /// The one output that is not change: who gets paid, and how much. Kept as is.
    recipient: TxOut,
    /// The original's change output script, reused by the replacement.
    change: Option<ScriptBuf>,
    /// The lowest rate that satisfies both BDK (`rate` + 1 sat/vB) and BIP125 rule 4.
    min_rate: FeeRate,
}

/// Every reason a transaction can't be replaced, checked up front so each gets its own message.
fn bump_target(wallet: &WalletService, txid: Txid) -> Result<BumpTarget> {
    let bdk = wallet.bdk();
    let refuse = |why: String| WalletError::TxBuild(format!("cannot speed up {txid}: {why}"));
    let Some(wtx) = bdk.get_tx(txid) else {
        // Still in the graph but not canonical: a conflicting transaction won.
        return Err(if bdk.tx_graph().get_tx(txid).is_some() {
            refuse("it was replaced by another transaction or dropped from the mempool".into())
        } else {
            WalletError::TxNotFound(txid.to_string())
        });
    };
    if let ChainPosition::Confirmed { anchor, .. } = &wtx.chain_position {
        return Err(refuse(format!(
            "it is already confirmed (block {}); only an unconfirmed payment can be replaced",
            anchor.block_id.height
        )));
    }
    let tx = Arc::clone(&wtx.tx_node.tx);

    // Ours: we must be able to sign every input of the replacement.
    let graph = bdk.tx_graph();
    let ours = tx
        .input
        .iter()
        .filter(|input| {
            graph
                .get_txout(input.previous_output)
                .and_then(|prev| bdk.derivation_of_spk(prev.script_pubkey.clone()))
                .is_some()
        })
        .count();
    if ours == 0 {
        return Err(refuse(
            "it was not sent by this wallet (it spends none of this wallet's coins), so only \
             its sender can replace it"
                .into(),
        ));
    }
    if ours < tx.input.len() {
        return Err(refuse(format!(
            "{} of its {} inputs are not this wallet's coins, so this wallet can't sign a \
             replacement",
            tx.input.len() - ours,
            tx.input.len()
        )));
    }

    // BIP125 rule 1.
    if !tx.input.iter().any(|input| input.sequence.is_rbf()) {
        return Err(refuse(
            "it does not signal replace-by-fee (BIP125: no input has an nSequence below \
             0xFFFFFFFE)"
                .into(),
        ));
    }

    // BIP125 rules 3 and 5: no unconfirmed descendants (a confirmed child would mean the
    // original is confirmed too).
    let canonical: HashSet<Txid> = bdk.transactions().map(|t| t.tx_node.txid).collect();
    for (vout, spenders) in graph.tx_spends(txid) {
        if let Some(child) = spenders.iter().find(|child| canonical.contains(*child)) {
            return Err(refuse(format!(
                "payment {child} spends its output {txid}:{vout}, and replacing {txid} would \
                 cancel that payment too"
            )));
        }
    }

    let fee = bdk
        .calculate_fee(&tx)
        .map_err(|e| refuse(format!("its fee is unknown: {e}")))?;

    // The shape btcw's own payments have: one recipient, at most one change output.
    let mut recipients = Vec::new();
    let mut change = Vec::new();
    for out in &tx.output {
        match bdk.derivation_of_spk(out.script_pubkey.clone()) {
            Some((KeychainKind::Internal, _)) => change.push(out.script_pubkey.clone()),
            _ => recipients.push(out.clone()),
        }
    }
    let [recipient] = <[TxOut; 1]>::try_from(recipients).map_err(|recipients| {
        refuse(format!(
            "it pays {} outputs besides its change; only a payment to a single recipient can be \
             sped up",
            recipients.len()
        ))
    })?;
    if change.len() > 1 {
        return Err(refuse(format!(
            "it has {} change outputs; only a payment with at most one can be sped up",
            change.len()
        )));
    }

    // The minimum rate is the larger of:
    // - BDK's: the original's rate + 1 sat/vB (fee ÷ signed weight, rounded down);
    // - BIP125 rule 4 for a same-size replacement: (old fee + 1 sat/vB × vsize) ÷ weight,
    //   rounded up. BDK's alone can fall a satoshi short through rounding: 562 WU at 703 sat
    //   gives ⌊703 000 / 562⌋ + 250 = 1 500 sat/kwu → 843 sat, but Core wants 703 + 141 = 844.
    let overflow = || refuse("its fee rate can't be worked out".into());
    let rate_kwu = fee
        .to_sat()
        .checked_mul(1_000)
        .and_then(|sat| sat.checked_div(tx.weight().to_wu()))
        .ok_or_else(overflow)?;
    let rate = FeeRate::from_sat_per_kwu(rate_kwu);
    let bdk_min = rate_kwu.saturating_add(FeeRate::BROADCAST_MIN.to_sat_per_kwu());
    let estimate = estimated_weight_of_sent(wallet, &tx)?;
    let rule4_min = INCREMENTAL_RELAY_FEE
        .fee_vb(estimate.to_vbytes_ceil())
        .and_then(|relay| fee.checked_add(relay))
        .and_then(|needed| needed.to_sat().checked_mul(1_000))
        .and_then(|needed| needed.checked_add(estimate.to_wu().saturating_sub(1)))
        .and_then(|needed| needed.checked_div(estimate.to_wu()))
        .ok_or_else(overflow)?;
    let min_rate = FeeRate::from_sat_per_kwu(bdk_min.max(rule4_min));
    Ok(BumpTarget {
        txid,
        tx,
        fee,
        rate,
        recipient,
        change: change.pop(),
        min_rate,
    })
}

/// The lowest fee rate [`build_fee_bump`] accepts for `txid`; fails like it if `txid` can't be
/// replaced at all.
pub fn min_fee_bump_rate(wallet: &WalletService, txid: Txid) -> Result<FeeRate> {
    bump_target(wallet, txid).map(|target| target.min_rate)
}

/// Unsigned PSBT replacing the unconfirmed payment `txid` at `fee_rate`: same inputs, recipient
/// and change address, more fee from the change (plus a confirmed coin if needed).
///
/// Like [`build_psbt`], may reserve a change address; [`cancel`] releases it.
pub fn build_fee_bump(wallet: &mut WalletService, txid: Txid, fee_rate: FeeRate) -> Result<Psbt> {
    check_fee_rate(fee_rate)?;
    let target = bump_target(wallet, txid)?;
    if fee_rate < target.min_rate {
        return Err(rate_too_low(&target, fee_rate, target.min_rate));
    }

    let mut builder = wallet
        .bdk_mut()
        .build_fee_bump(txid)
        // Unreachable after `bump_target`'s checks; BDK's messages hold no secrets.
        .map_err(|e| WalletError::TxBuild(format!("cannot speed up {txid}: {e}")))?;
    builder.fee_rate(fee_rate);
    if let Some(change) = &target.change {
        builder.drain_to(change.clone());
    }
    let psbt = builder.finish().map_err(|e| match e {
        CreateTxError::FeeRateTooLow { required } => rate_too_low(&target, fee_rate, required),
        other => create_tx_error(other, target.recipient.value),
    })?;

    if let Err(e) = check_replacement(wallet, &psbt, &target) {
        cancel(wallet, &psbt);
        return Err(e);
    }
    Ok(psbt)
}

/// [`preview`] for a fee bump, with [`SendPreview::replaces`] set. Fails if the PSBT doesn't
/// replace `txid` under the rules above.
pub fn preview_fee_bump(wallet: &WalletService, psbt: &Psbt, txid: Txid) -> Result<SendPreview> {
    let target = bump_target(wallet, txid)?;
    check_replacement(wallet, psbt, &target)?;
    let to =
        Address::from_script(&target.recipient.script_pubkey, wallet.network()).map_err(|e| {
            WalletError::TxBuild(format!("the recipient of {txid} has no address form: {e}"))
        })?;
    let mut summary = preview(wallet, psbt, &to, target.recipient.value)?;
    summary.replaces = Some(txid.to_string());
    Ok(summary)
}

/// The fee-bump twin of [`prepare_send`]. Without `fee_rate`, uses the node's estimate raised to
/// at least the minimum and rounded up to a whole sat/vB.
pub fn prepare_fee_bump(
    wallet: &mut WalletService,
    node: &Node,
    txid: Txid,
    fee_rate: Option<FeeRate>,
) -> Result<(Psbt, SendPreview)> {
    let fee_rate = match fee_rate {
        Some(rate) => rate,
        None => {
            let min = min_fee_bump_rate(wallet, txid)?;
            let whole = FeeRate::from_sat_per_vb_u32(
                u32::try_from(min.to_sat_per_vb_ceil()).unwrap_or(u32::MAX),
            );
            node.estimate_fee_rate(FEE_TARGET_BLOCKS)?.max(whole)
        }
    };
    let psbt = build_fee_bump(wallet, txid, fee_rate)?;
    match preview_fee_bump(wallet, &psbt, txid) {
        Ok(preview) => Ok((psbt, preview)),
        Err(e) => {
            cancel(wallet, &psbt);
            Err(e)
        }
    }
}

/// [`check_prepared`] for a fee bump: `replaces` must still be replaceable and `psbt` still a
/// valid replacement. (Its inputs and change are already used by `replaces`, so the plain check
/// would refuse it.)
pub fn check_prepared_bump(wallet: &WalletService, psbt: &Psbt, replaces: Txid) -> Result<()> {
    let target = bump_target(wallet, replaces)?;
    check_replacement(wallet, psbt, &target)?;
    check_change_unused(wallet, psbt, Some(replaces))
}

/// `psbt` replaces `target` under BIP125 rules 2–4.
fn check_replacement(wallet: &WalletService, psbt: &Psbt, target: &BumpTarget) -> Result<()> {
    check_shape(psbt)?;
    let txid = target.txid;
    let spent: HashSet<OutPoint> = psbt
        .unsigned_tx
        .input
        .iter()
        .map(|input| input.previous_output)
        .collect();
    let original: HashSet<OutPoint> = target
        .tx
        .input
        .iter()
        .map(|input| input.previous_output)
        .collect();
    if let Some(missing) = original.iter().find(|coin| !spent.contains(*coin)) {
        return Err(WalletError::TxBuild(format!(
            "this transaction does not replace {txid}: it doesn't spend its coin {missing}"
        )));
    }
    let bdk = wallet.bdk();
    for coin in spent.difference(&original) {
        match bdk.get_utxo(*coin) {
            Some(utxo) if utxo.chain_position.is_confirmed() => {}
            Some(_) => {
                return Err(WalletError::TxBuild(format!(
                    "the replacement for {txid} would add the unconfirmed coin {coin}, which \
                     BIP125 doesn't allow"
                )));
            }
            None => {
                return Err(WalletError::TxBuild(format!(
                    "coin {coin} is no longer an unspent coin of this wallet (spent after this \
                     fee bump was prepared?); review the fee bump again"
                )));
            }
        }
    }

    let fee = psbt
        .fee()
        .map_err(|e| WalletError::TxBuild(format!("cannot work out the fee: {e}")))?;
    let vsize = estimated_signed_weight(wallet, psbt)?.to_vbytes_ceil();
    let needed = INCREMENTAL_RELAY_FEE
        .fee_vb(vsize)
        .and_then(|relay| target.fee.checked_add(relay))
        .ok_or_else(|| amount_overflow("the replacement's minimum fee"))?;
    if fee < needed {
        return Err(WalletError::TxBuild(format!(
            "the replacement for {txid} pays {} sat in fees, but it must pay at least {} sat: \
             the {} sat the original pays, plus 1 sat/vB for its own {vsize} vB (BIP125); \
             choose a higher fee rate",
            fee.to_sat(),
            needed.to_sat(),
            target.fee.to_sat()
        )));
    }
    Ok(())
}

fn rate_too_low(target: &BumpTarget, asked: FeeRate, required: FeeRate) -> WalletError {
    // The minimum is rounded *up* to the cent, so the rate shown is always enough.
    let required_cents = required.to_sat_per_kwu().saturating_mul(2).div_ceil(5);
    WalletError::TxBuild(format!(
        "cannot speed up {}: a fee rate of {} sat/vB is too low; it pays {:.2} sat/vB now, and a \
         replacement must pay at least 1 sat/vB more (BIP125), so at least {}.{:02} sat/vB",
        target.txid,
        sat_per_vb(asked),
        sat_per_vb(target.rate),
        required_cents / 100,
        required_cents % 100
    ))
}

// ── Helpers ─────────────────────────────────────────────────────────────────────────────────

/// Weight once every input is signed, assuming the largest signatures: the unsigned weight plus
/// what signing adds.
///
/// ```text
/// unsigned weight   4 WU per byte (no witness → serialized without the segwit marker/flag)
/// + 2 WU            segwit marker + flag; witness bytes count 1 WU each (the witness discount)
/// + per input:
///     1 WU          the witness item-count varint
///     + satisfaction weight from the descriptor (miniscript's `max_weight_to_satisfy`):
///       P2WPKH = 1 + 72 (DER signature ≤ 71 bytes with low-S, + sighash byte)  signature push
///              + 1 + 33 (compressed public key)                                key push
///              = 107 WU
/// ```
/// Signatures are often a byte shorter, so this is an upper bound (0–1 WU per input over): the
/// rate paid is never below the one shown. BDK's coin selection sizes inputs the same way.
fn estimated_signed_weight(wallet: &WalletService, psbt: &Psbt) -> Result<Weight> {
    let tx = &psbt.unsigned_tx;
    if tx.input.is_empty() {
        return Err(WalletError::TxBuild("the transaction has no inputs".into()));
    }
    let mut weight = tx.weight() + SEGWIT_MARKER_AND_FLAG;
    for (i, utxo) in psbt.iter_funding_utxos().enumerate() {
        let utxo = utxo.map_err(|e| WalletError::TxBuild(format!("input {i}: {e}")))?;
        weight += input_witness_weight(wallet, i, &utxo.script_pubkey)?;
    }
    Ok(weight)
}

/// The same estimate for an already signed transaction, comparable with
/// [`estimated_signed_weight`] (real signatures vary by a byte).
fn estimated_weight_of_sent(wallet: &WalletService, tx: &Transaction) -> Result<Weight> {
    let mut unsigned = tx.clone();
    for input in &mut unsigned.input {
        input.witness.clear();
    }
    let graph = wallet.bdk().tx_graph();
    let mut weight = unsigned.weight() + SEGWIT_MARKER_AND_FLAG;
    for (i, input) in tx.input.iter().enumerate() {
        let prev = graph.get_txout(input.previous_output).ok_or_else(|| {
            WalletError::TxBuild(format!("input {i}: the coin it spends is unknown"))
        })?;
        weight += input_witness_weight(wallet, i, &prev.script_pubkey)?;
    }
    Ok(weight)
}

/// Witness item count + the descriptor's largest satisfaction, for input `i` spending `script`.
fn input_witness_weight(wallet: &WalletService, i: usize, script: &ScriptBuf) -> Result<Weight> {
    let bdk = wallet.bdk();
    let Some((keychain, _)) = bdk.derivation_of_spk(script.clone()) else {
        return Err(WalletError::TxBuild(format!(
            "input {i} does not spend one of this wallet's coins"
        )));
    };
    let satisfaction = bdk
        .public_descriptor(keychain)
        .max_weight_to_satisfy()
        .map_err(|e| WalletError::TxBuild(format!("input {i}: cannot size its witness: {e}")))?;
    Ok(WITNESS_ITEM_COUNT + satisfaction)
}

/// rust-bitcoin's PSBT helpers assert one input map per input; check first so a malformed PSBT
/// errors instead of panicking.
fn check_shape(psbt: &Psbt) -> Result<()> {
    let tx = &psbt.unsigned_tx;
    if psbt.inputs.len() != tx.input.len() || psbt.outputs.len() != tx.output.len() {
        return Err(WalletError::TxBuild(format!(
            "malformed PSBT: {} input maps for {} inputs, {} output maps for {} outputs",
            psbt.inputs.len(),
            tx.input.len(),
            psbt.outputs.len(),
            tx.output.len()
        )));
    }
    Ok(())
}

/// sat/kwu → sat/vB (÷ 250), for messages only.
fn sat_per_vb(rate: FeeRate) -> f64 {
    rate.to_sat_per_kwu() as f64 / 250.0
}

fn create_tx_error(e: CreateTxError, amount: Amount) -> WalletError {
    match e {
        CreateTxError::CoinSelection(InsufficientFunds { needed, available }) => {
            WalletError::InsufficientFunds { needed, available }
        }
        // BDK's own dust rule; `build_psbt` checks the same threshold first.
        CreateTxError::OutputBelowDustLimit(_) => WalletError::DustAmount(amount),
        // Built from public descriptors and our own parameters: nothing secret to leak.
        other => WalletError::TxBuild(other.to_string()),
    }
}

fn amount_overflow(what: &str) -> WalletError {
    WalletError::TxBuild(format!("{what} exceeds the largest possible amount"))
}

/// Seconds since the Unix epoch (0 if the clock is set before 1970).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Offline tests on a wallet funded with made-up unconfirmed transactions. The node-backed
/// flow is in `tests/tx.rs`.
#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::str::FromStr;

    use super::*;
    use crate::bitcoin::absolute::LockTime;
    use crate::bitcoin::hashes::Hash;
    use crate::bitcoin::transaction::Version;
    use crate::bitcoin::{CompressedPublicKey, OutPoint, Sequence, TxIn, TxOut};
    use crate::config::{Config, Overrides};
    use crate::keys::{self, WordCount};
    use crate::types::{BalanceView, Keychain};

    type TestResult<T = ()> = std::result::Result<T, Box<dyn Error>>;

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    /// The generator point G: a public key nobody in these tests holds the key for.
    const STRANGER_PUBKEY: &str =
        "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    struct Funded {
        wallet: WalletService,
        signer: Signer,
        _dir: tempfile::TempDir,
    }

    /// A regtest wallet (the `abandon … about` seed) holding one unconfirmed coin per value.
    fn funded(values: &[u64]) -> TestResult<Funded> {
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
        let mut wallet = WalletService::create(&cfg, &descriptors, Some(0))?;
        for (n, &sat) in values.iter().enumerate() {
            let to = parse_address(&wallet.new_address()?.address, Network::Regtest)?;
            let byte = u8::try_from(n + 1)?;
            let funding = Transaction {
                version: Version::TWO,
                lock_time: LockTime::ZERO,
                input: vec![TxIn {
                    previous_output: OutPoint::new(Txid::from_byte_array([byte; 32]), 0),
                    ..Default::default()
                }],
                output: vec![TxOut {
                    value: Amount::from_sat(sat),
                    script_pubkey: to.script_pubkey(),
                }],
            };
            wallet
                .bdk_mut()
                .apply_unconfirmed_txs([(funding, 1_700_000_000)]);
        }
        Ok(Funded {
            wallet,
            signer,
            _dir: dir,
        })
    }

    fn stranger_p2wpkh() -> TestResult<Address> {
        let key = CompressedPublicKey::from_str(STRANGER_PUBKEY)?;
        Ok(Address::p2wpkh(&key, Network::Regtest))
    }

    fn rate(sat_per_vb: u64) -> TestResult<FeeRate> {
        FeeRate::from_sat_per_vb(sat_per_vb).ok_or_else(|| "fee rate overflow".into())
    }

    /// The change output's derivation index (there must be exactly one change output).
    fn change_index(wallet: &WalletService, psbt: &Psbt) -> TestResult<u32> {
        let indexes: Vec<u32> = psbt
            .unsigned_tx
            .output
            .iter()
            .filter_map(|out| wallet.bdk().derivation_of_spk(out.script_pubkey.clone()))
            .filter(|(keychain, _)| *keychain == KeychainKind::Internal)
            .map(|(_, index)| index)
            .collect();
        match indexes[..] {
            [index] => Ok(index),
            _ => Err(format!("expected one change output, got {indexes:?}").into()),
        }
    }

    fn expect_err<T>(result: Result<T>) -> TestResult<WalletError> {
        match result {
            Ok(_) => Err("expected an error, got Ok".into()),
            Err(e) => Ok(e),
        }
    }

    #[test]
    fn addresses_must_parse_and_match_the_network() -> TestResult {
        // BIP84 test phrase, m/84'/1'/0'/0/0 on regtest and testnet4, m/84'/0'/0'/0/0 on mainnet.
        let regtest = "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk";
        let testnet = "tb1q6rz28mcfaxtmd6v789l9rrlrusdprr9pqcpvkl";
        let mainnet = "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";

        assert_eq!(
            parse_address(regtest, Network::Regtest)?.to_string(),
            regtest
        );
        assert_eq!(
            parse_address(&format!("  {regtest}\n"), Network::Regtest)?.to_string(),
            regtest
        );
        // Bech32 may be written in upper case (QR codes do); the canonical form is lower case.
        assert_eq!(
            parse_address(&regtest.to_uppercase(), Network::Regtest)?.to_string(),
            regtest
        );
        assert_eq!(
            parse_address(testnet, Network::Signet)?.to_string(),
            testnet
        );

        let mismatch = |s: &str, network| -> TestResult<String> {
            match parse_address(s, network) {
                Err(e @ WalletError::NetworkMismatch { .. }) => Ok(e.to_string()),
                other => Err(format!("expected NetworkMismatch, got {other:?}").into()),
            }
        };
        assert_eq!(
            mismatch(testnet, Network::Regtest)?,
            "network mismatch: expected regtest, found a testnet4/signet address"
        );
        assert_eq!(
            mismatch(mainnet, Network::Regtest)?,
            "network mismatch: expected regtest, found a mainnet address"
        );
        assert_eq!(
            mismatch(regtest, Network::Testnet4)?,
            "network mismatch: expected testnet4, found a regtest address"
        );

        for (input, start) in [
            ("not-an-address", "invalid address: `not-an-address`: "),
            // Right prefix, broken checksum.
            (
                "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppl",
                "invalid address: `bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppl`: ",
            ),
            ("", "invalid address: ``: "),
        ] {
            match parse_address(input, Network::Regtest) {
                Err(e @ WalletError::InvalidAddress(_)) => {
                    assert!(e.to_string().starts_with(start), "{e}");
                }
                other => return Err(format!("expected InvalidAddress, got {other:?}").into()),
            }
        }
        // A huge paste is cut short in the message.
        let long = "x".repeat(5_000);
        let msg = expect_err(parse_address(&long, Network::Regtest))?.to_string();
        assert!(msg.len() < 300 && msg.contains("…"), "{msg}");
        Ok(())
    }

    #[test]
    fn build_preview_sign_extract_and_record() -> TestResult {
        // Two coins, and a payment larger than either: the transaction needs both inputs.
        let Funded {
            mut wallet,
            signer,
            _dir,
        } = funded(&[60_000, 70_000])?;
        let to = stranger_p2wpkh()?;
        let amount = Amount::from_sat(100_000);
        let fee_rate = rate(3)?;

        let mut psbt = build_psbt(&mut wallet, &to, amount, fee_rate)?;
        let tx = &psbt.unsigned_tx;
        assert_eq!(tx.input.len(), 2);
        assert_eq!(tx.output.len(), 2, "recipient + change");
        // BDK's defaults, kept on purpose: RBF signalling and anti fee-sniping.
        assert!(
            tx.input
                .iter()
                .all(|txin| txin.sequence == Sequence::ENABLE_RBF_NO_LOCKTIME)
        );
        assert_eq!(tx.input[0].sequence.to_consensus_u32(), 0xFFFF_FFFD);
        assert_eq!(tx.lock_time, LockTime::from_height(wallet.synced_height())?);
        let change_index = change_index(&wallet, &psbt)?;

        let preview = preview(&wallet, &psbt, &to, amount)?;
        let inputs = 130_000;
        let outputs: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
        assert_eq!(preview.to, to.to_string());
        assert_eq!(preview.amount_sat, 100_000);
        assert_eq!(preview.fee_sat, inputs - outputs);
        assert_eq!(preview.total_sat, preview.amount_sat + preview.fee_sat);
        assert_eq!(preview.change_sat, Some(outputs - 100_000));
        // 2 P2WPKH inputs + 2 P2WPKH outputs: 10.5 + 2 × 68 + 2 × 31 = 208.5 → 209 vB.
        assert_eq!(preview.vsize, 209);
        // BDK sized the transaction the same way, so the fee is the rate × vsize, give or take
        // the rounding of each part.
        let target = 3 * preview.vsize;
        assert!(
            (target - 3..=target + 3).contains(&preview.fee_sat),
            "fee {} vs {target}",
            preview.fee_sat
        );
        assert!((preview.fee_rate_sat_vb - 3.0).abs() < 0.05, "{preview:?}");

        sign_psbt(&wallet, &signer, &mut psbt)?;
        let signed = extract_tx(psbt)?;
        assert!(signed.input.iter().all(|txin| txin.witness.len() == 2));
        // The estimate is an upper bound, at most a byte per input over.
        let real = u64::try_from(signed.vsize())?;
        assert!(
            real <= preview.vsize && preview.vsize <= real + 2,
            "estimated {} vB, signed {real} vB",
            preview.vsize
        );
        // So the rate really paid is at least the one shown...
        assert!(preview.fee_sat as f64 / real as f64 >= preview.fee_rate_sat_vb);
        // ...and, per weight unit, at least the one asked for. (Per rounded-up vbyte it can be a
        // hair below: 626 sat / 834 WU = 3.002 sat/vB, but 626 / 209 vB = 2.995.)
        assert!(
            4 * preview.fee_sat >= 3 * signed.weight().to_wu(),
            "{} sat for {}",
            preview.fee_sat,
            signed.weight()
        );

        // Recorded right away: both coins are spent and only the change is left, pending.
        record_broadcast(&mut wallet, &signed)?;
        let change = preview.change_sat.ok_or("no change")?;
        assert_eq!(
            wallet.balance(),
            BalanceView {
                confirmed_sat: 0,
                unconfirmed_sat: change,
                immature_sat: 0,
                total_sat: change,
            }
        );
        let utxos = wallet.utxos();
        assert_eq!(utxos.len(), 1);
        assert_eq!(utxos[0].keychain, Keychain::Internal);
        assert_eq!(utxos[0].derivation_index, change_index);
        assert_eq!(utxos[0].value_sat, change);
        match tx_status(&wallet, signed.compute_txid()) {
            Some(TxStatus::Unconfirmed {
                first_seen: Some(seen),
            }) => assert!(seen.abs_diff(unix_now()) < 60, "{seen}"),
            other => return Err(format!("expected unconfirmed, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn cancel_releases_the_change_address() -> TestResult {
        let Funded {
            mut wallet, _dir, ..
        } = funded(&[100_000])?;
        let to = stranger_p2wpkh()?;
        let amount = Amount::from_sat(10_000);

        let first = build_psbt(&mut wallet, &to, amount, rate(2)?)?;
        let first_change = change_index(&wallet, &first)?;
        // Without a cancel, the next build takes a fresh change address...
        let second = build_psbt(&mut wallet, &to, amount, rate(2)?)?;
        assert_eq!(change_index(&wallet, &second)?, first_change + 1);

        // ...with one, it gets the released address back.
        cancel(&mut wallet, &second);
        cancel(&mut wallet, &first);
        let third = build_psbt(&mut wallet, &to, amount, rate(2)?)?;
        assert_eq!(change_index(&wallet, &third)?, first_change);
        Ok(())
    }

    #[test]
    fn bad_requests_are_refused_before_building() -> TestResult {
        let Funded {
            mut wallet, _dir, ..
        } = funded(&[100_000])?;
        let to = stranger_p2wpkh()?;

        // Dust: 294 sat is the P2WPKH limit; legacy P2PKH outputs need 546.
        assert!(matches!(
            expect_err(build_psbt(&mut wallet, &to, Amount::from_sat(293), rate(2)?))?,
            WalletError::DustAmount(a) if a == Amount::from_sat(293)
        ));
        build_psbt(&mut wallet, &to, DUST_LIMIT_P2WPKH, rate(2)?)?;
        let legacy = Address::p2pkh(
            CompressedPublicKey::from_str(STRANGER_PUBKEY)?,
            Network::Regtest,
        );
        assert!(matches!(
            expect_err(build_psbt(
                &mut wallet,
                &legacy,
                Amount::from_sat(545),
                rate(2)?
            ))?,
            WalletError::DustAmount(_)
        ));
        build_psbt(&mut wallet, &legacy, Amount::from_sat(546), rate(2)?)?;

        // Fee rates below 1 sat/vB aren't relayed; above 25 000 sat/vB extract_tx would refuse.
        for (sat_per_kwu, needle) in [
            (0, "fee rate 0 sat/vB is below the 1 sat/vB minimum"),
            (249, "fee rate 0.996 sat/vB is below the 1 sat/vB minimum"),
            (6_250_001, "above the 25000 sat/vB safety limit"),
        ] {
            let fee_rate = FeeRate::from_sat_per_kwu(sat_per_kwu);
            match expect_err(build_psbt(
                &mut wallet,
                &to,
                Amount::from_sat(10_000),
                fee_rate,
            ))? {
                WalletError::TxBuild(msg) => assert!(msg.contains(needle), "{msg}"),
                other => return Err(format!("expected TxBuild, got {other:?}").into()),
            }
        }
        build_psbt(
            &mut wallet,
            &to,
            Amount::from_sat(10_000),
            FeeRate::BROADCAST_MIN,
        )?;

        // More than the wallet holds: BDK's numbers come through.
        match expect_err(build_psbt(
            &mut wallet,
            &to,
            Amount::from_sat(100_000),
            rate(2)?,
        ))? {
            WalletError::InsufficientFunds { needed, available } => {
                assert_eq!(available, Amount::from_sat(100_000));
                assert!(needed > Amount::from_sat(100_000), "{needed}");
            }
            other => return Err(format!("expected InsufficientFunds, got {other:?}").into()),
        }

        // An address checked for another network can't slip into a regtest wallet.
        let testnet = parse_address(
            "tb1q6rz28mcfaxtmd6v789l9rrlrusdprr9pqcpvkl",
            Network::Testnet4,
        )?;
        assert!(matches!(
            expect_err(build_psbt(
                &mut wallet,
                &testnet,
                Amount::from_sat(10_000),
                rate(2)?
            ))?,
            WalletError::NetworkMismatch {
                expected: Network::Regtest,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn only_this_wallets_key_signs_and_only_finalized_psbts_extract() -> TestResult {
        let Funded {
            mut wallet,
            signer,
            _dir,
        } = funded(&[100_000])?;
        let to = stranger_p2wpkh()?;
        let mut psbt = build_psbt(&mut wallet, &to, Amount::from_sat(10_000), rate(2)?)?;

        let unsigned = psbt.clone();
        match expect_err(extract_tx(unsigned))? {
            WalletError::Sign(msg) => assert!(msg.contains("input 0 is not signed"), "{msg}"),
            other => return Err(format!("expected Sign, got {other:?}").into()),
        }

        let stranger_seed = keys::generate_mnemonic(WordCount::Words12)?;
        let (_, stranger) = keys::derive_account(&stranger_seed, "", Network::Regtest, 0)?;
        match expect_err(sign_psbt(&wallet, &stranger, &mut psbt))? {
            WalletError::Sign(msg) => assert!(msg.contains("signed 0 of 1 inputs"), "{msg}"),
            other => return Err(format!("expected Sign, got {other:?}").into()),
        }
        assert!(
            psbt.inputs
                .iter()
                .all(|input| input.partial_sigs.is_empty())
        );

        sign_psbt(&wallet, &signer, &mut psbt)?;
        extract_tx(psbt)?;
        Ok(())
    }

    #[test]
    fn preview_refuses_what_it_cannot_explain() -> TestResult {
        let Funded {
            mut wallet, _dir, ..
        } = funded(&[100_000])?;
        let to = stranger_p2wpkh()?;
        let amount = Amount::from_sat(10_000);
        let psbt = build_psbt(&mut wallet, &to, amount, rate(2)?)?;
        preview(&wallet, &psbt, &to, amount)?;

        let tx_build = |result: Result<SendPreview>| -> TestResult<String> {
            match result {
                Err(WalletError::TxBuild(msg)) => Ok(msg),
                other => Err(format!("expected TxBuild, got {other:?}").into()),
            }
        };
        // Asked about a different amount or recipient than the PSBT pays.
        let msg = tx_build(preview(&wallet, &psbt, &to, Amount::from_sat(10_001)))?;
        assert!(msg.contains("does not pay 10001 sat to"), "{msg}");
        let own = parse_address(&wallet.new_address()?.address, Network::Regtest)?;
        tx_build(preview(&wallet, &psbt, &own, amount))?;

        // An extra output to someone else must not be hidden behind "amount + fee".
        let mut extra = psbt.clone();
        extra.unsigned_tx.output.push(TxOut {
            value: Amount::from_sat(5_000),
            script_pubkey: to.script_pubkey(),
        });
        extra.outputs.push(Default::default());
        let msg = tx_build(preview(&wallet, &extra, &to, amount))?;
        assert!(msg.contains("output 2 pays neither"), "{msg}");

        // A PSBT whose maps don't line up is an error, not a panic inside rust-bitcoin.
        let mut malformed = psbt.clone();
        malformed.inputs.clear();
        let msg = tx_build(preview(&wallet, &malformed, &to, amount))?;
        assert!(msg.contains("malformed PSBT"), "{msg}");
        assert!(matches!(
            expect_err(extract_tx(malformed))?,
            WalletError::TxBuild(_)
        ));
        Ok(())
    }

    /// What another process's payment looks like to a reopened wallet: a recorded transaction
    /// spending `input` and paying `to` (signatures don't matter to the wallet's bookkeeping).
    fn record_other_payment(wallet: &mut WalletService, input: OutPoint, to: &Address) {
        let other = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: input,
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(20_000),
                script_pubkey: to.script_pubkey(),
            }],
        };
        wallet
            .bdk_mut()
            .apply_unconfirmed_txs([(other, 1_700_000_100)]);
    }

    #[test]
    fn a_prepared_payment_is_refused_once_its_coin_is_spent() -> TestResult {
        let Funded {
            mut wallet, _dir, ..
        } = funded(&[100_000])?;
        let to = stranger_p2wpkh()?;
        let psbt = build_psbt(&mut wallet, &to, Amount::from_sat(10_000), rate(2)?)?;
        check_prepared(&wallet, &psbt)?;

        // Another payment spends the same coin before this one is signed.
        let coin = psbt.unsigned_tx.input[0].previous_output;
        record_other_payment(&mut wallet, coin, &to);
        let e = expect_err(check_prepared(&wallet, &psbt))?;
        assert_eq!(e.code(), "tx_build");
        assert!(
            e.to_string().contains(&format!("coin {coin} was spent")),
            "{e}"
        );
        Ok(())
    }

    #[test]
    fn a_prepared_payment_is_refused_once_its_change_address_is_used() -> TestResult {
        let Funded {
            mut wallet, _dir, ..
        } = funded(&[100_000, 100_000])?;
        let to = stranger_p2wpkh()?;
        let psbt = build_psbt(&mut wallet, &to, Amount::from_sat(10_000), rate(2)?)?;
        let change_out = psbt
            .unsigned_tx
            .output
            .iter()
            .find(|out| out.script_pubkey != to.script_pubkey())
            .ok_or("no change output")?;
        let change = Address::from_script(&change_out.script_pubkey, Network::Regtest)?;

        // Another payment, from the other coin, sends its change to the same address.
        let used: Vec<OutPoint> = psbt
            .unsigned_tx
            .input
            .iter()
            .map(|i| i.previous_output)
            .collect();
        let other_coin = wallet
            .bdk()
            .list_unspent()
            .map(|utxo| utxo.outpoint)
            .find(|op| !used.contains(op))
            .ok_or("no second coin")?;
        record_other_payment(&mut wallet, other_coin, &change);
        let e = expect_err(check_prepared(&wallet, &psbt))?;
        assert_eq!(e.code(), "tx_build");
        assert!(e.to_string().contains("change address"), "{e}");
        Ok(())
    }

    // ── Fee bumps ───────────────────────────────────────────────────────────────────────────

    /// Build, sign and record a payment as if it had been broadcast (no node needed).
    fn send_offline(
        wallet: &mut WalletService,
        signer: &Signer,
        to: &Address,
        sat: u64,
        fee_rate: FeeRate,
    ) -> TestResult<Transaction> {
        let mut psbt = build_psbt(wallet, to, Amount::from_sat(sat), fee_rate)?;
        sign_psbt(wallet, signer, &mut psbt)?;
        let tx = extract_tx(psbt)?;
        record_broadcast(wallet, &tx)?;
        Ok(tx)
    }

    fn tx_build_msg<T: std::fmt::Debug>(result: Result<T>) -> TestResult<String> {
        match result {
            Err(WalletError::TxBuild(msg)) => Ok(msg),
            other => Err(format!("expected TxBuild, got {other:?}").into()),
        }
    }

    #[test]
    fn a_fee_bump_replaces_the_payment_and_only_the_replacement_remains() -> TestResult {
        let Funded {
            mut wallet,
            signer,
            _dir,
        } = funded(&[100_000])?;
        let to = stranger_p2wpkh()?;
        let original = send_offline(&mut wallet, &signer, &to, 30_000, rate(2)?)?;
        let old_txid = original.compute_txid();
        let old_fee = wallet.bdk().calculate_fee(&original)?.to_sat();
        let old_change: Vec<&TxOut> = original
            .output
            .iter()
            .filter(|out| out.script_pubkey != to.script_pubkey())
            .collect();
        assert_eq!(old_change.len(), 1);
        wallet.set_label(old_txid, "rent")?;
        let balance_before = wallet.balance().total_sat;

        // The original pays 2 sat/vB, so BDK wants 3 sat/vB (750 sat/kwu) and BIP125 rule 4 a
        // hair more: (281 + 141) sat for 562 WU.
        assert_eq!(old_fee, 281);
        let min = min_fee_bump_rate(&wallet, old_txid)?;
        assert_eq!(min.to_sat_per_kwu(), 751, "{min:?}");
        let msg = tx_build_msg(build_fee_bump(&mut wallet, old_txid, rate(2)?))?;
        assert_eq!(
            msg,
            format!(
                "cannot speed up {old_txid}: a fee rate of 2 sat/vB is too low; it pays 2.00 \
                 sat/vB now, and a replacement must pay at least 1 sat/vB more (BIP125), so at \
                 least 3.01 sat/vB"
            )
        );
        // The exact minimum is enough (BIP125 rule 4 holds with the rounding).
        let at_min = build_fee_bump(&mut wallet, old_txid, min)?;
        preview_fee_bump(&wallet, &at_min, old_txid)?;
        cancel(&mut wallet, &at_min);

        let psbt = build_fee_bump(&mut wallet, old_txid, rate(5)?)?;
        // Same coin, same recipient and amount, same change address, more fee.
        let tx = &psbt.unsigned_tx;
        assert_eq!(tx.input.len(), 1);
        assert_eq!(
            tx.input[0].previous_output,
            original.input[0].previous_output
        );
        assert!(tx.input.iter().all(|input| input.sequence.is_rbf()));
        assert_eq!(tx.output.len(), 2);
        assert!(
            tx.output
                .iter()
                .any(|out| out.script_pubkey == to.script_pubkey()
                    && out.value == Amount::from_sat(30_000))
        );
        assert!(
            tx.output
                .iter()
                .any(|out| out.script_pubkey == old_change[0].script_pubkey)
        );

        let summary = preview_fee_bump(&wallet, &psbt, old_txid)?;
        assert_eq!(summary.replaces, Some(old_txid.to_string()));
        assert_eq!(summary.contact, None);
        assert_eq!(summary.to, to.to_string());
        assert_eq!(summary.amount_sat, 30_000);
        assert_eq!(summary.total_sat, 30_000 + summary.fee_sat);
        let extra = summary.fee_sat - old_fee;
        assert_eq!(
            summary.change_sat,
            Some(old_change[0].value.to_sat() - extra),
            "the extra fee comes out of the change"
        );
        assert!(extra >= summary.vsize, "rule 4: +1 sat/vB at least");
        assert!((summary.fee_rate_sat_vb - 5.0).abs() < 0.05, "{summary:?}");

        // Its coin is spent by the original, which the plain check refuses; the bump check knows.
        assert!(check_prepared(&wallet, &psbt).is_err());
        check_prepared_bump(&wallet, &psbt, old_txid)?;

        let mut signed = psbt;
        sign_psbt(&wallet, &signer, &mut signed)?;
        let replacement = extract_tx(signed)?;
        let new_txid = replacement.compute_txid();
        record_broadcast(&mut wallet, &replacement)?;

        let history = wallet.history();
        let txids: Vec<&str> = history.iter().map(|row| row.txid.as_str()).collect();
        assert!(txids.contains(&new_txid.to_string().as_str()), "{txids:?}");
        assert!(!txids.contains(&old_txid.to_string().as_str()), "{txids:?}");
        let row = history
            .iter()
            .find(|row| row.txid == new_txid.to_string())
            .ok_or("no replacement row")?;
        assert_eq!(row.fee_sat, Some(summary.fee_sat));
        assert_eq!(row.label.as_deref(), Some("rent"), "the label carries over");
        assert_eq!(wallet.balance().total_sat, balance_before - extra);
        assert_eq!(tx_status(&wallet, old_txid), None);
        assert!(tx_status(&wallet, new_txid).is_some());

        // The original can't be bumped again, and the replacement can (it is unconfirmed too).
        let msg = tx_build_msg(build_fee_bump(&mut wallet, old_txid, rate(10)?))?;
        assert!(msg.contains("was replaced by another transaction"), "{msg}");
        assert!(min_fee_bump_rate(&wallet, new_txid)?.to_sat_per_kwu() > 1_250);
        Ok(())
    }

    #[test]
    fn the_replacement_wins_even_if_the_original_was_seen_later() -> TestResult {
        let Funded {
            mut wallet,
            signer,
            _dir,
        } = funded(&[100_000])?;
        let to = stranger_p2wpkh()?;
        let original = send_offline(&mut wallet, &signer, &to, 30_000, rate(2)?)?;
        // A sync that saw the original "after" now (a clock step, or the same second).
        wallet
            .bdk_mut()
            .apply_unconfirmed_txs([(original.clone(), unix_now() + 1_000)]);
        let mut psbt = build_fee_bump(&mut wallet, original.compute_txid(), rate(4)?)?;
        sign_psbt(&wallet, &signer, &mut psbt)?;
        let replacement = extract_tx(psbt)?;
        record_broadcast(&mut wallet, &replacement)?;
        assert!(tx_status(&wallet, replacement.compute_txid()).is_some());
        assert_eq!(tx_status(&wallet, original.compute_txid()), None);
        Ok(())
    }

    #[test]
    fn what_cannot_be_bumped_is_refused_with_a_reason() -> TestResult {
        let Funded {
            mut wallet,
            signer,
            _dir,
        } = funded(&[100_000])?;
        let to = stranger_p2wpkh()?;

        // Unknown to the wallet.
        let unknown = Txid::from_byte_array([42; 32]);
        assert!(matches!(
            expect_err(build_fee_bump(&mut wallet, unknown, rate(5)?))?,
            WalletError::TxNotFound(t) if t == unknown.to_string()
        ));
        // The payment that funded us: someone else's coins.
        let incoming: Txid = wallet
            .history()
            .first()
            .ok_or("no funding tx")?
            .txid
            .parse()?;
        let msg = tx_build_msg(build_fee_bump(&mut wallet, incoming, rate(5)?))?;
        assert!(msg.contains("was not sent by this wallet"), "{msg}");
        // A bad rate is refused first, like for a payment.
        let msg = tx_build_msg(build_fee_bump(
            &mut wallet,
            incoming,
            FeeRate::from_sat_per_kwu(249),
        ))?;
        assert!(msg.contains("below the 1 sat/vB minimum"), "{msg}");

        // A payment that does not signal RBF (nSequence 0xFFFFFFFF).
        let mut psbt = build_psbt(&mut wallet, &to, Amount::from_sat(20_000), rate(2)?)?;
        for input in &mut psbt.unsigned_tx.input {
            input.sequence = Sequence::MAX;
        }
        sign_psbt(&wallet, &signer, &mut psbt)?;
        let final_tx = extract_tx(psbt)?;
        record_broadcast(&mut wallet, &final_tx)?;
        let msg = tx_build_msg(build_fee_bump(
            &mut wallet,
            final_tx.compute_txid(),
            rate(5)?,
        ))?;
        assert!(msg.contains("does not signal replace-by-fee"), "{msg}");

        // A payment whose change another unconfirmed payment already spends.
        let Funded {
            mut wallet,
            signer,
            _dir,
        } = funded(&[100_000])?;
        let parent = send_offline(&mut wallet, &signer, &to, 20_000, rate(2)?)?;
        let child = send_offline(&mut wallet, &signer, &to, 20_000, rate(2)?)?;
        assert_eq!(
            child.input[0].previous_output.txid,
            parent.compute_txid(),
            "the only coin left is the parent's change"
        );
        let msg = tx_build_msg(build_fee_bump(&mut wallet, parent.compute_txid(), rate(5)?))?;
        assert!(
            msg.contains(&format!(
                "payment {} spends its output",
                child.compute_txid()
            )),
            "{msg}"
        );

        // A transaction spending one of our coins and one we don't control.
        let Funded {
            mut wallet, _dir, ..
        } = funded(&[100_000])?;
        let ours = wallet
            .bdk()
            .list_unspent()
            .next()
            .ok_or("no coin")?
            .outpoint;
        let mixed = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![
                TxIn {
                    previous_output: ours,
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    ..Default::default()
                },
                TxIn {
                    previous_output: OutPoint::new(Txid::from_byte_array([5; 32]), 0),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    ..Default::default()
                },
            ],
            output: vec![TxOut {
                value: Amount::from_sat(150_000),
                script_pubkey: to.script_pubkey(),
            }],
        };
        record_broadcast(&mut wallet, &mixed)?;
        let msg = tx_build_msg(build_fee_bump(&mut wallet, mixed.compute_txid(), rate(5)?))?;
        assert!(
            msg.contains("1 of its 2 inputs are not this wallet's coins"),
            "{msg}"
        );
        Ok(())
    }

    #[test]
    fn a_prepared_bump_is_checked_again_before_signing() -> TestResult {
        let Funded {
            mut wallet,
            signer,
            _dir,
        } = funded(&[100_000])?;
        let to = stranger_p2wpkh()?;
        let original = send_offline(&mut wallet, &signer, &to, 30_000, rate(2)?)?;
        let txid = original.compute_txid();
        let psbt = build_fee_bump(&mut wallet, txid, rate(5)?)?;
        check_prepared_bump(&wallet, &psbt, txid)?;

        // Paying less than BIP125 rule 4 needs: give the extra fee back to the change.
        let mut cheap = psbt.clone();
        let change = cheap
            .unsigned_tx
            .output
            .iter_mut()
            .find(|out| out.script_pubkey != to.script_pubkey())
            .ok_or("no change output")?;
        let old_fee = wallet.bdk().calculate_fee(&original)?;
        let new_fee = psbt.fee()?;
        change.value += new_fee - old_fee - Amount::from_sat(1);
        let msg = tx_build_msg(check_prepared_bump(&wallet, &cheap, txid))?;
        assert!(msg.contains("must pay at least"), "{msg}");
        assert!(msg.contains("(BIP125)"), "{msg}");

        // A transaction that doesn't spend the original's coin doesn't replace it.
        let mut elsewhere = psbt.clone();
        elsewhere.unsigned_tx.input[0].previous_output =
            OutPoint::new(Txid::from_byte_array([3; 32]), 0);
        let msg = tx_build_msg(check_prepared_bump(&wallet, &elsewhere, txid))?;
        assert!(msg.contains("does not replace"), "{msg}");

        // Once the original confirms or is replaced, a prepared bump is stale.
        let mut signed = psbt.clone();
        sign_psbt(&wallet, &signer, &mut signed)?;
        let replacement = extract_tx(signed)?;
        record_broadcast(&mut wallet, &replacement)?;
        let msg = tx_build_msg(check_prepared_bump(&wallet, &psbt, txid))?;
        assert!(msg.contains("was replaced"), "{msg}");
        Ok(())
    }

    #[test]
    fn unknown_transactions_have_no_status() -> TestResult {
        let Funded { wallet, _dir, .. } = funded(&[100_000])?;
        assert_eq!(tx_status(&wallet, Txid::from_byte_array([42; 32])), None);
        Ok(())
    }
}
