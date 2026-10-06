//! `btcw bump TXID --fee-rate N`: speed up an unconfirmed payment by replacing it (RBF, BIP125)
//! with one that pays the same recipient the same amount and a higher fee (PLAN-v2 §2).
//!
//! Same order as `send`:
//! 1. Checks that need neither the password nor the node: `--json` needs `--yes`, the txid,
//!    `--fee-rate` (1 to 25 000 sat/vB), the wallet exists, a terminal to confirm on.
//! 2. Open the wallet **watch-only**.
//! 3. `Node::connect` + sync: a payment that confirmed meanwhile can't be replaced, and the
//!    extra fee may need a coin that arrived since the last sync.
//! 4. `tx::prepare_fee_bump`: every rule (ours, unconfirmed, signals RBF, no unconfirmed child,
//!    rate high enough), the replacement PSBT, and the same preview as a payment, which starts
//!    with "Replaces <txid>". The desktop bridge calls the same function.
//! 5. The preview, what changes, warnings for unusual fees; `Send the replacement? [y/N]`.
//! 6. Only now the password → `api::load_signer`; sign, drop the signer, `tx::broadcast_signed`
//!    (extract → broadcast → record: the replaced payment leaves the wallet's history at once).
//!
//! From step 4 on, every way out except a successful broadcast releases any change address the
//! replacement reserved (`send::release`).

use anyhow::{Result, bail};
use btcw_core::WalletError;
use btcw_core::api;
use btcw_core::bitcoin::Network;
use btcw_core::chain::Node;
use btcw_core::config::Config;
use btcw_core::tx;
use btcw_core::types::SendPreview;
use serde::Serialize;

use crate::commands::send::{describe_preview, fee_rate_from_flag, fee_warnings, release};
use crate::commands::status::parse_txid;
use crate::commands::sync::sync_with_progress;
use crate::output::{Ui, group_thousands};
use crate::prompt::{self, Terminal};

#[derive(Debug, Serialize)]
struct BumpJson {
    network: String,
    /// The replacement's txid.
    txid: String,
    /// `preview.replaces` is the txid of the payment that was replaced.
    preview: SendPreview,
}

/// The `btcw bump` command line.
#[derive(Debug)]
pub struct Request<'a> {
    pub txid: &'a str,
    pub fee_rate_sat_vb: u64,
    pub yes: bool,
}

pub fn run(cfg: &Config, ui: &Ui, req: &Request<'_>) -> Result<()> {
    // 1. Fail fast.
    if ui.json() && !req.yes {
        bail!(
            "--json needs --yes: a JSON run can't stop to ask for confirmation; review the fee \
             bump without --json first"
        );
    }
    let txid = parse_txid(req.txid)?;
    let fee_rate = fee_rate_from_flag(req.fee_rate_sat_vb)?;
    tx::check_fee_rate(fee_rate)?;
    let mut terminal = if req.yes {
        None
    } else {
        Some(Terminal::open()?)
    };
    if !api::wallet_exists(cfg) {
        return Err(WalletError::WalletNotFound(cfg.network_dir()).into());
    }

    // 2–3. Watch-only, synced.
    let mut wallet = api::open_watch_only(cfg)?;
    let node = Node::connect(&cfg.rpc, cfg.network)?;
    sync_with_progress(ui, &node, &mut wallet)?;
    // For "the fee rises from … to …": the wallet knows the fee of its own payments.
    let old_fee = wallet
        .history()
        .into_iter()
        .find(|row| row.txid == txid.to_string())
        .and_then(|row| row.fee_sat);

    // 4. Same function as the desktop app's "Speed up".
    let (mut psbt, preview) = tx::prepare_fee_bump(&mut wallet, &node, txid, Some(fee_rate))?;

    // 5. Declined or failed: give back any change address the replacement reserved.
    if !ui.json() {
        let mut text = describe_preview(cfg.network, &preview, ui.out);
        if let Some(old) = old_fee {
            text.push_str(&format!(
                "\nThe fee rises by {} sat (from {} sat); {} still gets the same amount.",
                group_thousands(preview.fee_sat.saturating_sub(old)),
                group_thousands(old),
                preview.contact.as_deref().unwrap_or("the recipient")
            ));
        }
        ui.println(&text)?;
    }
    for warning in fee_warnings(&preview) {
        ui.warn(warning);
    }
    if cfg.network == Network::Bitcoin {
        ui.warn("this is mainnet: the replacement spends real bitcoin");
    }
    let approved = match terminal.as_mut() {
        None => Ok(true),
        Some(terminal) => terminal.confirm("Send the replacement?"),
    };
    match approved {
        Ok(true) => {}
        Ok(false) => {
            release(ui, &mut wallet, &psbt);
            return ui.println(&format!(
                "{} Cancelled; nothing was sent, and {txid} is still waiting.",
                ui.out.badge(cfg.network)
            ));
        }
        Err(e) => {
            release(ui, &mut wallet, &psbt);
            return Err(e);
        }
    }

    // 6. The password authorises this one replacement.
    let signer = match prompt::password(ui)
        .and_then(|password| Ok(api::load_signer(cfg, &wallet, &password)?))
    {
        Ok(signer) => signer,
        Err(e) => {
            release(ui, &mut wallet, &psbt);
            return Err(e);
        }
    };
    let signed = tx::sign_psbt(&wallet, &signer, &mut psbt);
    drop(signer);
    if let Err(e) = signed {
        release(ui, &mut wallet, &psbt);
        return Err(e.into());
    }
    // Releases the change address itself if the extract or the broadcast fails.
    let replacement = tx::broadcast_signed(&mut wallet, &node, psbt)?;
    drop(wallet);

    if ui.json() {
        return ui.print_json(&BumpJson {
            network: cfg.network.to_string(),
            txid: replacement.to_string(),
            preview,
        });
    }
    ui.println(&format!(
        "{} Sent the replacement: {}\nIt replaces {txid}, which nodes drop from their mempools \
         now; follow the new one with `btcw status {replacement} --watch`.",
        ui.out.badge(cfg.network),
        ui.out.bold(replacement),
    ))
}
