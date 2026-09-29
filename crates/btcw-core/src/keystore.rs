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

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::bitcoin::Network;
use crate::error::{Result, WalletError};
use crate::keys::{self, Mnemonic};

pub const KEYSTORE_VERSION: u32 = 1;

const KDF_ALG: &str = "argon2id";
const CIPHER_ALG: &str = "xchacha20poly1305";
/// Prefix of the AEAD associated data; the network name is appended.
const AAD_PREFIX: &str = "btcw-keystore-v1|";

/// Argon2id cost for new keystores: 64 MiB, 3 passes, 1 lane (the RFC 9106 "second
/// recommended" profile). Roughly half a second per unlock on a laptop.
const DEFAULT_M_KIB: u32 = 64 * 1024;
const DEFAULT_T: u32 = 3;
const DEFAULT_P: u32 = 1;

/// Upper bounds for costs read from a file. The file is untrusted input: without these a
/// crafted keystore could make `load` allocate terabytes or spin for hours before the
/// password is even checked.
const MAX_M_KIB: u32 = 1024 * 1024; // 1 GiB
const MAX_T: u32 = 10;
const MAX_P: u32 = 16;

const KEY_LEN: usize = 32;
const SALT_LEN: usize = 16;
/// XChaCha20's 192-bit nonce is large enough to pick at random with no risk of reuse.
const NONCE_LEN: usize = 24;

/// A real keystore is well under 1 KiB; refuse to read anything absurdly large.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// On-disk layout. Unknown fields are rejected so a typo or a newer format isn't half-read.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeystoreFile {
    version: u32,
    network: String,
    kdf: KdfSection,
    cipher: CipherSection,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KdfSection {
    alg: String,
    m_kib: u32,
    t: u32,
    p: u32,
    salt: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CipherSection {
    alg: String,
    nonce: String,
    ciphertext: String,
}

/// Read before the full parse so a future version reports "unsupported version" rather than
/// a confusing "unknown field" error.
#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

/// Encrypt and write the mnemonic. Fails with `WalletExists` if `path` already exists.
pub fn save(
    path: &Path,
    mnemonic: &Mnemonic,
    network: Network,
    password: &SecretString,
) -> Result<()> {
    let password = password.expose_secret();
    if password.is_empty() {
        return Err(WalletError::Keystore("password must not be empty".into()));
    }
    // Checked up front so an existing wallet is reported before spending time on Argon2;
    // `write_new_file` checks again right before publishing the file.
    ensure_absent(path)?;

    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    fill_random(&mut salt)?;
    fill_random(&mut nonce)?;

    let key = derive_key(
        password.as_bytes(),
        &salt,
        DEFAULT_M_KIB,
        DEFAULT_T,
        DEFAULT_P,
    )?;
    let phrase = mnemonic.phrase();
    let aad = aad(network);
    let ciphertext = cipher(&key)?
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: phrase.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| WalletError::Keystore("encryption failed".into()))?;

    let file = KeystoreFile {
        version: KEYSTORE_VERSION,
        network: network.to_string(),
        kdf: KdfSection {
            alg: KDF_ALG.into(),
            m_kib: DEFAULT_M_KIB,
            t: DEFAULT_T,
            p: DEFAULT_P,
            salt: B64.encode(salt),
        },
        cipher: CipherSection {
            alg: CIPHER_ALG.into(),
            nonce: B64.encode(nonce),
            ciphertext: B64.encode(ciphertext),
        },
    };
    let json = serde_json::to_vec_pretty(&file)
        .map_err(|e| WalletError::Keystore(format!("could not encode keystore: {e}")))?;
    write_new_file(path, &json)
}

/// Read and decrypt the mnemonic. `WalletNotFound` if missing, `NetworkMismatch` if the
/// file belongs to another network, `WrongPassword` if decryption fails.
pub fn load(path: &Path, network: Network, password: &SecretString) -> Result<Mnemonic> {
    let bytes = read_capped(path)?;

    let probe: VersionProbe = serde_json::from_slice(&bytes).map_err(malformed)?;
    if probe.version != KEYSTORE_VERSION {
        return Err(WalletError::Keystore(format!(
            "unsupported keystore version {} (this build reads version {KEYSTORE_VERSION})",
            probe.version
        )));
    }
    let file: KeystoreFile = serde_json::from_slice(&bytes).map_err(malformed)?;
    if file.kdf.alg != KDF_ALG {
        return Err(WalletError::Keystore(format!(
            "unsupported key derivation function `{}`",
            file.kdf.alg
        )));
    }
    if file.cipher.alg != CIPHER_ALG {
        return Err(WalletError::Keystore(format!(
            "unsupported cipher `{}`",
            file.cipher.alg
        )));
    }

    // A friendly early error. The real protection is the AAD: editing this field to match
    // the requested network makes decryption fail below.
    let found: Network = file
        .network
        .parse()
        .map_err(|_| WalletError::Keystore("keystore names an unknown network".into()))?;
    if found != network {
        return Err(WalletError::NetworkMismatch {
            expected: network,
            found: found.to_string(),
        });
    }

    let KdfSection { m_kib, t, p, .. } = file.kdf;
    if !(1..=MAX_M_KIB).contains(&m_kib) || !(1..=MAX_T).contains(&t) || !(1..=MAX_P).contains(&p) {
        return Err(WalletError::Keystore(format!(
            "KDF parameters out of range (m_kib ≤ {MAX_M_KIB}, t ≤ {MAX_T}, p ≤ {MAX_P})"
        )));
    }
    let salt: [u8; SALT_LEN] = decode_exact("salt", &file.kdf.salt)?;
    let nonce: [u8; NONCE_LEN] = decode_exact("nonce", &file.cipher.nonce)?;
    let ciphertext = B64
        .decode(&file.cipher.ciphertext)
        .map_err(|_| WalletError::Keystore("ciphertext is not valid base64".into()))?;

    let key = derive_key(password.expose_secret().as_bytes(), &salt, m_kib, t, p)?;
    let aad = aad(network);
    // The Poly1305 tag authenticates the ciphertext *and* the AAD, so a wrong password, a
    // flipped byte and a relabelled network all fail here the same way.
    let plaintext = Zeroizing::new(
        cipher(&key)?
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &ciphertext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| WalletError::WrongPassword)?,
    );
    let phrase = std::str::from_utf8(&plaintext)
        .map_err(|_| WalletError::Keystore("decrypted keystore is not valid UTF-8".into()))?;
    keys::parse_mnemonic(phrase)
}

fn aad(network: Network) -> String {
    format!("{AAD_PREFIX}{network}")
}

/// Argon2id(password, salt) → 32-byte key, wiped on drop.
fn derive_key(
    password: &[u8],
    salt: &[u8],
    m_kib: u32,
    t: u32,
    p: u32,
) -> Result<Zeroizing<[u8; KEY_LEN]>> {
    let params = Params::new(m_kib, t, p, Some(KEY_LEN))
        .map_err(|e| WalletError::Keystore(format!("invalid KDF parameters: {e}")))?;
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password, salt, &mut key[..])
        .map_err(|e| WalletError::Keystore(format!("key derivation failed: {e}")))?;
    Ok(key)
}

fn cipher(key: &[u8; KEY_LEN]) -> Result<XChaCha20Poly1305> {
    XChaCha20Poly1305::new_from_slice(key)
        .map_err(|_| WalletError::Keystore("invalid cipher key length".into()))
}

fn fill_random(buf: &mut [u8]) -> Result<()> {
    getrandom::fill(buf)
        .map_err(|e| WalletError::Keystore(format!("OS random number generator failed: {e}")))
}

/// Base64 field that must decode to exactly `N` bytes (a wrong length would otherwise only
/// surface as a confusing decryption failure, or not at all for the salt).
fn decode_exact<const N: usize>(field: &str, value: &str) -> Result<[u8; N]> {
    B64.decode(value)
        .ok()
        .and_then(|bytes| <[u8; N]>::try_from(bytes).ok())
        .ok_or_else(|| {
            WalletError::Keystore(format!("{field} must be {N} bytes of standard base64"))
        })
}

/// serde_json errors describe structure (field names, positions); the file holds no plaintext.
fn malformed(e: serde_json::Error) -> WalletError {
    WalletError::Keystore(format!("unreadable keystore file: {e}"))
}

fn read_capped(path: &Path) -> Result<Vec<u8>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(WalletError::WalletNotFound(path.to_path_buf()));
        }
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(WalletError::Keystore("keystore file is too large".into()));
    }
    Ok(bytes)
}

fn ensure_absent(path: &Path) -> Result<()> {
    // `symlink_metadata` so that even a dangling symlink at `path` counts as "exists".
    match fs::symlink_metadata(path) {
        Ok(_) => Err(WalletError::WalletExists(path.to_path_buf())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Write `contents` to `path` so that readers only ever see no file or the complete file.
///
/// The data goes to a fresh temp file in the same directory (created with `create_new`, so
/// nothing is ever overwritten, and mode 0600 from the start, so the ciphertext is never
/// world-readable, not even briefly), is flushed to disk, and is then renamed into place.
/// Rename is atomic within one filesystem, which is why the temp file lives next to the target.
fn write_new_file(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .ok_or_else(|| WalletError::Keystore("keystore path has no file name".into()))?;
    create_private_dir(dir)?;

    let mut suffix = [0u8; 8];
    fill_random(&mut suffix)?;
    let tmp: PathBuf = dir.join(format!(
        ".{}.{:016x}.tmp",
        name.to_string_lossy(),
        u64::from_le_bytes(suffix)
    ));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;

    let result = (|| -> Result<()> {
        file.write_all(contents)?;
        file.sync_all()?;
        // `rename` replaces an existing target, so re-check right before it. The window left
        // between this check and the rename is only reachable by two concurrent creates of the
        // same wallet, which the caller already prevents by checking first (see `api.rs`).
        ensure_absent(path)?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        // Don't leave a stray temp file behind; the original error is what matters.
        let _ = fs::remove_file(&tmp);
    }
    result?;

    sync_dir(dir);
    Ok(())
}

/// Make the rename itself durable across a power cut by syncing the directory entry.
/// Best effort: the file is already complete and in place, so failing here would only make
/// the caller report an error for a keystore that was in fact written.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        if let Err(e) = File::open(dir).and_then(|d| d.sync_all()) {
            tracing::warn!(dir = %dir.display(), error = %e, "could not sync keystore directory");
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// `create_dir_all`, but directories it creates are private to the user (0700) on unix.
fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    Ok(())
}
