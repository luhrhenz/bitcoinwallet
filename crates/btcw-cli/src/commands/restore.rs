//! `btcw restore`: rebuild a wallet from its recovery phrase, then sync.

use anyhow::Result;
use btcw_core::WalletError;
use btcw_core::api::{self, Unlocked};
use btcw_core::chain::Node;
use btcw_core::config::Config;
use btcw_core::discover::BirthdayLookup;
use btcw_core::keys;
use btcw_core::types::{BalanceView, SyncReport};
use serde::Serialize;

use crate::commands::sync::{describe, sync_with_progress};
use crate::commands::view::balance_lines;
use crate::output::{ErrorBody, Ui, as_of};
use crate::prompt;

#[derive(Debug, Serialize)]
struct RestoreJson {
    network: String,
    birthday_height: u32,
    synced_height: u32,
    /// `null` when the node could not be reached or the sync failed (see `sync_error`).
    sync: Option<SyncReport>,
    sync_error: Option<ErrorBody>,
    balance: BalanceView,
    /// What `--find-birthday` found, if it was used.
    birthday_lookup: Option<BirthdayLookup>,
}

pub fn run(cfg: &Config, ui: &Ui, birthday: Option<u32>, find_birthday: bool) -> Result<()> {
    // The core checks again under the lock; this avoids asking for secrets first.
    if api::wallet_exists(cfg) {
        return Err(WalletError::WalletExists(cfg.network_dir()).into());
    }
    let phrase = prompt::recovery_phrase()?;
    // Validate before the password prompts. The error never names the words.
    let word_count = keys::parse_mnemonic(&phrase)?.word_count();
    let lookup = if find_birthday {
        let host = cfg
            .explorer_url
            .as_deref()
            .map(|url| url.split('/').take(3).collect::<Vec<_>>().join("/"))
            .unwrap_or_default();
        ui.note(format_args!(
            "asking {host} when this wallet was first used (this tells it the wallet's addresses)"
        ));
        let found = api::find_birthday(cfg, &phrase)?;
        if !ui.json() {
            let what = match found.first_used {
                Some(height) => format!("first used in block {height}"),
                None => "never used yet".to_owned(),
            };
            ui.println(&format!(
                "{} This wallet was {what}; scanning will start at block {} ({} addresses checked).",
                ui.out.badge(cfg.network),
                found.birthday,
                found.addresses_checked
            ))?;
        }
        Some(found)
    } else {
        None
    };
    let birthday = birthday.or(lookup.as_ref().map(|found| found.birthday));
    let password = prompt::new_password(ui)?;
    let Unlocked { mut wallet, signer } = api::restore_wallet(cfg, &phrase, &password, birthday)?;
    drop(phrase);
    drop(password);
    drop(signer);

    let net = ui.out.badge(cfg.network);
    let birthday_height = wallet.birthday_height();
    if !ui.json() {
        let start = match birthday_height {
            0 => "the genesis block".to_owned(),
            height => format!("block {height}"),
        };
        ui.println(&format!(
            "{net} Restored a {word_count}-word wallet in {}; scanning from {start}",
            cfg.network_dir().display()
        ))?;
    }

    if birthday_height == 0 {
        ui.warn(
            "no --birthday given, so the scan starts at the first block; through a hosted node that \
             can take many hours. To avoid it, stop now (Ctrl-C), delete the wallet folder above \
             and restore again with --find-birthday, or --birthday <height>",
        );
    }

    // A missing node only postpones the scan.
    let sync = Node::connect(&cfg.rpc, cfg.network)
        .and_then(|node| sync_with_progress(ui, &node, &mut wallet))
        .map_err(anyhow::Error::from);
    if let Err(e) = &sync {
        ui.warn(format_args!(
            "the wallet was restored but not synced: {e:#}\n  \
             run `btcw sync` once bitcoind is reachable to find its transactions"
        ));
    }
    let synced_height = wallet.synced_height();
    let balance = wallet.balance();
    drop(wallet);

    if ui.json() {
        let (sync, sync_error) = match &sync {
            Ok(report) => (Some(*report), None),
            Err(e) => (None, Some(ErrorBody::from_error(e))),
        };
        return ui.print_json(&RestoreJson {
            network: cfg.network.to_string(),
            birthday_height,
            synced_height,
            sync,
            sync_error,
            balance,
            birthday_lookup: lookup,
        });
    }
    match &sync {
        Ok(report) => ui.println(&format!(
            "{net} {}\n\nBalance {}\n{}",
            describe(report),
            as_of(synced_height),
            balance_lines(&balance, ui.out)
        )),
        // Nothing to show yet: an unsynced wallet has seen no transactions.
        Err(_) => ui.println(&format!("{net} Not synced yet: run `btcw sync`.")),
    }
}
