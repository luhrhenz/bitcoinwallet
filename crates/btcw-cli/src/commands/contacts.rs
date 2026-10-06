//! `btcw contacts list|add|remove|rename` (PLAN-v2 §1). Watch-only: no password, no node.
//!
//! The rules (name length, uniqueness ignoring case, an address for this network, no names that
//! look like addresses) live in `btcw_core::book`; this module only renders. Addresses are
//! printed in full, and in groups of four where the user is meant to check one.

use anyhow::Result;
use btcw_core::api;
use btcw_core::config::Config;
use btcw_core::types::Contact;
use serde::Serialize;

use crate::output::{Ui, grouped, plural, table};

#[derive(Debug, Serialize)]
struct ListJson {
    network: String,
    contacts: Vec<Contact>,
}

#[derive(Debug, Serialize)]
struct ContactJson {
    network: String,
    contact: Contact,
}

#[derive(Debug, Serialize)]
struct RemovedJson {
    network: String,
    removed: Contact,
}

#[derive(Debug, Serialize)]
struct RenamedJson {
    network: String,
    renamed_from: String,
    contact: Contact,
}

pub fn list(cfg: &Config, ui: &Ui) -> Result<()> {
    let wallet = api::open_watch_only(cfg)?;
    let contacts = wallet.contacts()?;
    drop(wallet);

    if ui.json() {
        return ui.print_json(&ListJson {
            network: cfg.network.to_string(),
            contacts,
        });
    }
    let net = ui.out.badge(cfg.network);
    if contacts.is_empty() {
        return ui.println(&format!(
            "{net} No contacts yet; add one with `btcw contacts add NAME ADDRESS`"
        ));
    }
    let mut rows = table(&["Name", "Address", "Note"], &[]);
    for contact in &contacts {
        rows.add_row(vec![
            contact.name.clone(),
            contact.address.clone(),
            contact.note.clone().unwrap_or_default(),
        ]);
    }
    ui.println(&format!(
        "{net} {}\n{rows}",
        plural(
            u32::try_from(contacts.len()).unwrap_or(u32::MAX),
            "contact",
            "contacts"
        )
    ))
}

pub fn add(cfg: &Config, ui: &Ui, name: &str, address: &str, note: Option<&str>) -> Result<()> {
    let mut wallet = api::open_watch_only(cfg)?;
    let contact = wallet.add_contact(name, address, note)?;
    drop(wallet);

    if ui.json() {
        return ui.print_json(&ContactJson {
            network: cfg.network.to_string(),
            contact,
        });
    }
    ui.println(&format!(
        "{} Saved contact {}\n{}",
        ui.out.badge(cfg.network),
        ui.out.bold(&contact.name),
        details(&contact)
    ))
}

pub fn remove(cfg: &Config, ui: &Ui, name: &str) -> Result<()> {
    let mut wallet = api::open_watch_only(cfg)?;
    let removed = wallet.remove_contact(name)?;
    drop(wallet);

    if ui.json() {
        return ui.print_json(&RemovedJson {
            network: cfg.network.to_string(),
            removed,
        });
    }
    ui.println(&format!(
        "{} Removed contact {} ({})",
        ui.out.badge(cfg.network),
        removed.name,
        removed.address
    ))
}

pub fn rename(cfg: &Config, ui: &Ui, old: &str, new: &str) -> Result<()> {
    let mut wallet = api::open_watch_only(cfg)?;
    // The saved spelling of the old name, for the message (the lookup ignores case).
    let renamed_from = wallet
        .contacts()?
        .into_iter()
        .find(|c| c.name.to_lowercase() == old.trim().to_lowercase())
        .map_or_else(|| old.trim().to_owned(), |c| c.name);
    let contact = wallet.rename_contact(old, new)?;
    drop(wallet);

    if ui.json() {
        return ui.print_json(&RenamedJson {
            network: cfg.network.to_string(),
            renamed_from,
            contact,
        });
    }
    ui.println(&format!(
        "{} Renamed contact {renamed_from} to {}\n{}",
        ui.out.badge(cfg.network),
        ui.out.bold(&contact.name),
        details(&contact)
    ))
}

/// ```text
///   Address   bcrt 1q6r z28m cfax tmd6 v789 l9rr lrus dprr 9pz3 cppk
///   Note      landlord
/// ```
fn details(contact: &Contact) -> String {
    let mut lines = vec![format!("  {:<10}{}", "Address", grouped(&contact.address))];
    if let Some(note) = &contact.note {
        lines.push(format!("  {:<10}{note}", "Note"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_show_the_full_address_grouped() {
        let contact = Contact {
            name: "Alice".into(),
            address: "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk".into(),
            note: Some("landlord".into()),
        };
        assert_eq!(
            details(&contact),
            "  Address   bcrt 1q6r z28m cfax tmd6 v789 l9rr lrus dprr 9pz3 cppk\n  Note      landlord"
        );
        let no_note = Contact {
            note: None,
            ..contact
        };
        assert!(!details(&no_note).contains("Note"));
    }
}
