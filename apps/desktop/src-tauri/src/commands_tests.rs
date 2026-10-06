//! Tests for `commands.rs`: the plain functions over a temporary datadir.
//!
//! Offline tests point at a closed port and a missing cookie, so node calls fail fast with
//! `rpc`. Node tests run a throwaway regtest `bitcoind` and skip when none is available.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use btcw_core::bitcoin::address::NetworkUnchecked;
use btcw_core::bitcoin::{Address, Network, Transaction, absolute, transaction};
use btcw_core::config::RpcAuth;
use btcw_core::testnode::TestNode;
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use crate::settings::SETTINGS_FILE;
use crate::state::test_clock::ManualClock;

const PASSWORD: &str = "correct horse battery";
const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
/// A regtest address that isn't ours (BIP173 vector re-encoded for `bcrt`).
const BCRT_ADDRESS: &str = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";
const TB_ADDRESS: &str = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx";

struct Fixture {
    dir: TempDir,
    state: AppState,
    clock: Arc<ManualClock>,
}

fn secret(s: &str) -> SecretString {
    SecretString::from(s)
}

/// Regtest, with node settings that fail fast (no node involved).
fn offline() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let missing_cookie = dir.path().join("no-such-cookie").display().to_string();
    with_env(
        dir,
        vec![
            ("BTCW_NETWORK", "regtest".to_owned()),
            // Port 9 (discard) has no bitcoind, and the missing cookie fails even sooner.
            ("BTCW_RPC_URL", "http://127.0.0.1:9".to_owned()),
            ("BTCW_RPC_COOKIE", missing_cookie),
        ],
    )
}

fn with_env(dir: TempDir, vars: Vec<(&'static str, String)>) -> Fixture {
    let clock = ManualClock::new();
    let env = move |key: &str| {
        vars.iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.clone())
    };
    let state = AppState::new(dir.path().to_path_buf(), Box::new(env), clock.clone());
    Fixture { dir, state, clock }
}

fn words_of(reply: &MnemonicReply) -> Vec<String> {
    match serde_json::to_value(reply).unwrap() {
        Value::Object(map) => match &map["mnemonic"] {
            Value::Array(words) => words
                .iter()
                .map(|w| w.as_str().unwrap().to_owned())
                .collect(),
            other => panic!("mnemonic is not an array: {other}"),
        },
        other => panic!("reply is not an object: {other}"),
    }
}

fn create(fx: &Fixture) -> Vec<String> {
    words_of(&create_wallet(&fx.state, 12, &secret(PASSWORD)).unwrap())
}

fn code<T: std::fmt::Debug>(result: ApiResult<T>) -> &'static str {
    result.unwrap_err().code
}

fn info(fx: &Fixture) -> AppInfo {
    app_info(&fx.state).unwrap()
}

/// A prepared payment without a node: an empty PSBT is enough for the session's bookkeeping.
fn fake_pending(fx: &Fixture, id: &str) {
    fake_pending_replacing(fx, id, None);
}

/// [`fake_pending`], as a fee bump of `replaces` when given.
fn fake_pending_replacing(fx: &Fixture, id: &str, replaces: Option<Txid>) {
    let tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![],
        output: vec![],
    };
    fx.state.session().set_pending(PendingSend {
        id: id.to_owned(),
        network: Network::Regtest,
        psbt: Psbt::from_unsigned_tx(tx).unwrap(),
        created: fx.state.now(),
        replaces,
    });
}

// ── Wallet lifecycle ────────────────────────────────────────────────────────────────────────

#[test]
fn create_then_lock_unlock_and_app_info() {
    let fx = offline();
    let before = info(&fx);
    assert_eq!(
        before,
        AppInfo {
            network: "regtest".into(),
            networks: config::selectable_networks()
                .iter()
                .map(ToString::to_string)
                .collect(),
            wallet_exists: false,
            unlocked: false,
            synced_height: None,
            backup_verified: None,
        }
    );
    assert_eq!(
        serde_json::to_value(&before).unwrap()["synced_height"],
        Value::Null
    );
    assert_eq!(code(balance(&fx.state)), "wallet_not_found");

    // Bad input is refused before anything is created.
    assert_eq!(
        code(create_wallet(&fx.state, 12, &secret("short"))),
        "weak_password"
    );
    assert_eq!(
        code(create_wallet(&fx.state, 13, &secret(PASSWORD))),
        "config"
    );
    assert!(!info(&fx).wallet_exists);

    // No node: the wallet is still created (birthday = genesis), and the app is unlocked.
    let reply = create_wallet(&fx.state, 24, &secret(PASSWORD)).unwrap();
    let words = words_of(&reply);
    assert_eq!(words.len(), 24);
    assert!(keys_parse(&words.join(" ")));
    // `Debug` never shows the words.
    let debug = format!("{reply:?}");
    assert!(debug.contains("24 words redacted"));
    assert!(!words.iter().any(|w| debug.contains(&format!(" {w} "))));

    let after = info(&fx);
    assert!(after.wallet_exists);
    assert!(after.unlocked);
    assert_eq!(after.synced_height, Some(0));
    assert_eq!(after.backup_verified, Some(false));
    assert_eq!(
        code(create_wallet(&fx.state, 12, &secret(PASSWORD))),
        "wallet_exists"
    );

    lock(&fx.state).unwrap();
    assert!(!info(&fx).unlocked);
    assert_eq!(
        code(unlock(&fx.state, &secret("not my password"))),
        "wrong_password"
    );
    assert!(!info(&fx).unlocked);
    unlock(&fx.state, &secret(PASSWORD)).unwrap();
    assert!(info(&fx).unlocked);

    // The wallet is not held open between commands: the CLI can open it right now.
    let cfg = fx.state.config().unwrap();
    let cli = api::open_watch_only(&cfg).unwrap();
    // ...and while the CLI has it, the app's commands report it, except `app_info`, which
    // answers from what it saw last.
    assert_eq!(code(balance(&fx.state)), "wallet_in_use");
    assert_eq!(info(&fx).synced_height, Some(0));
    drop(cli);
    assert_eq!(balance(&fx.state).unwrap().total_sat, 0);
}

fn keys_parse(phrase: &str) -> bool {
    btcw_core::keys::parse_mnemonic(phrase).is_ok()
}

#[test]
fn restore_is_verified_and_unlocked() {
    let fx = offline();
    assert_eq!(
        code(restore_wallet(
            &fx.state,
            &secret("abandon abandon"),
            &secret(PASSWORD),
            None
        )),
        "invalid_mnemonic"
    );
    restore_wallet(&fx.state, &secret(ABANDON), &secret(PASSWORD), Some(0)).unwrap();
    let after = info(&fx);
    assert!(after.unlocked);
    assert_eq!(after.backup_verified, Some(true));
    assert_eq!(
        code(restore_wallet(
            &fx.state,
            &secret(ABANDON),
            &secret(PASSWORD),
            None
        )),
        "wallet_exists"
    );
    // The first receive address of the BIP84 test vector, on regtest (coin type 1).
    assert_eq!(
        new_address(&fx.state).unwrap().address,
        "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk"
    );
}

#[test]
fn concurrent_commands_take_turns_instead_of_failing() {
    let fx = offline();
    create(&fx);
    let state = &fx.state;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                scope.spawn(move || match i % 3 {
                    0 => balance(state).map(|_| ()),
                    1 => history(state).map(|_| ()),
                    _ => app_info(state).map(|_| ()),
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
    });
}

// ── Backup ──────────────────────────────────────────────────────────────────────────────────

#[test]
fn backup_challenge_verify_and_reveal() {
    let fx = offline();
    let words = create(&fx);
    assert_eq!(info(&fx).backup_verified, Some(false));

    assert_eq!(
        code(backup_challenge(&fx.state, &secret("nope nope"))),
        "wrong_password"
    );
    let positions = backup_challenge(&fx.state, &secret(PASSWORD)).unwrap();
    assert_eq!(positions.len(), 3);
    assert!(positions.windows(2).all(|w| w[0] < w[1]));
    assert!(positions.iter().all(|p| (1..=12).contains(p)));

    let answer = |position: usize| words[position - 1].clone();
    let mut wrong: Vec<(usize, String)> = positions.iter().map(|&p| (p, answer(p))).collect();
    // A word from another position is still the wrong word here (unless it happens to repeat).
    let other = (1..=12)
        .find(|p| words[p - 1] != answer(positions[1]))
        .unwrap();
    wrong[1].1 = words[other - 1].clone();
    let err = verify_backup(&fx.state, &secret(PASSWORD), wrong).unwrap_err();
    assert_eq!(err.code, "backup_mismatch");
    assert_eq!(
        err.message,
        format!("word {} does not match your recovery phrase", positions[1])
    );
    assert!(!words.iter().any(|w| err.message.contains(w.as_str())));
    assert_eq!(info(&fx).backup_verified, Some(false));

    let right: Vec<(usize, String)> = positions
        .iter()
        .map(|&p| (p, format!("  {} ", answer(p).to_uppercase())))
        .collect();
    assert_eq!(
        code(verify_backup(
            &fx.state,
            &secret("wrong password"),
            right.clone()
        )),
        "wrong_password"
    );
    // Fewer than three words is not a check.
    assert_eq!(
        code(verify_backup(
            &fx.state,
            &secret(PASSWORD),
            right[..2].to_vec()
        )),
        "config"
    );
    verify_backup(&fx.state, &secret(PASSWORD), right).unwrap();
    assert_eq!(info(&fx).backup_verified, Some(true));

    // Shown again, any time, with the password; locked or not.
    lock(&fx.state).unwrap();
    assert_eq!(
        code(reveal_phrase(&fx.state, &secret("nope nope"))),
        "wrong_password"
    );
    let revealed = words_of(&reveal_phrase(&fx.state, &secret(PASSWORD)).unwrap());
    assert_eq!(revealed, words);
    assert!(
        !info(&fx).unlocked,
        "revealing the phrase does not unlock sending"
    );
}

#[test]
fn backup_commands_need_a_wallet() {
    let fx = offline();
    assert_eq!(
        code(backup_challenge(&fx.state, &secret(PASSWORD))),
        "wallet_not_found"
    );
    assert_eq!(
        code(reveal_phrase(&fx.state, &secret(PASSWORD))),
        "wallet_not_found"
    );
    assert_eq!(
        code(verify_backup(
            &fx.state,
            &secret(PASSWORD),
            vec![(1, "abandon".into())]
        )),
        "wallet_not_found"
    );
}

// ── Settings ────────────────────────────────────────────────────────────────────────────────

#[test]
fn settings_validate_persist_and_switch_networks() {
    let fx = offline();
    create(&fx);
    let current = get_settings(&fx.state).unwrap();
    assert_eq!(
        serde_json::to_value(&current).unwrap(),
        json!({ "network": "regtest", "rpc_url": null, "rpc_cookie": null,
                "auto_lock_minutes": 5, "mainnet_opt_in": false })
    );

    let with = |f: &dyn Fn(&mut Settings)| {
        let mut s = current.clone();
        f(&mut s);
        s
    };
    for (bad, expected) in [
        (with(&|s| s.auto_lock_minutes = 0), "config"),
        (with(&|s| s.auto_lock_minutes = 61), "config"),
        (with(&|s| s.network = "testnet".into()), "config"),
        (with(&|s| s.network = "bitcoin".into()), "mainnet_disabled"),
        (
            with(&|s| s.rpc_url = Some("127.0.0.1:18443".into())),
            "config",
        ),
    ] {
        assert_eq!(code(set_settings(&fx.state, &bad)), expected, "{bad:?}");
    }
    assert!(
        !fx.dir.path().join(SETTINGS_FILE).exists(),
        "nothing saved yet"
    );
    assert!(info(&fx).unlocked, "a refused change locks nothing");

    // Same network: saved (privately), applied, still unlocked.
    let longer = with(&|s| s.auto_lock_minutes = 7);
    set_settings(&fx.state, &longer).unwrap();
    assert_eq!(get_settings(&fx.state).unwrap(), longer);
    assert!(info(&fx).unlocked);
    let file = fx.dir.path().join(SETTINGS_FILE);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["auto_lock_minutes"], 7);
    assert_eq!(saved["network"], "regtest");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // Another network: locked, the prepared payment dropped, the other network's (empty) wallet.
    fake_pending(&fx, "aa");
    let signet_settings = Settings {
        network: "signet".into(),
        ..longer.clone()
    };
    set_settings(&fx.state, &signet_settings).unwrap();
    let signet = info(&fx);
    assert_eq!(signet.network, "signet");
    assert!(!signet.wallet_exists);
    assert!(!signet.unlocked);
    assert!(!fx.state.session().has_pending());

    // desktop.json beats BTCW_NETWORK (overrides > env), and a restart reads it back.
    let restarted = AppState::new(
        fx.dir.path().to_path_buf(),
        Box::new(|key: &str| (key == "BTCW_NETWORK").then(|| "regtest".to_owned())),
        fx.clock.clone(),
    );
    assert_eq!(get_settings(&restarted).unwrap().network, "signet");
    assert_eq!(get_settings(&restarted).unwrap().auto_lock_minutes, 7);

    // Back to regtest: the wallet is there, and locked.
    set_settings(&fx.state, &longer).unwrap();
    let back = info(&fx);
    assert!(back.wallet_exists);
    assert!(!back.unlocked);
}

// ── Sending ─────────────────────────────────────────────────────────────────────────────────

#[test]
fn sending_needs_the_signer_and_a_known_id() {
    let fx = offline();
    create(&fx);
    lock(&fx.state).unwrap();
    assert_eq!(
        code(prepare_send(&fx.state, BCRT_ADDRESS, 10_000, None)),
        "locked"
    );
    assert_eq!(code(confirm_send(&fx.state, "00")), "locked");

    unlock(&fx.state, &secret(PASSWORD)).unwrap();
    // Refused before the node is contacted (which would be `rpc` here).
    for (to, amount, rate, expected) in [
        // Not address-like: it could be a contact's name, and there is none by that name.
        ("not an address", 10_000, None, "contact"),
        ("bcrt1qnotanaddress", 10_000, None, "invalid_address"),
        (TB_ADDRESS, 10_000, None, "network_mismatch"),
        (BCRT_ADDRESS, 293, None, "dust_amount"),
        (BCRT_ADDRESS, 10_000, Some(0.5), "tx_build"),
        (BCRT_ADDRESS, 10_000, Some(f64::NAN), "tx_build"),
        (BCRT_ADDRESS, 10_000, Some(30_000.0), "tx_build"),
        (BCRT_ADDRESS, 10_000, Some(-1.0), "tx_build"),
    ] {
        assert_eq!(
            code(prepare_send(&fx.state, to, amount, rate)),
            expected,
            "{to} {amount} {rate:?}"
        );
    }
    // Valid input: it gets as far as syncing, and the node isn't there.
    assert_eq!(
        code(prepare_send(&fx.state, BCRT_ADDRESS, 10_000, Some(2.5))),
        "rpc"
    );

    let unknown = confirm_send(&fx.state, "0123456789abcdef0123456789abcdef").unwrap_err();
    assert_eq!(unknown.code, "tx_build");
    assert!(
        unknown.message.contains("review it again"),
        "{}",
        unknown.message
    );
    cancel_send(&fx.state, "0123456789abcdef0123456789abcdef").unwrap();

    // A known id is removed whatever happens: here the node is down, then it's gone.
    fake_pending(&fx, "beef");
    assert_eq!(code(confirm_send(&fx.state, "feed")), "tx_build");
    assert!(
        fx.state.session().has_pending(),
        "another id leaves it alone"
    );
    assert_eq!(code(confirm_send(&fx.state, "beef")), "rpc");
    assert!(!fx.state.session().has_pending());
    assert_eq!(code(confirm_send(&fx.state, "beef")), "tx_build");

    // Cancel forgets it.
    fake_pending(&fx, "cafe");
    cancel_send(&fx.state, "cafe").unwrap();
    assert!(!fx.state.session().has_pending());
    assert_eq!(code(confirm_send(&fx.state, "cafe")), "tx_build");
}

#[test]
fn prepared_payments_expire() {
    let fx = offline();
    create(&fx);
    fake_pending(&fx, "beef");
    // Stay active (so the auto-lock doesn't interfere) past the 10-minute lifetime.
    for _ in 0..3 {
        fx.clock.advance(Duration::from_secs(4 * 60));
        keep_alive(&fx.state).unwrap();
    }
    let expired = confirm_send(&fx.state, "beef").unwrap_err();
    assert_eq!(expired.code, "tx_build");
    assert!(expired.message.contains("expired"), "{}", expired.message);
    assert!(!fx.state.session().has_pending());
}

#[test]
fn tx_status_answers_from_the_last_sync_without_a_node() {
    let fx = offline();
    create(&fx);
    assert_eq!(code(tx_status(&fx.state, "not a txid")), "tx_not_found");
    let unknown = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
    assert_eq!(tx_status(&fx.state, unknown).unwrap(), None);
    assert_eq!(code(sync(&fx.state, &mut |_| {})), "rpc");
}

// ── Address book, labels, speed up (offline) ────────────────────────────────────────────────

/// A txid no wallet here has ever seen (the genesis block's coinbase).
const UNKNOWN_TXID: &str = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";

#[test]
fn contacts_are_edited_watch_only_with_the_core_rules() {
    let fx = offline();
    assert_eq!(code(list_contacts(&fx.state)), "wallet_not_found");
    assert_eq!(
        code(add_contact(&fx.state, "Alice", BCRT_ADDRESS, None)),
        "wallet_not_found"
    );
    create(&fx);
    lock(&fx.state).unwrap();
    assert_eq!(list_contacts(&fx.state).unwrap(), vec![]);

    // Saved while locked: no password needed. Trimmed; a blank note is no note.
    let alice = add_contact(&fx.state, "  Alice ", BCRT_ADDRESS, Some("rent")).unwrap();
    assert_eq!(
        alice,
        Contact {
            name: "Alice".into(),
            address: BCRT_ADDRESS.into(),
            note: Some("rent".into()),
        }
    );
    let bob = add_contact(&fx.state, "bob", &BCRT_ADDRESS.to_uppercase(), Some("  ")).unwrap();
    assert_eq!(
        bob.address, BCRT_ADDRESS,
        "stored in canonical (lower) case"
    );
    assert_eq!(bob.note, None);
    assert_eq!(
        serde_json::to_value(list_contacts(&fx.state).unwrap()).unwrap(),
        json!([
            { "name": "Alice", "address": BCRT_ADDRESS, "note": "rent" },
            { "name": "bob", "address": BCRT_ADDRESS, "note": null },
        ])
    );

    for (name, address, note, expected) in [
        ("ALICE", BCRT_ADDRESS, None, "contact"),
        ("", BCRT_ADDRESS, None, "contact"),
        ("bcrt1qnotanaddress", BCRT_ADDRESS, None, "contact"),
        (&*"x".repeat(41), BCRT_ADDRESS, None, "contact"),
        (
            "Line
break",
            BCRT_ADDRESS,
            None,
            "contact",
        ),
        ("Carol", BCRT_ADDRESS, Some(&*"n".repeat(201)), "contact"),
        ("Carol", TB_ADDRESS, None, "network_mismatch"),
        ("Carol", "not an address", None, "invalid_address"),
    ] {
        assert_eq!(
            code(add_contact(&fx.state, name, address, note)),
            expected,
            "{name:?} {address} {note:?}"
        );
    }
    let duplicate = add_contact(&fx.state, "alice", BCRT_ADDRESS, None).unwrap_err();
    assert_eq!(duplicate.message, "a contact named `Alice` already exists");

    // Rename (case-insensitive lookup), refusing a taken name; a change of case is fine.
    assert_eq!(
        rename_contact(&fx.state, "ALICE", "Alice B.").unwrap().name,
        "Alice B."
    );
    assert_eq!(
        code(rename_contact(&fx.state, "Alice B.", "BOB")),
        "contact"
    );
    assert_eq!(code(rename_contact(&fx.state, "nobody", "Zed")), "contact");
    assert_eq!(rename_contact(&fx.state, "bob", "Bob").unwrap().name, "Bob");

    // Remove returns what was removed; a second time there is nothing to remove.
    assert_eq!(remove_contact(&fx.state, "bob").unwrap().name, "Bob");
    assert_eq!(code(remove_contact(&fx.state, "bob")), "contact");
    let names: Vec<String> = list_contacts(&fx.state)
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, ["Alice B."]);
    assert!(!info(&fx).unlocked, "editing contacts never unlocks");
}

#[test]
fn wallet_edits_count_as_activity_and_listing_does_not() {
    let fx = offline();
    create(&fx); // unlocked; auto-lock 5 minutes (+ 1 minute grace on this side)
    for i in 0..3 {
        fx.clock.advance(Duration::from_secs(5 * 60));
        add_contact(&fx.state, &format!("Friend {i}"), BCRT_ADDRESS, None).unwrap();
    }
    assert!(info(&fx).unlocked, "15 minutes, a contact saved every 5");
    fx.clock.advance(Duration::from_secs(4 * 60));
    list_contacts(&fx.state).unwrap();
    fx.clock.advance(Duration::from_secs(2 * 60 + 1));
    list_contacts(&fx.state).unwrap();
    assert!(
        !info(&fx).unlocked,
        "reading the address book is not activity"
    );
}

#[test]
fn send_resolves_a_contact_name_before_the_node() {
    let fx = offline();
    create(&fx);
    add_contact(&fx.state, "Alice", BCRT_ADDRESS, None).unwrap();
    lock(&fx.state).unwrap();
    assert_eq!(
        code(prepare_send(&fx.state, "alice", 10_000, None)),
        "locked"
    );
    unlock(&fx.state, &secret(PASSWORD)).unwrap();

    for (to, amount, rate, expected) in [
        // An unknown name, and a dust amount for a known one: before the node.
        ("Alcie", 10_000, None, "contact"),
        ("alice", 293, None, "dust_amount"),
        ("alice", 10_000, Some(0.5), "tx_build"),
        // Address-like input is never looked up as a name, so typos stay address errors.
        ("bcrt1qnotanaddress", 10_000, None, "invalid_address"),
        (TB_ADDRESS, 10_000, None, "network_mismatch"),
        ("", 10_000, None, "invalid_address"),
        // A known name (any case, padded) resolves and gets as far as the node.
        (" ALICE ", 10_000, None, "rpc"),
    ] {
        assert_eq!(
            code(prepare_send(&fx.state, to, amount, rate)),
            expected,
            "{to:?} {amount} {rate:?}"
        );
    }
    let unknown = prepare_send(&fx.state, "Alcie", 10_000, None).unwrap_err();
    assert_eq!(
        unknown.message,
        "no contact is named `Alcie`, and it is not a valid address either"
    );
}

#[test]
fn labels_need_a_transaction_the_wallet_knows() {
    let fx = offline();
    assert_eq!(
        code(set_label(&fx.state, UNKNOWN_TXID, "rent")),
        "wallet_not_found"
    );
    create(&fx);
    lock(&fx.state).unwrap();
    for txid in [UNKNOWN_TXID, "not a txid", ""] {
        assert_eq!(
            code(set_label(&fx.state, txid, "rent")),
            "tx_not_found",
            "{txid:?}"
        );
        assert_eq!(
            code(clear_label(&fx.state, txid)),
            "tx_not_found",
            "{txid:?}"
        );
    }
    // The label's own rules come first (no wallet lookup needed for them).
    assert_eq!(code(set_label(&fx.state, UNKNOWN_TXID, "  ")), "contact");
    assert_eq!(
        code(set_label(&fx.state, UNKNOWN_TXID, &"x".repeat(101))),
        "contact"
    );
}

#[test]
fn speed_up_needs_the_signer_and_a_transaction_the_wallet_sent() {
    let fx = offline();
    create(&fx);
    lock(&fx.state).unwrap();
    assert_eq!(
        code(prepare_fee_bump(&fx.state, UNKNOWN_TXID, None)),
        "locked"
    );
    assert_eq!(
        code(prepare_fee_bump(&fx.state, "nope", Some(5.0))),
        "locked"
    );
    // The minimum is a watch-only question: no password, and an unknown txid says so.
    assert_eq!(
        code(min_fee_bump_rate(&fx.state, UNKNOWN_TXID)),
        "tx_not_found"
    );
    assert_eq!(code(min_fee_bump_rate(&fx.state, "nope")), "tx_not_found");

    unlock(&fx.state, &secret(PASSWORD)).unwrap();
    fake_pending(&fx, "beef");
    for (txid, rate, expected) in [
        ("not a txid", None, "tx_not_found"),
        (UNKNOWN_TXID, Some(0.5), "tx_build"),
        (UNKNOWN_TXID, Some(f64::NAN), "tx_build"),
        // Refused from the wallet file, before the node (which would be `rpc` here).
        (UNKNOWN_TXID, None, "tx_not_found"),
        (UNKNOWN_TXID, Some(5.0), "tx_not_found"),
    ] {
        assert_eq!(
            code(prepare_fee_bump(&fx.state, txid, rate)),
            expected,
            "{txid} {rate:?}"
        );
    }
    // Like a new payment preview, a fee bump that got as far as the wallet replaced the old one.
    assert!(!fx.state.session().has_pending());

    // A prepared bump lives in the same slot, with the same rules: confirm uses it up (here the
    // node is down), cancel forgets it, and a lock drops it.
    let original = Txid::from_str(UNKNOWN_TXID).unwrap();
    fake_pending_replacing(&fx, "b1", Some(original));
    assert_eq!(code(confirm_send(&fx.state, "b1")), "rpc");
    assert!(!fx.state.session().has_pending());
    fake_pending_replacing(&fx, "b2", Some(original));
    cancel_send(&fx.state, "b2").unwrap();
    assert_eq!(code(confirm_send(&fx.state, "b2")), "tx_build");
    fake_pending_replacing(&fx, "b3", Some(original));
    lock(&fx.state).unwrap();
    assert!(!fx.state.session().has_pending());
}

#[test]
fn the_minimum_bump_rate_is_rounded_up_and_accepted_back() {
    let shown = |kwu: u64| sat_vb_rounded_up(FeeRate::from_sat_per_kwu(kwu));
    assert_eq!(shown(751), 3.01, "BIP125 rule 4's 3.004 sat/vB");
    assert_eq!(shown(750), 3.0);
    assert_eq!(shown(500), 2.0);
    assert_eq!(shown(1_500), 6.0);
    assert_eq!(shown(1_501), 6.01);
    // Typing the number shown always gives at least the minimum back.
    for kwu in 250..=50_000 {
        let back = fee_rate_from_sat_vb(shown(kwu)).unwrap().to_sat_per_kwu();
        assert!(
            back >= kwu,
            "{kwu} sat/kWU shown as {} came back as {back}",
            shown(kwu)
        );
        assert!(
            back <= kwu + 3,
            "{kwu} sat/kWU came back as {back}: rounded up too far"
        );
    }
}

// ── Auto-lock ───────────────────────────────────────────────────────────────────────────────

#[test]
fn auto_lock_backstop_wipes_the_signer_after_idle_time() {
    let fx = offline();
    create(&fx); // unlocked; auto-lock 5 minutes (+ 1 minute grace on this side)
    fake_pending(&fx, "beef");

    // Background reads (polls, refreshes) don't count as activity...
    fx.clock.advance(Duration::from_secs(4 * 60));
    balance(&fx.state).unwrap();
    tx_status(
        &fx.state,
        "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b",
    )
    .unwrap();
    fx.clock.advance(Duration::from_secs(2 * 60 - 1));
    assert!(info(&fx).unlocked, "5:59 idle: within the limit");
    fx.clock.advance(Duration::from_secs(2));
    // ...so after 6 minutes the next command of any kind locks first.
    let after = info(&fx);
    assert!(!after.unlocked);
    assert!(!fx.state.session().has_pending());
    assert_eq!(code(confirm_send(&fx.state, "beef")), "locked");
}

#[test]
fn user_activity_keeps_the_session_alive() {
    let fx = offline();
    create(&fx);
    for _ in 0..5 {
        fx.clock.advance(Duration::from_secs(5 * 60));
        keep_alive(&fx.state).unwrap();
    }
    assert!(info(&fx).unlocked, "25 minutes, active every 5");
    fx.clock.advance(Duration::from_secs(5 * 60));
    new_address(&fx.state).unwrap();
    assert!(info(&fx).unlocked, "a user command counts too");

    // A shorter setting applies at once.
    let mut s = get_settings(&fx.state).unwrap();
    s.auto_lock_minutes = 1;
    set_settings(&fx.state, &s).unwrap();
    fx.clock.advance(Duration::from_secs(2 * 60 + 1));
    assert!(!info(&fx).unlocked);
}

// ── Small pieces ────────────────────────────────────────────────────────────────────────────

#[test]
fn fee_rates_convert_exactly_and_round_up() {
    let kwu = |rate: f64| fee_rate_from_sat_vb(rate).unwrap().to_sat_per_kwu();
    assert_eq!(kwu(1.0), 250);
    assert_eq!(kwu(2.0), 500);
    assert_eq!(kwu(2.5), 625);
    assert_eq!(kwu(2.3), 575, "float noise must not round a whole step up");
    assert_eq!(kwu(1.001), 251, "never below the rate asked for");
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e12] {
        assert!(fee_rate_from_sat_vb(bad).is_err(), "{bad}");
    }
}

#[test]
fn progress_events_are_throttled_but_the_last_one_arrives() {
    let start = Instant::now();
    let at = |ms: u64| start + Duration::from_millis(ms);
    let p = |height| SyncProgress {
        height,
        tip_height: 100,
    };
    let mut throttle = ProgressThrottle::new(Duration::from_millis(50));
    assert_eq!(throttle.offer(at(0), p(1)), Some(p(1)));
    assert_eq!(throttle.offer(at(10), p(2)), None);
    assert_eq!(throttle.offer(at(49), p(3)), None);
    assert_eq!(throttle.offer(at(50), p(4)), Some(p(4)));
    assert_eq!(throttle.offer(at(60), p(5)), None);
    assert_eq!(throttle.finish(), Some(p(5)));
    assert_eq!(throttle.finish(), None);
    // Reaching the tip is always shown.
    assert_eq!(throttle.offer(at(61), p(100)), Some(p(100)));
    assert_eq!(throttle.finish(), None);
}

#[test]
fn ids_are_random_128_bit_hex() {
    let a = random_id().unwrap();
    let b = random_id().unwrap();
    assert_eq!(a.len(), 32);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b);
}

// ── Against a real node ─────────────────────────────────────────────────────────────────────

#[test]
fn send_flow_against_a_regtest_node() {
    if !TestNode::available() {
        eprintln!("skipping: no bitcoind (set BITCOIND_EXE to run this test)");
        return;
    }
    let node = TestNode::start().unwrap();
    let rpc = node.rpc_config();
    let RpcAuth::Cookie(cookie) = &rpc.auth else {
        panic!("TestNode uses cookie auth");
    };
    let fx = with_env(
        tempfile::tempdir().unwrap(),
        vec![
            ("BTCW_NETWORK", "regtest".to_owned()),
            ("BTCW_RPC_URL", rpc.url.clone()),
            (
                "BTCW_RPC_COOKIE",
                PathBuf::from(cookie).display().to_string(),
            ),
        ],
    );
    let state = &fx.state;

    // Create: the birthday comes from the node, as in `btcw create`.
    create(&fx);
    let start = node.tip_height().unwrap();
    let receive = new_address(state).unwrap().address;
    let receive: Address = receive
        .parse::<Address<NetworkUnchecked>>()
        .unwrap()
        .require_network(Network::Regtest)
        .unwrap();
    node.fund(&receive, btcw_core::bitcoin::Amount::from_sat(1_000_000))
        .unwrap();
    node.mine(1).unwrap();

    // Sync, with progress events.
    let mut events = Vec::new();
    let report = sync(state, &mut |p| events.push(p)).unwrap();
    let tip = node.tip_height().unwrap();
    assert_eq!(report.tip_height, tip);
    assert_eq!(
        report.blocks_scanned,
        tip - start + 1,
        "from the birthday block on"
    );
    assert_eq!(
        events.last(),
        Some(&SyncProgress {
            height: tip,
            tip_height: tip
        })
    );
    assert_eq!(info(&fx).synced_height, Some(tip));
    assert_eq!(balance(state).unwrap().confirmed_sat, 1_000_000);

    // Prepare → cancel: nothing sent, the id is dead.
    let to = node.faucet_address().unwrap().to_string();
    let first = prepare_send(state, &to, 100_000, Some(2.0)).unwrap();
    assert_eq!(first.id.len(), 32);
    assert_eq!(first.preview.to, to);
    assert_eq!(first.preview.amount_sat, 100_000);
    assert_eq!(
        first.preview.total_sat,
        first.preview.amount_sat + first.preview.fee_sat
    );
    assert!(first.preview.change_sat.is_some());
    cancel_send(state, &first.id).unwrap();
    assert_eq!(code(confirm_send(state, &first.id)), "tx_build");

    // A newer preview replaces an older one.
    let second = prepare_send(state, &to, 100_000, None).unwrap();
    let third = prepare_send(state, &to, 100_000, Some(3.0)).unwrap();
    assert_ne!(second.id, third.id);
    assert_eq!(code(confirm_send(state, &second.id)), "tx_build");

    // Confirm → broadcast; the id can't be used twice.
    let sent = confirm_send(state, &third.id).unwrap();
    assert_eq!(code(confirm_send(state, &third.id)), "tx_build");
    let mempool = node.call("getrawmempool", &[]).unwrap();
    assert!(
        mempool
            .as_array()
            .unwrap()
            .iter()
            .any(|txid| txid == sent.txid.as_str())
    );
    assert!(matches!(
        tx_status(state, &sent.txid).unwrap(),
        Some(TxStatus::Unconfirmed { .. })
    ));
    assert_eq!(
        balance(state).unwrap().unconfirmed_sat,
        1_000_000 - third.preview.total_sat
    );

    node.mine(1).unwrap();
    assert_eq!(
        tx_status(state, &sent.txid).unwrap(),
        Some(TxStatus::Confirmed {
            height: tip + 1,
            confirmations: 1,
            block_time: match tx_status(state, &sent.txid).unwrap() {
                Some(TxStatus::Confirmed { block_time, .. }) => block_time,
                other => panic!("not confirmed: {other:?}"),
            },
        })
    );
    let after = balance(state).unwrap();
    assert_eq!(after.confirmed_sat, 1_000_000 - third.preview.total_sat);
    assert_eq!(after.unconfirmed_sat, 0);
    let history = history(state).unwrap();
    assert!(history.iter().any(|row| row.txid == sent.txid
        && row.net_sat == -i64::try_from(third.preview.total_sat).unwrap()));
}

// ── End to end: one wallet, two frontends ───────────────────────────────────────────────────

/// A regtest fixture plus the CLI's view of the same datadir: the `Config` that
/// `btcw --network regtest --datadir <dir> --rpc-url .. --rpc-cookie ..` resolves.
fn regtest_fixture(node: &TestNode) -> (Fixture, Config) {
    let rpc = node.rpc_config();
    let RpcAuth::Cookie(cookie) = &rpc.auth else {
        panic!("TestNode uses cookie auth");
    };
    let fx = with_env(
        tempfile::tempdir().unwrap(),
        vec![
            ("BTCW_NETWORK", "regtest".to_owned()),
            ("BTCW_RPC_URL", rpc.url.clone()),
            (
                "BTCW_RPC_COOKIE",
                PathBuf::from(cookie).display().to_string(),
            ),
        ],
    );
    let cli = Config::load_with(
        config::Overrides {
            network: Some("regtest".into()),
            datadir: Some(fx.dir.path().to_path_buf()),
            rpc_url: Some(rpc.url.clone()),
            rpc_cookie: Some(cookie.clone()),
            ..config::Overrides::default()
        },
        |_| None,
    )
    .unwrap();
    (fx, cli)
}

/// The app picks up a wallet the CLI created, verifies its backup, unlocks, sends; then the CLI
/// spends from the same wallet and each side sees the other's transactions. The CLI side runs
/// the same `btcw_core` calls `btcw` makes.
#[test]
fn desktop_and_cli_share_one_wallet_end_to_end() {
    if !TestNode::available() {
        eprintln!("skipping: no bitcoind (set BITCOIND_EXE to run this test)");
        return;
    }
    let node = TestNode::start().unwrap();
    let (fx, cli) = regtest_fixture(&node);
    let state = &fx.state;

    // `btcw create`: birthday from the node, phrase shown once, first receive address.
    let birthday = Node::connect(&cli.rpc, Network::Regtest)
        .unwrap()
        .tip_height()
        .unwrap();
    let CreatedWallet {
        mnemonic,
        unlocked: Unlocked { mut wallet, signer },
    } = api::create_wallet(&cli, WordCount::Words12, &secret(PASSWORD), Some(birthday)).unwrap();
    drop(signer);
    let receive = wallet.new_address().unwrap();
    drop(wallet);
    let phrase: Vec<String> = mnemonic
        .phrase()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    drop(mnemonic);

    // The app finds it: there, locked, backup not verified yet, never synced.
    let found = info(&fx);
    assert!(found.wallet_exists);
    assert!(!found.unlocked);
    assert_eq!(found.backup_verified, Some(false));
    assert_eq!(found.synced_height, Some(0));
    assert_eq!(list_addresses(state).unwrap(), vec![receive.clone()]);

    // Paid and mined; the app syncs from the CLI's birthday.
    let receive_address = receive
        .address
        .parse::<Address<NetworkUnchecked>>()
        .unwrap()
        .require_network(Network::Regtest)
        .unwrap();
    node.fund(&receive_address, Amount::from_sat(1_000_000))
        .unwrap();
    node.mine(1).unwrap();
    let report = sync(state, &mut |_| {}).unwrap();
    assert_eq!(
        report.blocks_scanned, 2,
        "the birthday block and the new one"
    );
    assert_eq!(balance(state).unwrap().confirmed_sat, 1_000_000);
    assert!(list_addresses(state).unwrap()[0].used);

    // Verify backup in the app, answering from the phrase the CLI showed; both frontends read
    // the same flag (the CLI's reminder stops too).
    let positions = backup_challenge(state, &secret(PASSWORD)).unwrap();
    assert_eq!(positions.len(), 3);
    let answers = positions
        .iter()
        .map(|&p| (p, phrase[p - 1].clone()))
        .collect();
    verify_backup(state, &secret(PASSWORD), answers).unwrap();
    assert_eq!(info(&fx).backup_verified, Some(true));
    assert_eq!(
        WalletService::read_backup_verified(&cli).unwrap(),
        Some(true)
    );

    // Locked: no payment can be prepared. The CLI's password unlocks the app.
    let to = node.faucet_address().unwrap().to_string();
    assert_eq!(code(prepare_send(state, &to, 100_000, Some(2.0))), "locked");
    assert_eq!(
        code(unlock(state, &secret("not the password"))),
        "wrong_password"
    );
    unlock(state, &secret(PASSWORD)).unwrap();
    assert!(info(&fx).unlocked);
    lock(state).unwrap();
    assert!(!info(&fx).unlocked);
    assert_eq!(code(prepare_send(state, &to, 100_000, Some(2.0))), "locked");
    unlock(state, &secret(PASSWORD)).unwrap();

    // Prepare → confirm. The wallet isn't held while the preview is on screen.
    let prepared = prepare_send(state, &to, 100_000, Some(2.0)).unwrap();
    assert_eq!(
        api::open_watch_only(&cli).unwrap().balance().confirmed_sat,
        1_000_000
    );
    let sent = confirm_send(state, &prepared.id).unwrap();
    let mempool = node.call("getrawmempool", &[]).unwrap();
    assert_eq!(mempool, json!([sent.txid]));
    assert!(matches!(
        tx_status(state, &sent.txid).unwrap(),
        Some(TxStatus::Unconfirmed { .. })
    ));
    // `btcw history` / `btcw status` see the app's payment at once, without a sync.
    let txid1 = Txid::from_str(&sent.txid).unwrap();
    let total1 = prepared.preview.total_sat;
    {
        let wallet = api::open_watch_only(&cli).unwrap();
        let row = wallet
            .history()
            .into_iter()
            .find(|row| row.txid == sent.txid)
            .unwrap();
        assert_eq!(row.net_sat, -i64::try_from(total1).unwrap());
        assert_eq!(row.fee_sat, Some(prepared.preview.fee_sat));
        assert!(matches!(
            tx::tx_status(&wallet, txid1),
            Some(TxStatus::Unconfirmed { .. })
        ));
    }
    node.mine(1).unwrap();
    let tip = node.tip_height().unwrap();
    let Some(TxStatus::Confirmed {
        height,
        confirmations,
        ..
    }) = tx_status(state, &sent.txid).unwrap()
    else {
        panic!("not confirmed after a block");
    };
    assert_eq!((height, confirmations), (tip, 1));

    // `btcw send` from the same wallet while the app stays unlocked. While the CLI holds the
    // wallet the app gets `wallet_in_use`; afterwards it sees the CLI's payment.
    let mut unlocked = api::unlock_wallet(&cli, &secret(PASSWORD)).unwrap();
    assert_eq!(code(balance(state)), "wallet_in_use");
    let cli_node = Node::connect(&cli.rpc, Network::Regtest).unwrap();
    cli_node.sync(&mut unlocked.wallet, &mut |_| {}).unwrap();
    let (psbt, preview) = tx::prepare_send(
        &mut unlocked.wallet,
        &cli_node,
        &to,
        50_000,
        Some(FeeRate::from_sat_per_vb_u32(2)),
    )
    .unwrap();
    let txid2 = tx::complete_send(&mut unlocked.wallet, &unlocked.signer, &cli_node, psbt).unwrap();
    drop(unlocked);
    let rows = history(state).unwrap();
    let row = rows
        .iter()
        .find(|row| row.txid == txid2.to_string())
        .unwrap();
    assert_eq!(row.net_sat, -i64::try_from(preview.total_sat).unwrap());
    assert!(matches!(row.status, TxStatus::Unconfirmed { .. }));
    let after = balance(state).unwrap();
    assert_eq!(after.confirmed_sat, 0);
    assert_eq!(
        after.unconfirmed_sat,
        1_000_000 - total1 - preview.total_sat
    );
    assert!(info(&fx).unlocked, "the CLI's use doesn't lock the app");
    node.mine(1).unwrap();
    assert!(matches!(
        tx_status(state, &txid2.to_string()).unwrap(),
        Some(TxStatus::Confirmed {
            confirmations: 1,
            ..
        })
    ));
}

/// `btcw send` spends the coin of a payment prepared in the app before "Send". Signing the stale
/// PSBT would RBF-replace the CLI's payment, so the app must refuse and ask for a new review.
#[test]
fn a_payment_prepared_in_the_app_is_refused_after_the_cli_spent_its_coin() {
    if !TestNode::available() {
        eprintln!("skipping: no bitcoind (set BITCOIND_EXE to run this test)");
        return;
    }
    let node = TestNode::start().unwrap();
    let (fx, cli) = regtest_fixture(&node);
    let state = &fx.state;
    create(&fx);
    let receive = new_address(state)
        .unwrap()
        .address
        .parse::<Address<NetworkUnchecked>>()
        .unwrap()
        .require_network(Network::Regtest)
        .unwrap();
    node.fund(&receive, Amount::from_sat(1_000_000)).unwrap();
    node.mine(1).unwrap();
    sync(state, &mut |_| {}).unwrap();

    let to = node.faucet_address().unwrap().to_string();
    let prepared = prepare_send(state, &to, 100_000, Some(5.0)).unwrap();

    // Meanwhile, `btcw send` (at a lower fee rate). There is one coin, so it spends that one.
    let mut unlocked = api::unlock_wallet(&cli, &secret(PASSWORD)).unwrap();
    let cli_node = Node::connect(&cli.rpc, Network::Regtest).unwrap();
    cli_node.sync(&mut unlocked.wallet, &mut |_| {}).unwrap();
    let (psbt, cli_preview) = tx::prepare_send(
        &mut unlocked.wallet,
        &cli_node,
        &to,
        50_000,
        Some(FeeRate::from_sat_per_vb_u32(2)),
    )
    .unwrap();
    let cli_txid = tx::complete_send(&mut unlocked.wallet, &unlocked.signer, &cli_node, psbt)
        .unwrap()
        .to_string();
    drop(unlocked);

    let refused = confirm_send(state, &prepared.id).unwrap_err();
    assert_eq!(refused.code, "tx_build", "{}", refused.message);
    assert!(refused.message.contains("review"), "{}", refused.message);
    // The CLI's payment is untouched, and the app shows it.
    assert_eq!(node.call("getrawmempool", &[]).unwrap(), json!([cli_txid]));
    sync(state, &mut |_| {}).unwrap();
    assert!(
        history(state)
            .unwrap()
            .iter()
            .any(|row| row.txid == cli_txid)
    );
    assert_eq!(
        balance(state).unwrap().unconfirmed_sat,
        1_000_000 - cli_preview.total_sat
    );
    // A new review builds on the wallet as it is now, and goes through.
    let again = prepare_send(state, &to, 100_000, Some(5.0)).unwrap();
    let sent = confirm_send(state, &again.id).unwrap();
    let mempool = node.call("getrawmempool", &[]).unwrap();
    let mempool = mempool.as_array().unwrap();
    assert_eq!(mempool.len(), 2);
    assert!(mempool.contains(&json!(sent.txid)) && mempool.contains(&json!(cli_txid)));
}

/// Against a real node: pay a contact by name, label the payment, speed it up, and check node
/// and wallet hold only the replacement, with the label. Then a prepared bump goes stale when
/// the payment confirms, and `confirm_send` refuses it without broadcasting.
#[test]
fn contacts_labels_and_speed_up_against_a_regtest_node() {
    if !TestNode::available() {
        eprintln!("skipping: no bitcoind (set BITCOIND_EXE to run this test)");
        return;
    }
    let node = TestNode::start().unwrap();
    let (fx, _cli) = regtest_fixture(&node);
    let state = &fx.state;
    create(&fx);
    let receive = new_address(state)
        .unwrap()
        .address
        .parse::<Address<NetworkUnchecked>>()
        .unwrap()
        .require_network(Network::Regtest)
        .unwrap();
    node.fund(&receive, Amount::from_sat(1_000_000)).unwrap();
    node.mine(1).unwrap();
    sync(state, &mut |_| {}).unwrap();

    // Pay Alice by name at 2 sat/vB: the preview has her full address and her name.
    let alice = node.faucet_address().unwrap().to_string();
    add_contact(state, "Alice", &alice, Some("rent")).unwrap();
    let prepared = prepare_send(state, "alice", 100_000, Some(2.0)).unwrap();
    assert_eq!(prepared.preview.to, alice);
    assert_eq!(prepared.preview.contact.as_deref(), Some("Alice"));
    assert_eq!(prepared.preview.replaces, None);
    assert_eq!(
        prepared.preview.fee_sat, 281,
        "PLAN-v2 §2: 141 vB at 2 sat/vB"
    );
    let original = confirm_send(state, &prepared.id).unwrap().txid;
    assert_eq!(node.call("getrawmempool", &[]).unwrap(), json!([original]));

    assert_eq!(
        set_label(state, &original, "  rent October ").unwrap(),
        "rent October"
    );
    let row = |txid: &str| {
        history(state)
            .unwrap()
            .into_iter()
            .find(|row| row.txid == txid)
    };
    assert_eq!(
        row(&original).unwrap().label.as_deref(),
        Some("rent October")
    );

    // The minimum: BDK's old rate + 1 sat/vB, raised to BIP125 rule 4 (751 sat/kWU), rounded up.
    let min = min_fee_bump_rate(state, &original).unwrap();
    assert_eq!(min, 3.01);
    for low in [2.0, 3.0] {
        let refused = prepare_fee_bump(state, &original, Some(low)).unwrap_err();
        assert_eq!(refused.code, "tx_build", "{low}: {}", refused.message);
        assert!(
            refused.message.contains("at least 3.01 sat/vB"),
            "{}",
            refused.message
        );
    }
    let at_min = prepare_fee_bump(state, &original, Some(min)).unwrap();
    assert_eq!(at_min.preview.replaces.as_deref(), Some(original.as_str()));
    assert!(
        at_min.preview.fee_sat >= 281 + at_min.preview.vsize,
        "rule 4"
    );

    // A new review takes the same single slot: the first id is dead.
    let bump = prepare_fee_bump(state, &original, Some(5.0)).unwrap();
    assert_eq!(code(confirm_send(state, &at_min.id)), "tx_build");
    let preview = &bump.preview;
    assert_eq!(preview.replaces.as_deref(), Some(original.as_str()));
    assert_eq!(preview.to, alice);
    assert_eq!(preview.amount_sat, 100_000);
    assert_eq!(preview.fee_sat, 703, "PLAN-v2 §2: 141 vB at 5 sat/vB");
    assert_eq!(
        preview.change_sat,
        prepared.preview.change_sat.map(|c| c - 422)
    );

    // Send: `check_prepared_bump` accepts it (the plain check would refuse a bump).
    let replacement = confirm_send(state, &bump.id).unwrap().txid;
    assert_ne!(replacement, original);
    assert_eq!(code(confirm_send(state, &bump.id)), "tx_build");
    assert_eq!(
        node.call("getrawmempool", &[]).unwrap(),
        json!([replacement])
    );
    let entry = node.call("getmempoolentry", &[json!(replacement)]).unwrap();
    assert_eq!(entry["fees"]["base"], json!(0.00000703));

    // The wallet has only the replacement, with the label, before and after a sync.
    for synced in [false, true] {
        if synced {
            sync(state, &mut |_| {}).unwrap();
        }
        assert!(row(&original).is_none(), "synced: {synced}");
        assert_eq!(tx_status(state, &original).unwrap(), None);
        let new_row = row(&replacement).unwrap();
        assert_eq!(new_row.label.as_deref(), Some("rent October"));
        assert_eq!(new_row.net_sat, -100_703);
        assert_eq!(new_row.fee_sat, Some(703));
        assert_eq!(balance(state).unwrap().total_sat, 1_000_000 - 100_703);
    }
    assert_eq!(code(min_fee_bump_rate(state, &original)), "tx_build");

    // Labels can be cleared; clearing twice is fine.
    clear_label(state, &replacement).unwrap();
    clear_label(state, &replacement).unwrap();
    assert_eq!(row(&replacement).unwrap().label, None);

    // A bump prepared (a cancelled one first), then the payment confirms before "Send".
    let cancelled = prepare_fee_bump(state, &replacement, None).unwrap();
    cancel_send(state, &cancelled.id).unwrap();
    assert_eq!(code(confirm_send(state, &cancelled.id)), "tx_build");
    let late = prepare_fee_bump(state, &replacement, None).unwrap();
    assert!(late.preview.fee_rate_sat_vb >= min_fee_bump_rate(state, &replacement).unwrap() - 0.01);
    node.mine(1).unwrap();
    let refused = confirm_send(state, &late.id).unwrap_err();
    assert_eq!(refused.code, "tx_build", "{}", refused.message);
    assert!(
        refused.message.contains("already confirmed"),
        "{}",
        refused.message
    );
    assert_eq!(node.call("getrawmempool", &[]).unwrap(), json!([]));
    assert_eq!(code(confirm_send(state, &late.id)), "tx_build", "used up");
    assert!(matches!(
        row(&replacement).unwrap().status,
        TxStatus::Confirmed {
            confirmations: 1,
            ..
        }
    ));
    let after = balance(state).unwrap();
    assert_eq!(after.confirmed_sat, 1_000_000 - 100_703);
    assert_eq!(after.unconfirmed_sat, 0);
    assert_eq!(
        code(prepare_fee_bump(state, &replacement, None)),
        "tx_build"
    );
}
