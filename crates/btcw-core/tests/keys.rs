//! BIP39 mnemonics, BIP84 derivation and PSBT signing (offline).

use std::error::Error;
use std::str::FromStr;

use btcw_core::WalletError;
use btcw_core::bdk_wallet::miniscript::{Descriptor, DescriptorPublicKey};
use btcw_core::bdk_wallet::{KeychainKind, SignOptions, Wallet};
use btcw_core::bitcoin::absolute::LockTime;
use btcw_core::bitcoin::hashes::Hash;
use btcw_core::bitcoin::transaction::Version;
use btcw_core::bitcoin::{Amount, FeeRate, Network, OutPoint, Transaction, TxIn, TxOut, Txid};
use btcw_core::keys::{self, Descriptors, Mnemonic, WordCount};

type TestResult = Result<(), Box<dyn Error>>;

/// The mnemonic used by the BIP84 test vectors.
const BIP84_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

fn abandon() -> Result<Mnemonic, Box<dyn Error>> {
    Ok(keys::parse_mnemonic(BIP84_MNEMONIC)?)
}

fn wallet_for(descriptors: &Descriptors, network: Network) -> Result<Wallet, Box<dyn Error>> {
    Ok(Wallet::create(
        descriptors.external().to_string(),
        descriptors.internal().to_string(),
    )
    .network(network)
    .create_wallet_no_persist()?)
}

fn address(wallet: &Wallet, keychain: KeychainKind, index: u32) -> String {
    wallet.peek_address(keychain, index).address.to_string()
}

/// The error text shown to the user, which must never contain any of the entered words.
fn invalid_mnemonic_message(phrase: &str) -> Result<String, Box<dyn Error>> {
    match keys::parse_mnemonic(phrase) {
        Err(e @ WalletError::InvalidMnemonic(_)) => Ok(e.to_string()),
        Err(other) => Err(format!("expected InvalidMnemonic, got {other}").into()),
        Ok(_) => Err("invalid phrase was accepted".into()),
    }
}

// ── BIP84 test vector ───────────────────────────────────────────────────────────────────────

#[test]
fn bip84_mainnet_vector() -> TestResult {
    // https://github.com/bitcoin/bips/blob/master/bip-0084.mediawiki#test-vectors
    let (descriptors, signer) = keys::derive_account(&abandon()?, "", Network::Bitcoin, 0)?;

    assert_eq!(signer.fingerprint().to_string(), "73c5da0a");
    // The BIP's account key is zpub6rFR7y4Q2AijBEqTUquhVz398htDFrtymD9xYYfG1m4wAcvPhXNfE3EfH1r1ADqtfSdVCToUG868RvUUkgDKf31mGDtKsAYz2oz2AGutZYs;
    // this is the same key with the standard xpub version bytes that descriptors use.
    let account = "[73c5da0a/84'/0'/0']xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V";
    assert!(
        descriptors
            .external()
            .starts_with(&format!("wpkh({account}/0/*)#")),
        "{}",
        descriptors.external()
    );
    assert!(
        descriptors
            .internal()
            .starts_with(&format!("wpkh({account}/1/*)#")),
        "{}",
        descriptors.internal()
    );

    let wallet = wallet_for(&descriptors, Network::Bitcoin)?;
    assert_eq!(
        address(&wallet, KeychainKind::External, 0),
        "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu"
    );
    assert_eq!(
        address(&wallet, KeychainKind::External, 1),
        "bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g"
    );
    assert_eq!(
        address(&wallet, KeychainKind::Internal, 0),
        "bc1q8c6fshw2dlwun7ekn9qwf37cu2rn755upcp6el"
    );
    Ok(())
}

#[test]
fn descriptors_are_public_with_valid_checksum() -> TestResult {
    let (descriptors, _) = keys::derive_account(&abandon()?, "", Network::Testnet4, 0)?;
    for d in [descriptors.external(), descriptors.internal()] {
        assert!(!d.contains("prv"), "descriptor holds a private key");
        let (_, checksum) = d.split_once('#').ok_or("no checksum")?;
        assert_eq!(checksum.len(), 8);
        // miniscript verifies a present checksum while parsing.
        let parsed = Descriptor::<DescriptorPublicKey>::from_str(d)?;
        assert_eq!(parsed.to_string(), d);
        let mut corrupted = d.to_string();
        corrupted.pop();
        corrupted.push(if d.ends_with('q') { 'p' } else { 'q' });
        assert!(Descriptor::<DescriptorPublicKey>::from_str(&corrupted).is_err());
    }
    Ok(())
}

#[test]
fn test_networks_use_coin_type_1_and_tpub() -> TestResult {
    let mnemonic = abandon()?;
    for (network, hrp) in [
        (Network::Testnet4, "tb1q"),
        (Network::Signet, "tb1q"),
        (Network::Regtest, "bcrt1q"),
    ] {
        let (descriptors, _) = keys::derive_account(&mnemonic, "", network, 0)?;
        assert!(
            descriptors
                .external()
                .starts_with("wpkh([73c5da0a/84'/1'/0']tpub"),
            "{network}: {}",
            descriptors.external()
        );
        let wallet = wallet_for(&descriptors, network)?;
        for keychain in [KeychainKind::External, KeychainKind::Internal] {
            let addr = address(&wallet, keychain, 0);
            assert!(addr.starts_with(hrp), "{network}: {addr}");
        }
    }

    // Widely published first testnet BIP84 address for this mnemonic (m/84'/1'/0'/0/0).
    let (descriptors, _) = keys::derive_account(&mnemonic, "", Network::Testnet4, 0)?;
    let wallet = wallet_for(&descriptors, Network::Testnet4)?;
    assert_eq!(
        address(&wallet, KeychainKind::External, 0),
        "tb1q6rz28mcfaxtmd6v789l9rrlrusdprr9pqcpvkl"
    );
    Ok(())
}

#[test]
fn account_index_is_part_of_the_path() -> TestResult {
    let mnemonic = abandon()?;
    let (account0, _) = keys::derive_account(&mnemonic, "", Network::Regtest, 0)?;
    let (account1, _) = keys::derive_account(&mnemonic, "", Network::Regtest, 1)?;
    assert!(account1.external().contains("/84'/1'/1']"));
    assert_ne!(account0, account1);
    assert!(matches!(
        keys::derive_account(&mnemonic, "", Network::Regtest, 1 << 31),
        Err(WalletError::Config(_))
    ));
    Ok(())
}

#[test]
fn passphrase_changes_the_wallet() -> TestResult {
    let mnemonic = abandon()?;
    let (plain, plain_signer) = keys::derive_account(&mnemonic, "", Network::Regtest, 0)?;
    let (with_pass, pass_signer) = keys::derive_account(&mnemonic, "TREZOR", Network::Regtest, 0)?;
    assert_ne!(plain, with_pass);
    assert_ne!(plain_signer.fingerprint(), pass_signer.fingerprint());
    // Derivation is deterministic.
    let (again, _) = keys::derive_account(&mnemonic, "TREZOR", Network::Regtest, 0)?;
    assert_eq!(with_pass, again);
    Ok(())
}

// ── Mnemonic generation and parsing ─────────────────────────────────────────────────────────

#[test]
fn generates_12_and_24_words_that_round_trip() -> TestResult {
    for (count, words) in [(WordCount::Words12, 12), (WordCount::Words24, 24)] {
        let mnemonic = keys::generate_mnemonic(count)?;
        assert_eq!(mnemonic.word_count(), words);
        let phrase = mnemonic.phrase();
        assert_eq!(phrase.split(' ').count(), words);
        let reparsed = keys::parse_mnemonic(&phrase)?;
        assert_eq!(*reparsed.phrase(), *phrase);
    }
    assert_eq!(WordCount::default(), WordCount::Words12);
    Ok(())
}

#[test]
fn generations_differ() -> TestResult {
    let a = keys::generate_mnemonic(WordCount::Words12)?;
    let b = keys::generate_mnemonic(WordCount::Words12)?;
    assert_ne!(*a.phrase(), *b.phrase());
    Ok(())
}

#[test]
fn accepts_messy_whitespace_and_case() -> TestResult {
    let messy = "  ABANDON\tabandon  Abandon\nabandon abandon\r\nabandon abandon abandon \
                 abandon abandon    abandon AbOuT \n";
    let mnemonic = keys::parse_mnemonic(messy)?;
    assert_eq!(*mnemonic.phrase(), BIP84_MNEMONIC);
    Ok(())
}

#[test]
fn rejects_unknown_word_without_echoing_it() -> TestResult {
    let phrase = "abandon abandon abandon abandon blockchain abandon abandon abandon abandon abandon abandon about";
    let msg = invalid_mnemonic_message(phrase)?;
    assert!(msg.contains("word 5"), "{msg}");
    for word in ["blockchain", "abandon", "about"] {
        assert!(!msg.contains(word), "error echoes `{word}`: {msg}");
    }
    Ok(())
}

#[test]
fn rejects_bad_checksum_without_echoing_words() -> TestResult {
    // Twelve valid words whose last word doesn't carry the right checksum bits.
    let msg = invalid_mnemonic_message(&["abandon"; 12].join(" "))?;
    assert!(msg.contains("checksum"), "{msg}");
    assert!(!msg.contains("abandon"), "{msg}");
    Ok(())
}

#[test]
fn rejects_wrong_word_count() -> TestResult {
    let msg = invalid_mnemonic_message(&["abandon"; 11].join(" "))?;
    assert!(msg.contains("got 11"), "{msg}");
    assert!(!msg.contains("abandon"), "{msg}");
    assert!(invalid_mnemonic_message("   ")?.contains("got 0"));
    Ok(())
}

// ── Signer ──────────────────────────────────────────────────────────────────────────────────

#[test]
fn signer_signs_a_wallet_psbt_and_a_stranger_signs_nothing() -> TestResult {
    let network = Network::Regtest;
    let (descriptors, signer) = keys::derive_account(&abandon()?, "", network, 0)?;
    let stranger_mnemonic = keys::generate_mnemonic(WordCount::Words12)?;
    let (stranger_descriptors, stranger) =
        keys::derive_account(&stranger_mnemonic, "", network, 0)?;

    // Fund the wallet with a made-up unconfirmed transaction (BDK doesn't need its parent).
    let mut wallet = wallet_for(&descriptors, network)?;
    let receive = wallet.reveal_next_address(KeychainKind::External).address;
    let funding = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(Txid::from_byte_array([7; 32]), 0),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(100_000),
            script_pubkey: receive.script_pubkey(),
        }],
    };
    wallet.apply_unconfirmed_txs([(funding, 0)]);
    assert_eq!(wallet.balance().total(), Amount::from_sat(100_000));

    let pay_to = wallet_for(&stranger_descriptors, network)?
        .peek_address(KeychainKind::External, 0)
        .address;
    let mut builder = wallet.build_tx();
    builder
        .add_recipient(pay_to.script_pubkey(), Amount::from_sat(40_000))
        .fee_rate(FeeRate::from_sat_per_vb_u32(2));
    let mut psbt = builder.finish()?;

    // BDK wrote our key origin into the input; that's what lets the signer find the key.
    let origins: Vec<_> = psbt.inputs[0].bip32_derivation.values().collect();
    assert!(!origins.is_empty());
    assert!(origins.iter().all(|(fp, _)| *fp == signer.fingerprint()));

    assert_eq!(stranger.sign_psbt(&mut psbt)?, 0);
    assert!(psbt.inputs.iter().all(|i| i.partial_sigs.is_empty()));

    let signed = signer.sign_psbt(&mut psbt)?;
    assert!(signed >= 1);
    assert_eq!(signed, psbt.inputs.len());
    assert!(wallet.finalize_psbt(&mut psbt, SignOptions::default())?);
    let tx = psbt.extract_tx()?;
    assert!(tx.input.iter().all(|i| !i.witness.is_empty()));
    Ok(())
}

#[test]
fn sign_errors_are_reported_without_key_material() -> TestResult {
    let network = Network::Regtest;
    let (descriptors, signer) = keys::derive_account(&abandon()?, "", network, 0)?;
    let mut wallet = wallet_for(&descriptors, network)?;
    let receive = wallet.reveal_next_address(KeychainKind::External).address;
    let funding = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(Txid::from_byte_array([9; 32]), 0),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: receive.script_pubkey(),
        }],
    };
    wallet.apply_unconfirmed_txs([(funding, 0)]);
    let mut builder = wallet.build_tx();
    builder.add_recipient(receive.script_pubkey(), Amount::from_sat(10_000));
    let mut psbt = builder.finish()?;

    // Without the spent output the sighash can't be computed.
    psbt.inputs[0].witness_utxo = None;
    psbt.inputs[0].non_witness_utxo = None;
    match signer.sign_psbt(&mut psbt) {
        Err(e @ WalletError::Sign(_)) => {
            let msg = e.to_string();
            assert!(msg.contains("input 0"), "{msg}");
            assert!(!msg.contains("prv"), "{msg}");
        }
        other => return Err(format!("expected a signing error, got {other:?}").into()),
    }
    Ok(())
}

// ── Secrets stay out of Debug output ────────────────────────────────────────────────────────

#[test]
fn debug_output_is_redacted() -> TestResult {
    let mnemonic = abandon()?;
    let (_, signer) = keys::derive_account(&mnemonic, "", Network::Regtest, 0)?;

    let mnemonic_debug = format!("{mnemonic:?}");
    assert_eq!(mnemonic_debug, "Mnemonic(<12 words redacted>)");
    assert!(!mnemonic_debug.contains("abandon") && !mnemonic_debug.contains("about"));

    let signer_debug = format!("{signer:?} {signer:#?}");
    assert!(!signer_debug.contains("prv"), "{signer_debug}");
    assert!(signer_debug.contains("redacted"));
    Ok(())
}
