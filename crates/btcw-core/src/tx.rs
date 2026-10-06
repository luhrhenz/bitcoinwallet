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
//! [`prepare_send`] (parse → fee rate → build → preview) and [`complete_send`]
//! (sign → [`broadcast_signed`]) bundle those steps, so the CLI and the desktop bridge run the
//! same sequence. The bridge keeps the PSBT in Rust between the two calls; the UI only ever sees
//! the [`SendPreview`].
//!
//! Guarantees the frontends rely on:
//! - **The preview explains every output.** It finds the recipient output (exact script and
//!   amount) and requires every other output to pay this wallet's change keychain; anything else
//!   is an error. So what the user confirms is the whole transaction, not a summary of part of it.
//! - **All or nothing signing.** [`sign_psbt`] fails unless the wallet's key signed *every* input
//!   and BDK finalized every input.
//! - **Nothing stays reserved after a failure.** The helpers call [`cancel`] on every error before
//!   the broadcast succeeds, so a declined or failed send never burns a change address.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bdk_wallet::coin_selection::InsufficientFunds;
use bdk_wallet::error::CreateTxError;
use bdk_wallet::{KeychainKind, SignOptions};

use crate::bitcoin::address::NetworkUnchecked;
use crate::bitcoin::psbt::ExtractTxError;
use crate::bitcoin::{Address, Amount, FeeRate, Network, Psbt, Transaction, Txid, Weight};
use crate::chain::Node;
use crate::error::{Result, WalletError};
use crate::keys::Signer;
use crate::types::{SendPreview, TxStatus};
use crate::wallet::WalletService;

/// Dust threshold for a P2WPKH output at the default relay fee.
pub const DUST_LIMIT_P2WPKH: Amount = Amount::from_sat(294);

/// Confirmation target (in blocks) for the node's fee estimate when the caller doesn't choose a
/// fee rate: about an hour.
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

// ── Build and preview ───────────────────────────────────────────────────────────────────────

/// Unsigned PSBT paying `amount` to `to` at `fee_rate`; BDK picks coins and adds change.
/// `DustAmount` / `InsufficientFunds` on bad input.
///
/// BDK's defaults are kept on purpose: change goes to the next unused address of the
/// **internal** keychain, every input signals RBF (`nSequence = 0xFFFFFFFD`), and `nLockTime` is
/// the wallet's synced tip height (anti fee-sniping). Building reveals that change address and
/// marks it used in memory; [`cancel`] undoes that if the payment is not sent.
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

/// The dust rule [`build_psbt`] applies (`DustAmount`), exposed so a frontend can reject the
/// amount before unlocking the wallet or syncing.
///
/// Nodes don't relay an output worth less than it would cost to spend at 3 sat/vB ("dust"):
/// 294 sat for P2WPKH, more for bigger scripts (546 for legacy P2PKH, 330 for taproot).
pub fn check_amount(to: &Address, amount: Amount) -> Result<()> {
    if amount < DUST_LIMIT_P2WPKH.max(to.script_pubkey().minimal_non_dust()) {
        return Err(WalletError::DustAmount(amount));
    }
    Ok(())
}

/// The fee-rate rule [`build_psbt`] applies (`TxBuild`), exposed so a frontend can reject the
/// rate before unlocking the wallet or syncing.
///
/// Nodes drop transactions below 1 sat/vB (the default `minrelaytxfee`), and rust-bitcoin's
/// `extract_tx` refuses anything above 25 000 sat/vB, so a rate outside that range is refused
/// before the user is asked to confirm anything.
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
/// Fails (`TxBuild`) unless the PSBT pays exactly `amount` to `to` and every other output goes
/// back to this wallet's change keychain, so the preview can never hide an output.
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

    // Fee = inputs − outputs. The PSBT carries each input's previous output (`witness_utxo`),
    // which is how the fee is known before the transaction is signed.
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
        // For display only; every amount stays an integer number of satoshis. `vsize` is at
        // least the 10-byte transaction header, so this never divides by zero.
        fee_rate_sat_vb: fee.to_sat() as f64 / vsize.max(1) as f64,
        vsize,
        change_sat: change.map(Amount::to_sat),
        total_sat: total.to_sat(),
    })
}

/// User declined: un-reserve the change address BDK revealed while building.
///
/// Only touches staged, in-memory state (BDK's "unused" marks are not persisted); the caller
/// persists if it wants the reveal itself saved. Harmless for an address that was actually paid:
/// BDK ignores `unmark_used` for scripts that have received outputs.
pub fn cancel(wallet: &mut WalletService, psbt: &Psbt) {
    let bdk = wallet.bdk_mut();
    for out in &psbt.unsigned_tx.output {
        if let Some((KeychainKind::Internal, index)) =
            bdk.derivation_of_spk(out.script_pubkey.clone())
        {
            // `build_tx` picks the lowest revealed-but-unused change index and marks it used.
            // Unmarking it makes the next build pick the same index again instead of a new one,
            // so declined payments don't leave a trail of never-used change addresses.
            bdk.unmark_used(KeychainKind::Internal, index);
        }
    }
}

/// For a PSBT built earlier and signed later, after the wallet was closed in between (the
/// desktop app's prepared payment): check the wallet hasn't moved on. Every input must still be
/// an unspent coin of this wallet, and no change output may pay an address another transaction
/// has paid since. `TxBuild` otherwise.
///
/// Another process (`btcw send` in a terminal) can spend those coins, or take the same
/// revealed-but-unused change address, while the preview is on screen. Signing anyway would
/// replace that payment by RBF (if this one pays more) or reuse an address.
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
    for out in &psbt.unsigned_tx.output {
        let is_change = matches!(
            bdk.derivation_of_spk(out.script_pubkey.clone()),
            Some((KeychainKind::Internal, _))
        );
        // Outputs the wallet already holds, spent or not: was this address paid meanwhile?
        if is_change
            && bdk
                .list_output()
                .any(|output| output.txout.script_pubkey == out.script_pubkey)
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

/// `signer.sign_psbt` then BDK's `finalize_psbt` (builds the witnesses).
/// `Sign` error unless every input ends up finalized.
///
/// The two halves are split on purpose (PLAN §4, Phase 0 finding 1): rust-bitcoin's `Psbt::sign`
/// produces the signatures from the master key (BDK 3.2 deprecated its own key store), and BDK,
/// which knows the descriptors, turns each signature + public key into the final witness.
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
    // Extracting an unfinalized input yields an empty witness: a transaction every node rejects.
    if let Some(i) = psbt
        .inputs
        .iter()
        .position(|input| input.final_script_witness.is_none() && input.final_script_sig.is_none())
    {
        return Err(WalletError::Sign(format!(
            "input {i} is not signed and finalized; call sign_psbt first"
        )));
    }
    // `Psbt::extract_tx` computes the fee itself but treats one of the fee errors (a
    // `non_witness_utxo` without the spent output) as unreachable and panics. Computing it here
    // first turns that case into an error.
    psbt.fee()
        .map_err(|e| WalletError::TxBuild(format!("cannot work out the fee: {e}")))?;
    // rust-bitcoin's sanity check stays on: it refuses fee rates above 25 000 sat/vB, the
    // classic "fee in BTC typed into the sat field" mistake.
    psbt.extract_tx().map_err(|e| match e {
        ExtractTxError::AbsurdFeeRate { fee_rate, .. } => WalletError::TxBuild(format!(
            "refusing to extract a transaction paying {} sat/vB (the limit is {} sat/vB)",
            sat_per_vb(fee_rate),
            sat_per_vb(Psbt::DEFAULT_MAX_FEE_RATE)
        )),
        other => WalletError::TxBuild(format!("cannot extract the transaction: {other}")),
    })
}

/// After a successful broadcast: add the tx to the wallet as unconfirmed and persist,
/// so the balance reflects the spend before the next sync.
pub fn record_broadcast(wallet: &mut WalletService, tx: &Transaction) -> Result<()> {
    // `now` is the "last seen in the mempool" time BDK uses to pick between conflicting
    // unconfirmed transactions, and it becomes the transaction's `first_seen`.
    wallet
        .bdk_mut()
        .apply_unconfirmed_txs([(Arc::new(tx.clone()), unix_now())]);
    wallet.persist()
}

/// Status from the wallet's view (call `Node::sync` first for fresh data).
///
/// `None` if the wallet doesn't know the transaction, or no longer considers it part of the
/// chain or mempool (e.g. it was replaced). Confirmations count from the wallet's synced tip.
pub fn tx_status(wallet: &WalletService, txid: Txid) -> Option<TxStatus> {
    let wtx = wallet.bdk().get_tx(txid)?;
    Some(crate::wallet::tx_status(
        &wtx.chain_position,
        wallet.synced_height(),
    ))
}

// ── One code path for both frontends ────────────────────────────────────────────────────────

/// Everything from "the user typed a payment" to "show the preview": parse `to` for the wallet's
/// network, pick the fee rate (`fee_rate`, else the node's estimate for [`FEE_TARGET_BLOCKS`]),
/// build the PSBT and summarise it.
///
/// Call [`Node::sync`] first so coin selection sees the current UTXOs. The caller then holds the
/// PSBT until the user decides: [`complete_send`] to send it, [`cancel`] to drop it.
pub fn prepare_send(
    wallet: &mut WalletService,
    node: &Node,
    to: &str,
    amount_sat: u64,
    fee_rate: Option<FeeRate>,
) -> Result<(Psbt, SendPreview)> {
    let to = parse_address(to, wallet.network())?;
    let amount = Amount::from_sat(amount_sat);
    let fee_rate = match fee_rate {
        Some(rate) => rate,
        None => node.estimate_fee_rate(FEE_TARGET_BLOCKS)?,
    };
    let psbt = build_psbt(wallet, &to, amount, fee_rate)?;
    match preview(wallet, &psbt, &to, amount) {
        Ok(preview) => Ok((psbt, preview)),
        Err(e) => {
            cancel(wallet, &psbt);
            Err(e)
        }
    }
}

/// The user confirmed: [`sign_psbt`], then [`broadcast_signed`]. Returns the txid.
///
/// On any error before the broadcast succeeds, the change address is released ([`cancel`]) and
/// nothing was sent.
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

/// Second half of [`complete_send`], for callers that drop the [`Signer`] as soon as signing is
/// done (the CLI): [`extract_tx`] → [`Node::broadcast`] → [`record_broadcast`].
///
/// Errors before the node accepts the transaction release the change address ([`cancel`]).
/// Once it has accepted it the coins are on their way, so a failure to *record* it is only
/// logged: the spend stays staged in memory, and the next sync finds it in the mempool anyway.
pub fn broadcast_signed(wallet: &mut WalletService, node: &Node, psbt: Psbt) -> Result<Txid> {
    // Extract from a copy: on failure the outputs are still needed to release the change address.
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
            // If the node did accept it after all (say, the reply timed out), the next sync sees
            // the transaction in the mempool and marks the change address used again.
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

// ── Helpers ─────────────────────────────────────────────────────────────────────────────────

/// Weight of the transaction once every input is signed, with the largest possible signatures.
///
/// The unsigned transaction can't simply be measured: its inputs have empty witnesses, and the
/// witness (signature + public key) is most of a P2WPKH input's size. So we start from the
/// unsigned weight and add what signing adds:
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
/// About half of all signatures are a byte shorter (when `r` fits in 32 bytes), and very rarely
/// two, so the estimate is an upper bound, usually 0 or 1 WU per input above the real weight.
/// That's the safe side: the fee rate actually paid is never below the one shown. BDK's coin
/// selection sizes inputs the same way, which is why `fee ≈ fee rate × this vsize`.
fn estimated_signed_weight(wallet: &WalletService, psbt: &Psbt) -> Result<Weight> {
    let tx = &psbt.unsigned_tx;
    if tx.input.is_empty() {
        return Err(WalletError::TxBuild("the transaction has no inputs".into()));
    }
    let bdk = wallet.bdk();
    let mut weight = tx.weight() + SEGWIT_MARKER_AND_FLAG;
    for (i, utxo) in psbt.iter_funding_utxos().enumerate() {
        let utxo = utxo.map_err(|e| WalletError::TxBuild(format!("input {i}: {e}")))?;
        let Some((keychain, _)) = bdk.derivation_of_spk(utxo.script_pubkey.clone()) else {
            return Err(WalletError::TxBuild(format!(
                "input {i} does not spend one of this wallet's coins"
            )));
        };
        let satisfaction = bdk
            .public_descriptor(keychain)
            .max_weight_to_satisfy()
            .map_err(|e| {
                WalletError::TxBuild(format!("input {i}: cannot size its witness: {e}"))
            })?;
        weight += WITNESS_ITEM_COUNT + satisfaction;
    }
    Ok(weight)
}

/// rust-bitcoin's PSBT helpers (`fee`, `iter_funding_utxos`) *assert* one input map per
/// transaction input. PSBTs we build always match, but a hand-made one could panic there.
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

/// `FeeRate` counts sat per 1000 weight units; 1 vB = 4 WU, so 1 sat/vB = 250 sat/kwu.
/// For messages only (`2`, `0.996`, `25000`).
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

/// Offline tests: the wallet is funded with made-up unconfirmed transactions (BDK doesn't need to
/// see the parents of their dummy inputs), which needs the crate-private `bdk_mut()`. The same
/// flow against a real node, including broadcast and confirmation, is in `tests/tx.rs`.
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
        // The estimate is an upper bound, and at most a byte per input above the real size.
        let real = u64::try_from(signed.vsize())?;
        assert!(
            real <= preview.vsize && preview.vsize <= real + 2,
            "estimated {} vB, signed {real} vB",
            preview.vsize
        );
        // So the rate really paid is at least the one shown...
        assert!(preview.fee_sat as f64 / real as f64 >= preview.fee_rate_sat_vb);
        // ...and, per weight unit (how BDK and Core's fee logic measure it), at least the one
        // asked for. (Per *rounded-up* vbyte it can be a hair below: 626 sat / 834 WU is
        // 3.002 sat/vB, but 834 WU rounds up to 209 vB and 626 / 209 = 2.995.)
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

        // Another payment, from the *other* coin, sends its change to the same address (as a
        // second process does: the address was revealed, saved and never paid).
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

    #[test]
    fn unknown_transactions_have_no_status() -> TestResult {
        let Funded { wallet, _dir, .. } = funded(&[100_000])?;
        assert_eq!(tx_status(&wallet, Txid::from_byte_array([42; 32])), None);
        Ok(())
    }
}
