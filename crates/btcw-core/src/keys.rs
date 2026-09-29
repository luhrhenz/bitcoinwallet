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
#![allow(unused_variables, dead_code)] // remove once implemented

use std::fmt;

use zeroize::Zeroizing;

use crate::bitcoin::{self, Network};
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WordCount {
    #[default]
    Words12,
    Words24,
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
    todo!("Agent A: 128 bits of entropy for 12 words, 256 for 24")
}

/// Parse and validate a user-entered phrase: normalise whitespace and case, then check the
/// wordlist and checksum. Errors map to `WalletError::InvalidMnemonic` *without echoing the words*.
pub fn parse_mnemonic(phrase: &str) -> Result<Mnemonic> {
    todo!("Agent A")
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
        todo!(
            "Agent A: psbt.sign(&self.master, &Secp256k1::new()); map SignError without leaking keys"
        )
    }

    /// Master key fingerprint (first 4 bytes of hash160 of the master pubkey). Not secret.
    pub fn fingerprint(&self) -> bitcoin::bip32::Fingerprint {
        todo!("Agent A")
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
    todo!("Agent A")
}
