//! BIP39 mnemonics → BIP32 master key → BIP84 (native SegWit) descriptors.
//!
//! OWNER: Agent A (Phase 1). Contract: PLAN §4, §5.1–5.3.
//!
//! ```text
//! mnemonic + passphrase ──PBKDF2──▶ seed ──BIP32──▶ master xprv
//! master / 84' / coin' / account'  ──▶ account xprv
//! external: wpkh([fingerprint/84'/coin'/account']xpub/0/*)   receive   (public, stored by BDK)
//! internal: wpkh([fingerprint/84'/coin'/account']xpub/1/*)   change
//! Signer:   master xprv, kept in memory only while unlocked
//! ```
//! `coin` comes from [`crate::config::coin_type`] (0 mainnet, 1 test networks).
//! The key origin `[fingerprint/path]` must be included so PSBTs carry BIP32 derivation info,
//! which is how [`Signer`] finds the right child key.

use std::fmt;

use bdk_wallet::miniscript::descriptor::{
    Descriptor, DescriptorPublicKey, DescriptorXKey, Wildcard,
};
use zeroize::Zeroizing;

use crate::bitcoin::bip32::{ChildNumber, DerivationPath, Fingerprint, Xpriv, Xpub};
use crate::bitcoin::psbt::SigningKeys;
use crate::bitcoin::secp256k1::Secp256k1;
use crate::bitcoin::{self, Network};
use crate::error::{Result, WalletError};

/// BIP84 purpose field: native SegWit v0 single-key (P2WPKH) accounts.
const BIP84_PURPOSE: u32 = 84;
/// Last path step before the address index: 0 = receive (external), 1 = change (internal).
const EXTERNAL_CHAIN: u32 = 0;
const INTERNAL_CHAIN: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WordCount {
    #[default]
    Words12,
    Words24,
}

impl WordCount {
    /// BIP39: 12 words encode 128 bits of entropy, 24 words encode 256 bits.
    fn entropy_bytes(self) -> usize {
        match self {
            Self::Words12 => 16,
            Self::Words24 => 32,
        }
    }
}

/// A validated BIP39 mnemonic (English wordlist). Wiped from memory on drop.
/// `Debug` is redacted; the only way to read the words is [`Mnemonic::phrase`].
pub struct Mnemonic(pub(crate) bip39::Mnemonic);

impl Mnemonic {
    /// The space-separated words. Callers must not log or persist this unencrypted.
    pub fn phrase(&self) -> Zeroizing<String> {
        Zeroizing::new(self.0.to_string())
    }

    pub fn word_count(&self) -> usize {
        self.0.word_count()
    }
}

impl fmt::Debug for Mnemonic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mnemonic(<{} words redacted>)", self.word_count())
    }
}

/// Generate a fresh mnemonic from the OS CSPRNG.
pub fn generate_mnemonic(words: WordCount) -> Result<Mnemonic> {
    // Entropy comes straight from the OS (getrandom) instead of a userspace PRNG, and the
    // buffer is wiped once the words have been computed from it.
    let mut entropy = Zeroizing::new([0u8; 32]);
    let entropy = &mut entropy[..words.entropy_bytes()];
    getrandom::fill(entropy).map_err(|e| {
        WalletError::Io(std::io::Error::other(format!(
            "OS random number generator failed: {e}"
        )))
    })?;
    // Only fails for entropy lengths other than 128..=256 bits in 32-bit steps, which
    // `WordCount` rules out; the message is generic because entropy is secret anyway.
    let mnemonic = bip39::Mnemonic::from_entropy(entropy)
        .map_err(|_| WalletError::InvalidMnemonic("could not encode entropy".into()))?;
    Ok(Mnemonic(mnemonic))
}

/// Parse and validate a user-entered phrase: normalise whitespace and case, then check the
/// wordlist and checksum. Errors map to `WalletError::InvalidMnemonic` *without echoing the words*.
pub fn parse_mnemonic(phrase: &str) -> Result<Mnemonic> {
    let normalized = normalize_phrase(phrase);
    let mnemonic = bip39::Mnemonic::parse_in_normalized(bip39::Language::English, &normalized)
        .map_err(|e| WalletError::InvalidMnemonic(describe_bip39_error(e)))?;
    Ok(Mnemonic(mnemonic))
}

/// Trim, collapse any run of whitespace to one space and lowercase, into a buffer that is
/// wiped on drop.
///
/// ASCII lowercasing is enough: every word in the English list is ASCII, so a word with any
/// non-ASCII letter is rejected by the wordlist check anyway. It also means the output is never
/// longer than the input, so the pre-sized buffer never reallocates and leaves no unwiped copy.
fn normalize_phrase(phrase: &str) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(phrase.len()));
    for word in phrase.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().map(|c| c.to_ascii_lowercase()));
    }
    out
}

/// A user-facing message that names positions and counts, never the words themselves.
fn describe_bip39_error(e: bip39::Error) -> String {
    match e {
        bip39::Error::BadWordCount(n) => {
            format!("expected 12, 15, 18, 21 or 24 words, got {n}")
        }
        // bip39 reports a 0-based index; people count words from 1.
        bip39::Error::UnknownWord(i) => {
            format!("word {} is not in the BIP39 English word list", i + 1)
        }
        bip39::Error::InvalidChecksum => {
            "checksum mismatch (a word is probably mistyped or out of order)".into()
        }
        // Entropy-length and language-ambiguity errors can't come from an English-only parse.
        _ => "not a valid BIP39 phrase".into(),
    }
}

/// *Public* external + internal descriptors for one BIP84 account, with key origin, e.g.
/// `wpkh([73c5da0a/84'/1'/0']tpubDC.../0/*)`. Safe to store and display: they reveal
/// addresses and balances, never spending keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptors {
    external: String,
    internal: String,
}

impl Descriptors {
    /// Wrap already-built descriptor strings (used by tests with fixed descriptors).
    pub fn from_strings(external: impl Into<String>, internal: impl Into<String>) -> Self {
        Self {
            external: external.into(),
            internal: internal.into(),
        }
    }

    pub fn external(&self) -> &str {
        &self.external
    }

    pub fn internal(&self) -> &str {
        &self.internal
    }
}

/// Holds the BIP32 *master* private key and signs PSBTs with rust-bitcoin's `Psbt::sign`.
/// (BDK 3.2 deprecated `Wallet::sign`; the wallet itself only ever holds public descriptors.)
///
/// `Psbt::sign` matches each input's `bip32_derivation` (master fingerprint + full path, which
/// BDK fills in from the descriptor's key origin) and derives the child key from this master.
/// The secret key is wiped on drop (`non_secure_erase`); `Debug` is redacted.
pub struct Signer {
    master: bitcoin::bip32::Xpriv,
}

impl Signer {
    /// Sign every input this key can sign. Returns how many inputs were signed.
    /// Finalizing (building the witness) is done afterwards by `tx::sign_psbt` via BDK.
    pub fn sign_psbt(&self, psbt: &mut bitcoin::Psbt) -> Result<usize> {
        let secp = Secp256k1::new();
        match psbt.sign(&self.master, &secp) {
            // `Psbt::sign` records every input it looked at, with an empty key list when none of
            // the input's derivation paths belong to this master, so count non-empty entries.
            Ok(used) => Ok(used.values().filter(|keys| signed_any(keys)).count()),
            // `SignError` only describes the input (missing UTXO, bad sighash type, ...), never
            // key material, so it is safe to surface.
            Err((_, errors)) => {
                let detail = errors
                    .iter()
                    .map(|(input, e)| format!("input {input}: {e}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                Err(WalletError::Sign(detail))
            }
        }
    }

    /// Master key fingerprint (first 4 bytes of hash160 of the master pubkey). Not secret.
    pub fn fingerprint(&self) -> bitcoin::bip32::Fingerprint {
        self.master.fingerprint(&Secp256k1::signing_only())
    }
}

fn signed_any(keys: &SigningKeys) -> bool {
    match keys {
        SigningKeys::Ecdsa(pks) => !pks.is_empty(),
        SigningKeys::Schnorr(pks) => !pks.is_empty(),
    }
}

impl Drop for Signer {
    fn drop(&mut self) {
        self.master.private_key.non_secure_erase();
    }
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Signer(<redacted>)")
    }
}

/// BIP84 account `account` on `network`: public descriptors for the wallet + a signer.
/// `passphrase` is the optional BIP39 "25th word" (`""` for none).
pub fn derive_account(
    mnemonic: &Mnemonic,
    passphrase: &str,
    network: Network,
    account: u32,
) -> Result<(Descriptors, Signer)> {
    let secp = Secp256k1::new();

    // BIP39: PBKDF2-HMAC-SHA512(words, "mnemonic" + passphrase), 2048 rounds → 64-byte seed.
    let seed = Zeroizing::new(mnemonic.0.to_seed(passphrase));
    // Hand the master to `Signer` straight away so its `Drop` wipes it on every error path below.
    let signer = Signer {
        master: Xpriv::new_master(network, seed.as_slice()).map_err(derivation_error)?,
    };
    let master_fingerprint = signer.master.fingerprint(&secp);

    // m/84'/coin'/account', every step hardened (BIP44 convention). With non-hardened steps a
    // single leaked child private key plus the parent xpub reveals the parent private key;
    // hardening stops such a leak at the account, so the master and other accounts stay safe.
    let account_path = DerivationPath::from(vec![
        hardened(BIP84_PURPOSE)?,
        hardened(crate::config::coin_type(network))?,
        hardened(account)?,
    ]);
    let mut account_xprv = signer
        .master
        .derive_priv(&secp, &account_path)
        .map_err(derivation_error)?;
    let account_xpub = Xpub::from_priv(&secp, &account_xprv);
    // Best effort: `Xpriv` is `Copy`, so this only wipes our copy of the account key.
    account_xprv.private_key.non_secure_erase();

    let origin = (master_fingerprint, account_path);
    let descriptors = Descriptors {
        external: wpkh_descriptor(&origin, account_xpub, EXTERNAL_CHAIN)?,
        internal: wpkh_descriptor(&origin, account_xpub, INTERNAL_CHAIN)?,
    };
    Ok((descriptors, signer))
}

/// `wpkh([fp/84'/coin'/account']xpub/<chain>/*)#checksum`, built from typed miniscript keys so
/// the origin syntax and the checksum come from the library rather than string formatting.
fn wpkh_descriptor(
    origin: &(Fingerprint, DerivationPath),
    account_xpub: Xpub,
    chain: u32,
) -> Result<String> {
    let key = DescriptorPublicKey::XPub(DescriptorXKey {
        origin: Some(origin.clone()),
        xkey: account_xpub,
        derivation_path: DerivationPath::from(vec![ChildNumber::Normal { index: chain }]),
        wildcard: Wildcard::Unhardened,
    });
    let descriptor = Descriptor::new_wpkh(key)
        .map_err(|e| WalletError::Config(format!("could not build descriptor: {e}")))?;
    // `Display` for a descriptor appends `#checksum` (BIP380).
    Ok(descriptor.to_string())
}

fn hardened(index: u32) -> Result<ChildNumber> {
    ChildNumber::from_hardened_idx(index).map_err(|_| {
        WalletError::Config(format!(
            "derivation index {index} is out of range (must be below 2^31)"
        ))
    })
}

/// BIP32 errors describe the failing step (invalid child number, secp256k1 range check),
/// never key bytes. Reaching this for a valid seed has a probability of about 2^-127.
fn derivation_error(e: bitcoin::bip32::Error) -> WalletError {
    WalletError::Config(format!("key derivation failed: {e}"))
}
