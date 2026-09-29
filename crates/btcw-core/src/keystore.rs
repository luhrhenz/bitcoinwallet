//! The mnemonic, encrypted at rest.
//!
//! OWNER: Agent A (Phase 1). Contract: PLAN §5.10.
//!
//! BDK persists only *public* descriptors, so the seed has to be stored separately.
//! File format (JSON, mode 0600, written atomically via temp file + rename):
//! ```json
//! { "version": 1, "network": "testnet4",
//!   "kdf": { "alg": "argon2id", "m_kib": 65536, "t": 3, "p": 1, "salt": "<b64 16B>" },
//!   "cipher": { "alg": "xchacha20poly1305", "nonce": "<b64 24B>", "ciphertext": "<b64>" } }
//! ```
//! AAD = `"btcw-keystore-v1|" + network`, so a file can't be swapped between networks.
//! A wrong password or a tampered file fails the AEAD tag and returns `WrongPassword`.
#![allow(unused_variables, dead_code)] // remove once implemented

use std::path::Path;

use secrecy::SecretString;

use crate::bitcoin::Network;
use crate::error::Result;
use crate::keys::Mnemonic;

pub const KEYSTORE_VERSION: u32 = 1;

/// Encrypt and write the mnemonic. Fails with `WalletExists` if `path` already exists.
pub fn save(
    path: &Path,
    mnemonic: &Mnemonic,
    network: Network,
    password: &SecretString,
) -> Result<()> {
    todo!("Agent A")
}

/// Read and decrypt the mnemonic. `WalletNotFound` if missing, `NetworkMismatch` if the
/// file belongs to another network, `WrongPassword` if decryption fails.
pub fn load(path: &Path, network: Network, password: &SecretString) -> Result<Mnemonic> {
    todo!("Agent A")
}
