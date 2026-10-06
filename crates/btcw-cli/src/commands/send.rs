//! `btcw send --to ADDR|NAME --amount SAT`: build → preview → confirm → sign → broadcast
//! (PLAN §5.8; contact names: PLAN-v2 §1).
//!
//! 1. Checks that need neither the password nor the node: `--json` needs `--yes`, the address
//!    (format and network), the amount (dust), `--fee-rate`, `--psbt-out`, the wallet exists,
//!    and a terminal to confirm on (unless `--yes`).
//! 2. Open the wallet **watch-only**: building and previewing a payment needs no secrets. A
//!    `--to` that isn't an address is looked up in the address book here (and the amount checked
//!    against the contact's address), still before any sync.
//! 3. `Node::connect` + sync, so coin selection sees the current coins.
//! 4. `tx::prepare_send`: fee rate (`--fee-rate`, else the node's 6-block estimate), unsigned
//!    PSBT, preview. The desktop bridge calls the same function.
//! 5. The preview on stdout, warnings for unusual fees, the unsigned PSBT if `--psbt-out`.
//! 6. `Send? [y/N]` on the terminal unless `--yes`. Anything but yes → `tx::cancel`, exit 0.
//! 7. Only now the password → `api::load_signer` (checks the seed belongs to this wallet), so the
//!    master key is never in memory while the user reads the preview, and a declined payment
//!    never touches it at all.
//! 8. `tx::sign_psbt`, drop the signer (wiping the master key), `tx::broadcast_signed`
//!    (extract → broadcast → record in the wallet).
//!
//! From step 4 on, every way out except a successful broadcast releases the change address.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use btcw_core::WalletError;
use btcw_core::api;
use btcw_core::bitcoin::{Amount, FeeRate, Network, Psbt};
use btcw_core::book;
use btcw_core::chain::Node;
use btcw_core::config::Config;
use btcw_core::tx;
use btcw_core::types::SendPreview;
use btcw_core::wallet::WalletService;
use serde::Serialize;

use crate::commands::sync::sync_with_progress;
use crate::output::{Painter, Ui, amount, btc, group_thousands, grouped};
use crate::prompt::{self, Terminal};

/// Warn when the fee is at least this share of the amount (same threshold as the desktop app).
const HIGH_FEE_PERCENT: u64 = 10;
/// Warn above this rate: even busy mempools rarely need more, so it's more likely a typo.
const HIGH_FEE_RATE_SAT_VB: f64 = 100.0;

#[derive(Debug, Serialize)]
struct SendJson {
    network: String,
    txid: String,
    preview: SendPreview,
}

/// The `btcw send` command line.
#[derive(Debug)]
pub struct Request<'a> {
    pub to: &'a str,
    pub amount_sat: u64,
    pub fee_rate_sat_vb: Option<u64>,
    pub yes: bool,
    pub psbt_out: Option<&'a Path>,
}

pub fn run(cfg: &Config, ui: &Ui, req: &Request<'_>) -> Result<()> {
    // 1. Fail fast: nothing below needs the password, the node or the wallet lock.
    if ui.json() && !req.yes {
        bail!(
            "--json needs --yes: a JSON run can't stop to ask for confirmation; review the \
             payment without --json first"
        );
    }
    let value = Amount::from_sat(req.amount_sat);
    let address = match tx::parse_address(req.to, cfg.network) {
        Ok(address) => Some(address),
        // Not an address and doesn't look like one: maybe a contact's name, which needs the
        // wallet file (step 2). Anything address-like keeps its address error.
        Err(WalletError::InvalidAddress(_))
            if !req.to.trim().is_empty() && !book::looks_like_address(req.to) =>
        {
            None
        }
        Err(e) => return Err(e.into()),
    };
    if let Some(address) = &address {
        tx::check_amount(address, value)?;
    }
    let fee_rate = req.fee_rate_sat_vb.map(fee_rate_from_flag).transpose()?;
    if let Some(rate) = fee_rate {
        tx::check_fee_rate(rate)?;
    }
    if let Some(path) = req.psbt_out {
        refuse_existing(path)?;
    }
    let mut terminal = if req.yes {
        None
    } else {
        Some(Terminal::open()?)
    };
    if !api::wallet_exists(cfg) {
        return Err(WalletError::WalletNotFound(cfg.network_dir()).into());
    }

    // 2. Watch-only: no password until the user has approved the payment.
    let mut wallet = api::open_watch_only(cfg)?;
    if address.is_none() {
        let recipient = wallet.resolve_recipient(req.to)?;
        tx::check_amount(&recipient.address, value)?;
    }

    // 3. Spend from fresh state: coins that arrived or were spent since the last sync.
    let node = Node::connect(&cfg.rpc, cfg.network)?;
    sync_with_progress(ui, &node, &mut wallet)?;

    // 4. Same function as the desktop app's "Review payment".
    let (mut psbt, preview) =
        tx::prepare_send(&mut wallet, &node, req.to, req.amount_sat, fee_rate)?;

    // 5–6. Declined or failed: give the change address back.
    match review(cfg, ui, req, &psbt, &preview, terminal.as_mut()) {
        Ok(true) => {}
        Ok(false) => {
            release(ui, &mut wallet, &psbt);
            return ui.println(&format!(
                "{} Cancelled; nothing was sent.",
                ui.out.badge(cfg.network)
            ));
        }
        Err(e) => {
            release(ui, &mut wallet, &psbt);
            return Err(e);
        }
    }

    // 7. The password authorises this one payment.
    let signer = match prompt::password(ui)
        .and_then(|password| Ok(api::load_signer(cfg, &wallet, &password)?))
    {
        Ok(signer) => signer,
        Err(e) => {
            release(ui, &mut wallet, &psbt);
            return Err(e);
        }
    };

    // 8. Sign, then wipe the master key before anything touches the network.
    let signed = tx::sign_psbt(&wallet, &signer, &mut psbt);
    drop(signer);
    if let Err(e) = signed {
        release(ui, &mut wallet, &psbt);
        return Err(e.into());
    }
    // Releases the change address itself if the extract or the broadcast fails.
    let txid = tx::broadcast_signed(&mut wallet, &node, psbt)?;
    drop(wallet);

    if ui.json() {
        return ui.print_json(&SendJson {
            network: cfg.network.to_string(),
            txid: txid.to_string(),
            preview,
        });
    }
    let to = match &preview.contact {
        Some(name) => format!("{name} ({})", preview.to),
        None => preview.to.clone(),
    };
    ui.println(&format!(
        "{} Sent {} to {to}\nTxid: {}\nIt is waiting in the mempool now; follow it with `btcw status {txid} --watch`.",
        ui.out.badge(cfg.network),
        amount(preview.amount_sat),
        ui.out.bold(txid),
    ))
}

/// Show the preview, write the PSBT file if asked, and ask. `Ok(true)` means "send it".
fn review(
    cfg: &Config,
    ui: &Ui,
    req: &Request<'_>,
    psbt: &Psbt,
    preview: &SendPreview,
    terminal: Option<&mut Terminal>,
) -> Result<bool> {
    if !ui.json() {
        ui.println(&describe_preview(cfg.network, preview, ui.out))?;
    }
    for warning in fee_warnings(preview) {
        ui.warn(warning);
    }
    if cfg.network == Network::Bitcoin {
        ui.warn("this is mainnet: the payment sends real bitcoin");
    }
    if let Some(path) = req.psbt_out {
        write_psbt(path, psbt)?;
        ui.note(format_args!(
            "wrote the unsigned PSBT to {} (inspect it with `bitcoin-cli decodepsbt \"$(cat {})\"`)",
            path.display(),
            path.display()
        ));
    }
    match terminal {
        // `--yes`: the user approved before seeing the preview.
        None => Ok(true),
        Some(terminal) => terminal.confirm("Send?"),
    }
}

/// `tx::cancel` + persist, for every way out before the broadcast. The persist only saves the
/// change address BDK revealed (the next payment reuses it), so a failure is just a warning.
pub(crate) fn release(ui: &Ui, wallet: &mut WalletService, psbt: &Psbt) {
    tx::cancel(wallet, psbt);
    if let Err(e) = wallet.persist() {
        ui.warn(format_args!(
            "could not save the wallet after cancelling: {e}"
        ));
    }
}

/// ```text
/// [regtest] Payment preview
///   To        bcrt 1q6r z28m cfax tmd6 v789 l9rr lrus dprr 9pz3 cppk
///   Amount    0.00100000 BTC (100,000 sat)
///   Fee       0.00000705 BTC (705 sat)
///   Fee rate  5.00 sat/vB for an estimated 141 vB
///   Change    0.00899295 BTC (899,295 sat), back to this wallet
///   Total     0.00100705 BTC (100,705 sat), amount + fee
/// ```
/// The recipient is shown in full, in groups of four, never shortened (see `output::grouped`).
/// A contact's name gets a line of its own *below* the address, never instead of it, and a fee
/// bump starts with the payment it replaces:
/// ```text
/// [regtest] Fee bump preview
///   Replaces  3485a59489e4cd7425887ced25fe63f90d33989339dee25ea0675dc1185312d6
///   To        bcrt 1q6r z28m cfax tmd6 v789 l9rr lrus dprr 9pz3 cppk
///   Contact   Alice
///   ...
/// ```
pub fn describe_preview(network: Network, p: &SendPreview, paint: Painter) -> String {
    let width = [
        p.amount_sat,
        p.fee_sat,
        p.change_sat.unwrap_or(0),
        p.total_sat,
    ]
    .iter()
    .map(|sat| btc(*sat).len())
    .max()
    .unwrap_or(0);
    let line = |label: &str, sat: u64| {
        format!(
            "  {label:<10}{:>width$} BTC ({} sat)",
            btc(sat),
            group_thousands(sat)
        )
    };
    let change = match p.change_sat {
        Some(sat) => format!("{}, back to this wallet", line("Change", sat)),
        None => format!(
            "  {:<10}none (anything left over was below the dust limit and goes to the fee)",
            "Change"
        ),
    };
    let title = if p.replaces.is_some() {
        "Fee bump preview"
    } else {
        "Payment preview"
    };
    let mut lines = vec![format!("{} {title}", paint.badge(network))];
    if let Some(old) = &p.replaces {
        lines.push(format!("  {:<10}{old}", "Replaces"));
    }
    lines.push(format!("  {:<10}{}", "To", paint.bold(grouped(&p.to))));
    if let Some(name) = &p.contact {
        lines.push(format!("  {:<10}{name}", "Contact"));
    }
    lines.extend([
        line("Amount", p.amount_sat),
        line("Fee", p.fee_sat),
        format!(
            "  {:<10}{:.2} sat/vB for an estimated {} vB",
            "Fee rate",
            p.fee_rate_sat_vb,
            group_thousands(p.vsize)
        ),
        change,
        paint
            .bold(format!("{}, amount + fee", line("Total", p.total_sat)))
            .to_string(),
    ]);
    lines.join("\n")
}

/// Fees that are probably a mistake: a large share of the amount, or a very high rate.
pub(crate) fn fee_warnings(p: &SendPreview) -> Vec<String> {
    let mut warnings = Vec::new();
    // `fee / amount ≥ 10%` in integers (u128 so nothing can overflow).
    let (fee, amount) = (u128::from(p.fee_sat), u128::from(p.amount_sat));
    if fee * 100 >= amount * u128::from(HIGH_FEE_PERCENT) {
        warnings.push(format!(
            "the fee ({} sat) is {}% of the amount being sent",
            group_thousands(p.fee_sat),
            fee * 100 / amount.max(1)
        ));
    }
    if p.fee_rate_sat_vb > HIGH_FEE_RATE_SAT_VB {
        warnings.push(format!(
            "the fee rate of {:.2} sat/vB is unusually high (above {HIGH_FEE_RATE_SAT_VB} sat/vB); \
             check that it's what you meant",
            p.fee_rate_sat_vb
        ));
    }
    warnings
}

pub(crate) fn fee_rate_from_flag(sat_per_vb: u64) -> Result<FeeRate> {
    FeeRate::from_sat_per_vb(sat_per_vb)
        .ok_or_else(|| anyhow!("--fee-rate {sat_per_vb} sat/vB is too large"))
}

/// Checked up front so an existing file is reported before the password prompt; `write_psbt`
/// refuses again (atomically, `create_new`) in case the file appears in between.
fn refuse_existing(path: &Path) -> Result<()> {
    // `symlink_metadata` so that a dangling symlink also counts as "something is there".
    if std::fs::symlink_metadata(path).is_ok() {
        bail!(
            "--psbt-out: {} already exists; refusing to overwrite it",
            path.display()
        );
    }
    Ok(())
}

/// The **unsigned** PSBT as base64 (BIP174's text form) plus a newline, readable only by the
/// owner: it holds no keys, but it does list the wallet's coins, change address and key paths.
fn write_psbt(path: &Path, psbt: &Psbt) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| {
        if e.kind() == ErrorKind::AlreadyExists {
            anyhow!(
                "--psbt-out: {} already exists; refusing to overwrite it",
                path.display()
            )
        } else {
            anyhow!(e).context(format!("creating {}", path.display()))
        }
    })?;
    let written = writeln!(file, "{psbt}").and_then(|()| file.sync_all());
    if let Err(e) = written {
        drop(file);
        // Best effort: a half-written PSBT is worse than none.
        let _ = std::fs::remove_file(path);
        return Err(e).context(format!("writing {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use btcw_core::bitcoin::absolute::LockTime;
    use btcw_core::bitcoin::transaction::Version;
    use btcw_core::bitcoin::{ScriptBuf, Transaction, TxIn, TxOut};

    use super::*;

    fn preview(amount_sat: u64, fee_sat: u64, vsize: u64, change_sat: Option<u64>) -> SendPreview {
        SendPreview {
            to: "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk".into(),
            amount_sat,
            fee_sat,
            fee_rate_sat_vb: fee_sat as f64 / vsize as f64,
            vsize,
            change_sat,
            total_sat: amount_sat + fee_sat,
            contact: None,
            replaces: None,
        }
    }

    #[test]
    fn preview_shows_everything_aligned_with_the_full_address() {
        let text = describe_preview(
            Network::Regtest,
            &preview(100_000, 705, 141, Some(899_295)),
            Painter::plain(),
        );
        assert_eq!(
            text,
            "[regtest] Payment preview\n\
             \x20 To        bcrt 1q6r z28m cfax tmd6 v789 l9rr lrus dprr 9pz3 cppk\n\
             \x20 Amount    0.00100000 BTC (100,000 sat)\n\
             \x20 Fee       0.00000705 BTC (705 sat)\n\
             \x20 Fee rate  5.00 sat/vB for an estimated 141 vB\n\
             \x20 Change    0.00899295 BTC (899,295 sat), back to this wallet\n\
             \x20 Total     0.00100705 BTC (100,705 sat), amount + fee"
        );

        let text = describe_preview(
            Network::Testnet4,
            &preview(5_000_000_000, 141, 141, None),
            Painter::plain(),
        );
        assert!(text.starts_with("[testnet4] Payment preview"), "{text}");
        assert!(
            text.contains("  Fee        0.00000141 BTC (141 sat)"),
            "amounts right-aligned to the widest: {text}"
        );
        assert!(
            text.contains("  Change    none (anything left over"),
            "{text}"
        );
    }

    #[test]
    fn a_contact_and_a_replaced_payment_are_named_but_the_address_stays() {
        let txid = "3485a59489e4cd7425887ced25fe63f90d33989339dee25ea0675dc1185312d6";
        let p = SendPreview {
            contact: Some("Alice".into()),
            replaces: Some(txid.into()),
            ..preview(100_000, 705, 141, Some(899_295))
        };
        let text = describe_preview(Network::Regtest, &p, Painter::plain());
        assert!(
            text.starts_with(&format!(
                "[regtest] Fee bump preview\n\
             \x20 Replaces  {txid}\n\
             \x20 To        bcrt 1q6r z28m cfax tmd6 v789 l9rr lrus dprr 9pz3 cppk\n\
             \x20 Contact   Alice\n\
             \x20 Amount    0.00100000 BTC (100,000 sat)\n"
            )),
            "{text}"
        );

        let only_contact = SendPreview {
            replaces: None,
            ..p
        };
        let text = describe_preview(Network::Regtest, &only_contact, Painter::plain());
        assert!(
            text.starts_with("[regtest] Payment preview\n  To        bcrt 1q6r"),
            "{text}"
        );
        assert!(text.contains("\n  Contact   Alice\n"), "{text}");
    }

    #[test]
    fn unusual_fees_are_flagged() {
        assert!(fee_warnings(&preview(100_000, 705, 141, None)).is_empty());
        // Exactly 10% counts.
        let warnings = fee_warnings(&preview(7_050, 705, 141, None));
        assert_eq!(
            warnings,
            ["the fee (705 sat) is 10% of the amount being sent"]
        );
        // 150 sat/vB on a large amount: only the rate warning.
        let warnings = fee_warnings(&preview(10_000_000, 21_150, 141, None));
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("150.00 sat/vB is unusually high"),
            "{warnings:?}"
        );
        // Both.
        assert_eq!(fee_warnings(&preview(1_000, 21_150, 141, None)).len(), 2);
    }

    #[test]
    fn psbt_files_are_private_and_never_overwritten() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("payment.psbt");
        let psbt = Psbt::from_unsigned_tx(Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        })?;
        write_psbt(&path, &psbt)?;
        let written = std::fs::read_to_string(&path)?;
        assert_eq!(written.trim_end().parse::<Psbt>()?, psbt);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)?.permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        let err = write_psbt(&path, &psbt)
            .err()
            .ok_or_else(|| anyhow!("overwrote the file"))?;
        assert!(
            format!("{err:#}").contains("refusing to overwrite"),
            "{err:#}"
        );
        assert!(refuse_existing(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path)?, written);
        Ok(())
    }

    #[test]
    fn fee_rate_flag_overflow_is_an_error() {
        assert!(fee_rate_from_flag(u64::MAX).is_err());
        assert_eq!(
            fee_rate_from_flag(2).ok(),
            Some(FeeRate::from_sat_per_vb_u32(2))
        );
    }
}
