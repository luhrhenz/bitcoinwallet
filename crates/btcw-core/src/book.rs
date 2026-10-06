//! The address book (contacts) and transaction labels: two more of btcw's own tables in
//! `wallet.sqlite`, next to `btcw_meta`.
//!
//! OWNER: Agent I (v2). Contract: PLAN-v2 §1.
//!
//! ```sql
//! CREATE TABLE btcw_contacts (name TEXT PRIMARY KEY COLLATE NOCASE, address TEXT NOT NULL, note TEXT);
//! CREATE TABLE btcw_labels   (txid TEXT PRIMARY KEY, label TEXT NOT NULL);
//! ```
//!
//! Each network has its own `wallet.sqlite`, so contacts are per network by construction, and an
//! address is validated for the wallet's network (`tx::parse_address`) before it is saved.
//!
//! Rules:
//! - Contact names are trimmed, 1–[`MAX_NAME_CHARS`] characters, unique ignoring case, and never
//!   look like an address ([`looks_like_address`]). So whatever the user types as a recipient is
//!   either an address or a name, never both, and a mistyped address can't match a contact.
//! - [`WalletService::resolve_recipient`] tries the input as an address first and only then as a
//!   name. A contact name is a shortcut for typing; frontends always show the full address next
//!   to it (`SendPreview::to`), never the name instead.
//! - Labels are trimmed, 1–[`MAX_LABEL_CHARS`] characters, one per txid, and only for
//!   transactions this wallet knows.
//! - No control characters anywhere (a newline or an escape sequence would break the CLI's tables
//!   or repaint the terminal).
//!
//! Writes go straight to SQLite (not staged with BDK's changes), like the backup flag.

use std::collections::HashMap;

use bdk_wallet::rusqlite::{Connection, ErrorCode};

use crate::bitcoin::address::NetworkUnchecked;
use crate::bitcoin::{Address, Txid};
use crate::error::{Result, WalletError};
use crate::tx;
use crate::types::Contact;
use crate::wallet::{WalletService, persist_err};

/// Longest contact name, in characters.
pub const MAX_NAME_CHARS: usize = 40;
/// Longest transaction label, in characters.
pub const MAX_LABEL_CHARS: usize = 100;
/// Longest contact note, in characters.
pub const MAX_NOTE_CHARS: usize = 200;

/// `COLLATE NOCASE` only folds ASCII; [`WalletService::add_contact`] also compares names with
/// Unicode lower-casing before inserting, so "Émile" and "émile" clash too.
const CREATE_CONTACTS_TABLE: &str = "CREATE TABLE IF NOT EXISTS btcw_contacts \
     (name TEXT PRIMARY KEY COLLATE NOCASE, address TEXT NOT NULL, note TEXT)";
const CREATE_LABELS_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS btcw_labels (txid TEXT PRIMARY KEY, label TEXT NOT NULL)";

/// Prefixes of SegWit addresses on every network btcw supports (bech32 is case-insensitive).
const SEGWIT_PREFIXES: [&str; 3] = ["bc1", "tb1", "bcrt1"];

/// A payment's destination as the user typed it: an address, or a contact's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    /// Checked for the wallet's network.
    pub address: Address,
    /// The contact's name (as saved) when the input was a name; `None` for a typed address.
    pub contact: Option<String>,
}

/// True if `input` is an address on any network, or looks like an attempt at one: it starts
/// with a SegWit prefix (`bc1`, `tb1`, `bcrt1`), or it is longer than a contact name can be.
///
/// Such input is never looked up in the address book, so a mistyped address is reported as an
/// invalid address and can't silently resolve to a contact. Contact names that would pass this
/// test are refused for the same reason.
pub fn looks_like_address(input: &str) -> bool {
    let input = input.trim();
    let lower = input.to_ascii_lowercase();
    SEGWIT_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
        || input.chars().count() > MAX_NAME_CHARS
        || input.parse::<Address<NetworkUnchecked>>().is_ok()
}

/// Both tables, if missing (databases from before v2 don't have them).
pub(crate) fn create_tables(db: &Connection) -> Result<()> {
    db.execute(CREATE_CONTACTS_TABLE, [])
        .and_then(|_| db.execute(CREATE_LABELS_TABLE, []))
        .map(|_| ())
        .map_err(|e| persist_err("creating the address book tables", e))
}

/// Every stored label. Rows whose txid doesn't parse weren't written by btcw; they are skipped.
pub(crate) fn load_labels(db: &Connection) -> Result<HashMap<Txid, String>> {
    let mut statement = db
        .prepare("SELECT txid, label FROM btcw_labels")
        .map_err(|e| persist_err("reading transaction labels", e))?;
    let rows: Vec<(String, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .and_then(|rows| rows.collect())
        .map_err(|e| persist_err("reading transaction labels", e))?;
    let mut labels = HashMap::with_capacity(rows.len());
    for (txid, label) in rows {
        match txid.parse::<Txid>() {
            Ok(txid) => {
                labels.insert(txid, label);
            }
            Err(_) => tracing::warn!(txid, "skipping a label stored for an invalid txid"),
        }
    }
    Ok(labels)
}

impl WalletService {
    /// Every contact, sorted by name (ignoring case).
    pub fn contacts(&self) -> Result<Vec<Contact>> {
        let mut statement = self
            .db()
            .prepare("SELECT name, address, note FROM btcw_contacts")
            .map_err(|e| persist_err("reading the address book", e))?;
        let mut contacts: Vec<Contact> = statement
            .query_map([], |row| {
                Ok(Contact {
                    name: row.get(0)?,
                    address: row.get(1)?,
                    note: row.get(2)?,
                })
            })
            .and_then(|rows| rows.collect())
            .map_err(|e| persist_err("reading the address book", e))?;
        contacts.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(contacts)
    }

    /// Save a new contact. The address must be valid for this wallet's network
    /// (`InvalidAddress` / `NetworkMismatch`) and is stored in its canonical form; the name must
    /// follow the rules in the module docs and not be taken, ignoring case (`Contact`). A blank
    /// note is no note.
    pub fn add_contact(
        &mut self,
        name: &str,
        address: &str,
        note: Option<&str>,
    ) -> Result<Contact> {
        let name = contact_name(name)?;
        let address = tx::parse_address(address, self.network())?;
        let note = contact_note(note)?;
        if let Some(existing) = self.find_contact(&name)? {
            return Err(WalletError::Contact(format!(
                "a contact named `{}` already exists",
                existing.name
            )));
        }
        let contact = Contact {
            name,
            address: address.to_string(),
            note,
        };
        self.db()
            .execute(
                "INSERT INTO btcw_contacts (name, address, note) VALUES (?1, ?2, ?3)",
                (&contact.name, &contact.address, &contact.note),
            )
            .map_err(|e| contact_write_err(&contact.name, "saving", e))?;
        Ok(contact)
    }

    /// Delete a contact (name matched ignoring case); returns what was removed.
    pub fn remove_contact(&mut self, name: &str) -> Result<Contact> {
        let contact = self.existing_contact(name)?;
        self.db()
            .execute("DELETE FROM btcw_contacts WHERE name = ?1", [&contact.name])
            .map_err(|e| persist_err("removing a contact", e))?;
        Ok(contact)
    }

    /// Rename a contact (`old` matched ignoring case). The new name follows the same rules as in
    /// [`WalletService::add_contact`]; changing only its case is allowed.
    pub fn rename_contact(&mut self, old: &str, new: &str) -> Result<Contact> {
        let contact = self.existing_contact(old)?;
        let new = contact_name(new)?;
        if let Some(other) = self.find_contact(&new)?
            && other.name != contact.name
        {
            return Err(WalletError::Contact(format!(
                "a contact named `{}` already exists",
                other.name
            )));
        }
        self.db()
            .execute(
                "UPDATE btcw_contacts SET name = ?1 WHERE name = ?2",
                (&new, &contact.name),
            )
            .map_err(|e| contact_write_err(&new, "renaming", e))?;
        Ok(Contact {
            name: new,
            ..contact
        })
    }

    /// What the user typed as a payment's destination: an address, or else a contact's name.
    ///
    /// 1. A valid address for this network is used as is (`contact: None`), even if a contact
    ///    has that address.
    /// 2. An address for another network is `NetworkMismatch`, and anything that
    ///    [`looks_like_address`] is `InvalidAddress`: never looked up as a name.
    /// 3. Otherwise it is a contact name, matched ignoring case; an unknown name is `Contact`.
    pub fn resolve_recipient(&self, input: &str) -> Result<Recipient> {
        let input = input.trim();
        match tx::parse_address(input, self.network()) {
            Ok(address) => Ok(Recipient {
                address,
                contact: None,
            }),
            Err(WalletError::InvalidAddress(_))
                if !input.is_empty() && !looks_like_address(input) =>
            {
                let Some(contact) = self.find_contact(input)? else {
                    return Err(WalletError::Contact(format!(
                        "no contact is named `{input}`, and it is not a valid address either"
                    )));
                };
                // Checked when it was saved; checked again in case the file was edited since.
                let address = tx::parse_address(&contact.address, self.network())?;
                Ok(Recipient {
                    address,
                    contact: Some(contact.name),
                })
            }
            Err(e) => Err(e),
        }
    }

    /// The label of a transaction, if it has one.
    pub fn label(&self, txid: Txid) -> Option<&str> {
        self.labels().get(&txid).map(String::as_str)
    }

    /// Label a transaction this wallet knows (one that appears in `history()`; `TxNotFound`
    /// otherwise), replacing any earlier label. Returns the label as stored (trimmed).
    pub fn set_label(&mut self, txid: Txid, label: &str) -> Result<String> {
        let label = checked_text("a label", label, MAX_LABEL_CHARS)?;
        if self.bdk().get_tx(txid).is_none() {
            return Err(WalletError::TxNotFound(txid.to_string()));
        }
        self.store_label(txid, &label)?;
        Ok(label)
    }

    /// Remove a transaction's label. `Ok(false)` if it had none; `TxNotFound` if it had none and
    /// the wallet doesn't know the transaction either (a mistyped txid).
    pub fn clear_label(&mut self, txid: Txid) -> Result<bool> {
        if !self.labels().contains_key(&txid) {
            return match self.bdk().get_tx(txid) {
                Some(_) => Ok(false),
                None => Err(WalletError::TxNotFound(txid.to_string())),
            };
        }
        self.db()
            .execute(
                "DELETE FROM btcw_labels WHERE txid = ?1",
                [txid.to_string()],
            )
            .map_err(|e| persist_err("removing a transaction label", e))?;
        self.labels_mut().remove(&txid);
        Ok(true)
    }

    /// After a fee bump: the replacement takes over the label of the payment it replaced, so the
    /// history keeps saying what the payment was for. Never overwrites a label of its own.
    /// Best effort (logged): the payment has been sent either way.
    pub(crate) fn inherit_label(&mut self, txid: Txid, replaced: &[Txid]) {
        if self.labels().contains_key(&txid) {
            return;
        }
        let Some(label) = replaced
            .iter()
            .find_map(|old| self.labels().get(old).cloned())
        else {
            return;
        };
        if let Err(e) = self.store_label(txid, &label) {
            tracing::warn!(%txid, error = %e, "could not copy the label to the replacement");
        }
    }

    fn store_label(&mut self, txid: Txid, label: &str) -> Result<()> {
        self.db()
            .execute(
                "INSERT OR REPLACE INTO btcw_labels (txid, label) VALUES (?1, ?2)",
                (txid.to_string(), label),
            )
            .map_err(|e| persist_err("saving a transaction label", e))?;
        self.labels_mut().insert(txid, label.to_owned());
        Ok(())
    }

    /// The contact whose name equals `name` ignoring case (Unicode lower-casing), if any.
    fn find_contact(&self, name: &str) -> Result<Option<Contact>> {
        let wanted = name.trim().to_lowercase();
        Ok(self
            .contacts()?
            .into_iter()
            .find(|contact| contact.name.to_lowercase() == wanted))
    }

    fn existing_contact(&self, name: &str) -> Result<Contact> {
        self.find_contact(name)?.ok_or_else(|| {
            WalletError::Contact(format!("no contact is named `{}`", shown(name.trim())))
        })
    }
}

/// A valid contact name, trimmed (`Contact` error otherwise).
fn contact_name(name: &str) -> Result<String> {
    let name = checked_text("a contact name", name, MAX_NAME_CHARS)?;
    if looks_like_address(&name) {
        return Err(WalletError::Contact(format!(
            "`{name}` looks like a Bitcoin address, so it can't be a contact name (a recipient \
             must always be clearly one or the other)"
        )));
    }
    Ok(name)
}

/// `None` for a missing or blank note, else the trimmed note.
fn contact_note(note: Option<&str>) -> Result<Option<String>> {
    match note.map(str::trim) {
        None | Some("") => Ok(None),
        Some(note) => checked_text("a note", note, MAX_NOTE_CHARS).map(Some),
    }
}

/// `text` trimmed: not empty, at most `max` characters, no control characters.
fn checked_text(what: &str, text: &str, max: usize) -> Result<String> {
    let text = text.trim();
    let chars = text.chars().count();
    if chars == 0 {
        return Err(WalletError::Contact(format!("{what} can't be empty")));
    }
    if chars > max {
        return Err(WalletError::Contact(format!(
            "{what} can be at most {max} characters (this one has {chars})"
        )));
    }
    if text.chars().any(char::is_control) {
        return Err(WalletError::Contact(format!(
            "{what} can't contain control characters such as line breaks or tabs"
        )));
    }
    Ok(text.to_owned())
}

/// User input echoed in an error, capped like `tx::parse_address` does.
fn shown(input: &str) -> String {
    let cut: String = input.chars().take(MAX_NAME_CHARS).collect();
    if cut.len() < input.len() {
        format!("{cut}…")
    } else {
        cut
    }
}

/// SQLite's own uniqueness check (ASCII case folding) backs up ours.
fn contact_write_err(name: &str, action: &str, e: bdk_wallet::rusqlite::Error) -> WalletError {
    if e.sqlite_error_code() == Some(ErrorCode::ConstraintViolation) {
        WalletError::Contact(format!("a contact named `{name}` already exists"))
    } else {
        persist_err(format!("{action} a contact"), e)
    }
}

/// Labels need transactions the wallet knows, which needs the crate-private `bdk_mut()` to fund
/// it without a node. The contact rules are tested through the public API in `tests/book.rs`.
#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;
    use crate::bitcoin::absolute::LockTime;
    use crate::bitcoin::hashes::Hash;
    use crate::bitcoin::transaction::Version;
    use crate::bitcoin::{Amount, Network, OutPoint, Transaction, TxIn, TxOut};
    use crate::config::{Config, Overrides};
    use crate::keys;

    type TestResult = std::result::Result<(), Box<dyn Error>>;

    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn config(dir: &std::path::Path) -> Result<Config> {
        Config::load_with(
            Overrides {
                datadir: Some(dir.to_path_buf()),
                network: Some("regtest".into()),
                ..Default::default()
            },
            |_| None,
        )
    }

    /// A wallet that knows one (made-up, unconfirmed) incoming transaction; returns its txid.
    fn wallet_with_one_tx(
        cfg: &Config,
    ) -> std::result::Result<(WalletService, Txid), Box<dyn Error>> {
        let (descriptors, _) =
            keys::derive_account(&keys::parse_mnemonic(ABANDON)?, "", Network::Regtest, 0)?;
        let mut wallet = WalletService::create(cfg, &descriptors, Some(0))?;
        let to = tx::parse_address(&wallet.new_address()?.address, Network::Regtest)?;
        let funding = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([9; 32]), 0),
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: to.script_pubkey(),
            }],
        };
        let txid = funding.compute_txid();
        wallet
            .bdk_mut()
            .apply_unconfirmed_txs([(funding, 1_700_000_000)]);
        wallet.persist()?;
        Ok((wallet, txid))
    }

    fn contact_error(
        result: Result<impl std::fmt::Debug>,
    ) -> std::result::Result<String, Box<dyn Error>> {
        match result {
            Err(WalletError::Contact(msg)) => Ok(msg),
            other => Err(format!("expected a Contact error, got {other:?}").into()),
        }
    }

    #[test]
    fn labels_are_trimmed_checked_shown_in_history_and_survive_reopen() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path())?;
        let (mut wallet, txid) = wallet_with_one_tx(&cfg)?;
        assert_eq!(wallet.history()[0].label, None);

        assert_eq!(
            wallet.set_label(txid, "  rent, October \n")?,
            "rent, October"
        );
        assert_eq!(wallet.label(txid), Some("rent, October"));
        assert_eq!(wallet.history()[0].label.as_deref(), Some("rent, October"));
        // A new label replaces the old one.
        wallet.set_label(txid, "rent")?;

        // 100 characters is fine, 101 is not; multi-byte characters count as one.
        wallet.set_label(txid, &"é".repeat(MAX_LABEL_CHARS))?;
        let msg = contact_error(wallet.set_label(txid, &"x".repeat(MAX_LABEL_CHARS + 1)))?;
        assert_eq!(
            msg,
            "a label can be at most 100 characters (this one has 101)"
        );
        let msg = contact_error(wallet.set_label(txid, "   "))?;
        assert_eq!(msg, "a label can't be empty");
        let msg = contact_error(wallet.set_label(txid, "two\nlines"))?;
        assert!(msg.contains("control characters"), "{msg}");
        let msg = contact_error(wallet.set_label(txid, "\u{1b}[31mred"))?;
        assert!(msg.contains("control characters"), "{msg}");
        wallet.set_label(txid, "rent")?;

        // Unknown transactions can't be labelled.
        let unknown = Txid::from_byte_array([42; 32]);
        match wallet.set_label(unknown, "nope") {
            Err(e @ WalletError::TxNotFound(_)) => {
                assert_eq!(e.to_string(), format!("transaction not found: {unknown}"));
            }
            other => return Err(format!("expected TxNotFound, got {other:?}").into()),
        }
        assert!(matches!(
            wallet.clear_label(unknown),
            Err(WalletError::TxNotFound(_))
        ));

        // Stored in the database, not just in memory.
        drop(wallet);
        let mut wallet = WalletService::open(&cfg, None)?;
        assert_eq!(wallet.label(txid), Some("rent"));
        assert_eq!(wallet.history()[0].label.as_deref(), Some("rent"));

        assert!(wallet.clear_label(txid)?);
        assert!(!wallet.clear_label(txid)?, "already clear");
        assert_eq!(wallet.history()[0].label, None);
        drop(wallet);
        assert_eq!(WalletService::open(&cfg, None)?.label(txid), None);
        Ok(())
    }

    #[test]
    fn a_replacement_inherits_the_label_but_keeps_its_own() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path())?;
        let (mut wallet, txid) = wallet_with_one_tx(&cfg)?;
        let other = Txid::from_byte_array([1; 32]);
        wallet.inherit_label(other, &[txid]);
        assert_eq!(wallet.label(other), None, "nothing to inherit yet");

        wallet.set_label(txid, "rent")?;
        wallet.inherit_label(other, &[txid]);
        assert_eq!(wallet.label(other), Some("rent"));
        wallet.store_label(other, "own")?;
        wallet.inherit_label(other, &[txid]);
        assert_eq!(wallet.label(other), Some("own"));
        Ok(())
    }

    #[test]
    fn address_lookalikes() {
        for input in [
            "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk",
            "BCRT1QNOTANADDRESS",
            "bc1",
            "tb1q",
            " tb1-friend",
            // A valid legacy address (base58), any network.
            "mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn",
            "x".repeat(MAX_NAME_CHARS + 1).as_str(),
        ] {
            assert!(looks_like_address(input), "{input}");
        }
        for input in [
            "Alice",
            "bob the builder",
            "b c1",
            "x".repeat(MAX_NAME_CHARS).as_str(),
        ] {
            assert!(!looks_like_address(input), "{input}");
        }
    }
}
