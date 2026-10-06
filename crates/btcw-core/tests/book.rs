//! The address book through the public API: add / list / rename / remove, the rules for names,
//! addresses and notes, persistence, and `resolve_recipient`. Labels are tested in `src/book.rs`.

use std::error::Error;
use std::path::Path;

use btcw_core::WalletError;
use btcw_core::bitcoin::{Address, CompressedPublicKey, Network};
use btcw_core::book::{self, MAX_NAME_CHARS, MAX_NOTE_CHARS};
use btcw_core::config::{Config, Overrides};
use btcw_core::keys;
use btcw_core::types::Contact;
use btcw_core::wallet::WalletService;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
/// `ABANDON`'s receive address #0 on regtest and on testnet4/signet.
const REGTEST_ADDRESS: &str = "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk";
const TESTNET_ADDRESS: &str = "tb1q6rz28mcfaxtmd6v789l9rrlrusdprr9pqcpvkl";
/// The generator point G: a public key nobody in these tests holds the key for.
const STRANGER_PUBKEY: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

/// Another valid regtest address (P2WPKH of `STRANGER_PUBKEY`).
fn other_address() -> TestResult<String> {
    let key: CompressedPublicKey = STRANGER_PUBKEY.parse()?;
    Ok(Address::p2wpkh(&key, Network::Regtest).to_string())
}

fn config(datadir: &Path) -> TestResult<Config> {
    Ok(Config::load_with(
        Overrides {
            datadir: Some(datadir.to_path_buf()),
            network: Some("regtest".into()),
            ..Default::default()
        },
        |_| None,
    )?)
}

fn new_wallet(cfg: &Config) -> TestResult<WalletService> {
    let (descriptors, _signer) =
        keys::derive_account(&keys::parse_mnemonic(ABANDON)?, "", Network::Regtest, 0)?;
    Ok(WalletService::create(cfg, &descriptors, Some(0))?)
}

fn contact_error<T: std::fmt::Debug>(result: btcw_core::Result<T>) -> TestResult<String> {
    match result {
        Err(e @ WalletError::Contact(_)) => {
            assert_eq!(e.code(), "contact");
            Ok(e.to_string())
        }
        other => Err(format!("expected a Contact error, got {other:?}").into()),
    }
}

fn contact(name: &str, address: &str, note: Option<&str>) -> Contact {
    Contact {
        name: name.into(),
        address: address.into(),
        note: note.map(Into::into),
    }
}

#[test]
fn contacts_add_list_rename_remove_and_survive_reopen() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg = config(dir.path())?;
    let mut wallet = new_wallet(&cfg)?;
    let other = other_address()?;
    assert!(wallet.contacts()?.is_empty());

    // Trimmed; upper-case bech32 is stored in its canonical lower-case form; blank note = none.
    let alice = wallet.add_contact(
        "  Alice ",
        &format!(" {} ", REGTEST_ADDRESS.to_uppercase()),
        Some("  landlord "),
    )?;
    assert_eq!(alice, contact("Alice", REGTEST_ADDRESS, Some("landlord")));
    let bob = wallet.add_contact("bob", &other, Some("   "))?;
    assert_eq!(bob, contact("bob", &other, None));
    // Two names for one address are fine.
    wallet.add_contact("Émile", REGTEST_ADDRESS, None)?;
    // Sorted by name, ignoring case.
    let names = |w: &WalletService| -> TestResult<Vec<String>> {
        Ok(w.contacts()?.into_iter().map(|c| c.name).collect())
    };
    assert_eq!(names(&wallet)?, ["Alice", "bob", "Émile"]);

    // Rename (old name matched ignoring case), including a change of case only.
    let renamed = wallet.rename_contact("ALICE", "Alicia")?;
    assert_eq!(
        renamed,
        contact("Alicia", REGTEST_ADDRESS, Some("landlord"))
    );
    assert_eq!(wallet.rename_contact("bob", "Bob")?.name, "Bob");
    assert_eq!(names(&wallet)?, ["Alicia", "Bob", "Émile"]);

    drop(wallet);
    let mut wallet = WalletService::open(&cfg, None)?;
    assert_eq!(names(&wallet)?, ["Alicia", "Bob", "Émile"]);
    assert_eq!(wallet.contacts()?[0], renamed);

    // Remove (ignoring case) returns what was removed.
    assert_eq!(
        wallet.remove_contact("émile")?,
        contact("Émile", REGTEST_ADDRESS, None)
    );
    drop(wallet);
    let mut wallet = WalletService::open(&cfg, None)?;
    assert_eq!(names(&wallet)?, ["Alicia", "Bob"]);

    // Unknown names.
    assert_eq!(
        contact_error(wallet.remove_contact("Carol"))?,
        "no contact is named `Carol`"
    );
    assert_eq!(
        contact_error(wallet.rename_contact("Carol", "Caroline"))?,
        "no contact is named `Carol`"
    );
    Ok(())
}

#[test]
fn contact_names_are_unique_ignoring_case() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut wallet = new_wallet(&config(dir.path())?)?;
    let other = other_address()?;
    wallet.add_contact("Alice", REGTEST_ADDRESS, None)?;
    wallet.add_contact("Émile", REGTEST_ADDRESS, None)?;
    wallet.add_contact("Bob", REGTEST_ADDRESS, None)?;

    for taken in ["alice", " ALICE ", "Alice"] {
        assert_eq!(
            contact_error(wallet.add_contact(taken, &other, None))?,
            "a contact named `Alice` already exists"
        );
    }
    // SQLite's NOCASE only folds ASCII; our own check covers the rest.
    assert_eq!(
        contact_error(wallet.add_contact("éMILE", &other, None))?,
        "a contact named `Émile` already exists"
    );
    assert_eq!(
        contact_error(wallet.rename_contact("Bob", "ALICE"))?,
        "a contact named `Alice` already exists"
    );
    assert_eq!(wallet.contacts()?.len(), 3, "nothing changed");
    Ok(())
}

#[test]
fn names_addresses_and_notes_are_checked() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut wallet = new_wallet(&config(dir.path())?)?;

    // Names: 1 to 40 characters (not bytes), no control characters, nothing address-like.
    wallet.add_contact(&"é".repeat(MAX_NAME_CHARS), REGTEST_ADDRESS, None)?;
    for (name, expected) in [
        ("", "a contact name can't be empty".to_owned()),
        ("   ", "a contact name can't be empty".to_owned()),
        (
            &"n".repeat(MAX_NAME_CHARS + 1),
            "a contact name can be at most 40 characters (this one has 41)".to_owned(),
        ),
        (
            "two\nlines",
            "a contact name can't contain control characters such as line breaks or tabs"
                .to_owned(),
        ),
        (
            "bcrt1q alice",
            "`bcrt1q alice` looks like a Bitcoin address, so it can't be a contact name (a \
             recipient must always be clearly one or the other)"
                .to_owned(),
        ),
    ] {
        assert_eq!(
            contact_error(wallet.add_contact(name, REGTEST_ADDRESS, None))?,
            expected,
            "{name:?}"
        );
    }
    // A legacy (base58) address is short enough to be a name, but it parses as an address.
    let key: CompressedPublicKey = STRANGER_PUBKEY.parse()?;
    let legacy = Address::p2pkh(key, Network::Regtest).to_string();
    for lookalike in [legacy.as_str(), "bc1 friend", "tb1", "BCRT1Q"] {
        let msg = contact_error(wallet.add_contact(lookalike, REGTEST_ADDRESS, None))?;
        assert!(msg.contains("looks like a Bitcoin address"), "{msg}");
    }

    // Addresses: valid, and for this wallet's network.
    assert!(matches!(
        wallet.add_contact("Testnet friend", TESTNET_ADDRESS, None),
        Err(WalletError::NetworkMismatch {
            expected: Network::Regtest,
            ..
        })
    ));
    assert!(matches!(
        wallet.add_contact("Typo", "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppl", None),
        Err(WalletError::InvalidAddress(_))
    ));

    // Notes: optional, up to 200 characters, no control characters.
    wallet.add_contact(
        "Long note",
        REGTEST_ADDRESS,
        Some(&"n".repeat(MAX_NOTE_CHARS)),
    )?;
    assert_eq!(
        contact_error(wallet.add_contact(
            "Longer note",
            REGTEST_ADDRESS,
            Some(&"n".repeat(MAX_NOTE_CHARS + 1))
        ))?,
        "a note can be at most 200 characters (this one has 201)"
    );
    assert!(
        contact_error(wallet.add_contact("Tab", REGTEST_ADDRESS, Some("a\tb")))?
            .contains("control characters")
    );

    // Renames follow the same rules.
    let msg = contact_error(wallet.rename_contact("Long note", &legacy))?;
    assert!(msg.contains("looks like a Bitcoin address"), "{msg}");
    assert_eq!(
        wallet.contacts()?.len(),
        2,
        "only the two valid contacts were saved"
    );
    Ok(())
}

#[test]
fn recipients_resolve_as_an_address_first_then_as_a_contact() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut wallet = new_wallet(&config(dir.path())?)?;
    let other = other_address()?;
    wallet.add_contact("Alice", &other, None)?;

    // A name, ignoring case and surrounding spaces; the saved spelling comes back.
    let alice = wallet.resolve_recipient("  alice ")?;
    assert_eq!(alice.address.to_string(), other);
    assert_eq!(alice.contact.as_deref(), Some("Alice"));

    // An address is used as is, even when a contact has it.
    let typed = wallet.resolve_recipient(&other)?;
    assert_eq!(typed.address.to_string(), other);
    assert_eq!(typed.contact, None);

    // Unknown name: a contact error that says it isn't an address either.
    match wallet.resolve_recipient("Alcie") {
        Err(e @ WalletError::Contact(_)) => assert_eq!(
            e.to_string(),
            "no contact is named `Alcie`, and it is not a valid address either"
        ),
        other => return Err(format!("expected Contact, got {other:?}").into()),
    }
    // Anything address-like keeps its address error and is never looked up as a name.
    assert!(matches!(
        wallet.resolve_recipient(TESTNET_ADDRESS),
        Err(WalletError::NetworkMismatch { .. })
    ));
    for typo in [
        "bcrt1qnotanaddress",
        "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppl",
        "",
        "   ",
    ] {
        assert!(
            matches!(
                wallet.resolve_recipient(typo),
                Err(WalletError::InvalidAddress(_))
            ),
            "{typo:?}"
        );
    }
    assert!(book::looks_like_address("bcrt1qnotanaddress"));
    assert!(!book::looks_like_address("Alice"));
    Ok(())
}
