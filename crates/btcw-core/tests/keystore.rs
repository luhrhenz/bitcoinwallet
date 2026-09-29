//! Agent A: the encrypted mnemonic keystore (Argon2id → XChaCha20-Poly1305).

use std::error::Error;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use btcw_core::WalletError;
use btcw_core::bitcoin::Network;
use btcw_core::keys::{self, Mnemonic, WordCount};
use btcw_core::keystore;
use secrecy::SecretString;
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;
/// A named edit to a keystore file's JSON.
type Edit = (&'static str, fn(&mut Value));

fn password(s: &str) -> SecretString {
    SecretString::from(s.to_string())
}

/// A saved regtest keystore inside a fresh temp dir (kept alive by the returned guard).
fn saved() -> Result<(tempfile::TempDir, PathBuf, Mnemonic), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("regtest").join("seed.enc");
    let mnemonic = keys::generate_mnemonic(WordCount::Words12)?;
    keystore::save(
        &path,
        &mnemonic,
        Network::Regtest,
        &password("correct horse"),
    )?;
    Ok((dir, path, mnemonic))
}

fn read_json(path: &Path) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn write_json(path: &Path, value: &Value) -> TestResult {
    std::fs::write(path, serde_json::to_vec(value)?)?;
    Ok(())
}

fn load_regtest(path: &Path) -> btcw_core::Result<Mnemonic> {
    keystore::load(path, Network::Regtest, &password("correct horse"))
}

#[test]
fn round_trip() -> TestResult {
    let (_dir, path, mnemonic) = saved()?;
    let loaded = load_regtest(&path)?;
    assert_eq!(*loaded.phrase(), *mnemonic.phrase());
    Ok(())
}

#[test]
fn file_matches_the_documented_format() -> TestResult {
    let (_dir, path, mnemonic) = saved()?;
    let json = read_json(&path)?;
    assert_eq!(json["version"], keystore::KEYSTORE_VERSION);
    assert_eq!(json["network"], "regtest");
    assert_eq!(json["kdf"]["alg"], "argon2id");
    assert_eq!(json["kdf"]["m_kib"], 65536);
    assert_eq!(json["kdf"]["t"], 3);
    assert_eq!(json["kdf"]["p"], 1);
    assert_eq!(json["cipher"]["alg"], "xchacha20poly1305");
    let b64_len = |v: &Value| -> Result<usize, Box<dyn Error>> {
        Ok(B64.decode(v.as_str().ok_or("not a string")?)?.len())
    };
    assert_eq!(b64_len(&json["kdf"]["salt"])?, 16);
    assert_eq!(b64_len(&json["cipher"]["nonce"])?, 24);
    // Ciphertext = plaintext + 16-byte Poly1305 tag.
    assert_eq!(
        b64_len(&json["cipher"]["ciphertext"])?,
        mnemonic.phrase().len() + 16
    );

    // No word of the mnemonic appears anywhere in the file.
    let raw = std::fs::read_to_string(&path)?;
    for word in mnemonic.phrase().split(' ') {
        assert!(!raw.contains(word), "keystore contains a plaintext word");
    }
    Ok(())
}

#[test]
fn salt_and_nonce_are_fresh_per_save() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mnemonic = keys::generate_mnemonic(WordCount::Words12)?;
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    keystore::save(&a, &mnemonic, Network::Regtest, &password("pw"))?;
    keystore::save(&b, &mnemonic, Network::Regtest, &password("pw"))?;
    let (a, b) = (read_json(&a)?, read_json(&b)?);
    assert_ne!(a["kdf"]["salt"], b["kdf"]["salt"]);
    assert_ne!(a["cipher"]["nonce"], b["cipher"]["nonce"]);
    assert_ne!(a["cipher"]["ciphertext"], b["cipher"]["ciphertext"]);
    Ok(())
}

#[test]
fn wrong_password() -> TestResult {
    let (_dir, path, _) = saved()?;
    let result = keystore::load(&path, Network::Regtest, &password("correct horsE"));
    assert!(matches!(result, Err(WalletError::WrongPassword)));
    Ok(())
}

#[test]
fn flipped_ciphertext_byte_fails_authentication() -> TestResult {
    let (_dir, path, _) = saved()?;
    let mut json = read_json(&path)?;
    let mut ct = B64.decode(
        json["cipher"]["ciphertext"]
            .as_str()
            .ok_or("no ciphertext")?,
    )?;
    ct[0] ^= 0x01;
    json["cipher"]["ciphertext"] = Value::from(B64.encode(ct));
    write_json(&path, &json)?;
    assert!(matches!(
        load_regtest(&path),
        Err(WalletError::WrongPassword)
    ));
    Ok(())
}

#[test]
fn relabelled_network_fails_authentication() -> TestResult {
    // Editing the plaintext `network` field passes the friendly check but not the AAD.
    let (_dir, path, _) = saved()?;
    let mut json = read_json(&path)?;
    json["network"] = Value::from("testnet4");
    write_json(&path, &json)?;
    let result = keystore::load(&path, Network::Testnet4, &password("correct horse"));
    assert!(matches!(result, Err(WalletError::WrongPassword)));
    Ok(())
}

#[test]
fn loading_as_another_network_is_a_mismatch() -> TestResult {
    let (_dir, path, _) = saved()?;
    match keystore::load(&path, Network::Testnet4, &password("correct horse")) {
        Err(WalletError::NetworkMismatch { expected, found }) => {
            assert_eq!(expected, Network::Testnet4);
            assert_eq!(found, "regtest");
        }
        other => return Err(format!("expected NetworkMismatch, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn refuses_to_overwrite() -> TestResult {
    let (_dir, path, mnemonic) = saved()?;
    let before = std::fs::read(&path)?;
    let result = keystore::save(&path, &mnemonic, Network::Regtest, &password("other"));
    assert!(matches!(result, Err(WalletError::WalletExists(p)) if p == path));
    assert_eq!(std::fs::read(&path)?, before);
    Ok(())
}

#[test]
fn missing_file_is_wallet_not_found() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("seed.enc");
    assert!(matches!(
        load_regtest(&path),
        Err(WalletError::WalletNotFound(p)) if p == path
    ));
    Ok(())
}

#[test]
fn empty_password_is_rejected() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("seed.enc");
    let mnemonic = keys::generate_mnemonic(WordCount::Words12)?;
    let result = keystore::save(&path, &mnemonic, Network::Regtest, &password(""));
    assert!(matches!(result, Err(WalletError::Keystore(_))));
    assert!(!path.exists());
    Ok(())
}

#[test]
fn leaves_only_the_keystore_behind() -> TestResult {
    let (_dir, path, _) = saved()?;
    let parent = path.parent().ok_or("no parent")?;
    let entries: Vec<_> = std::fs::read_dir(parent)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<Result<_, _>>()?;
    assert_eq!(entries, vec![std::ffi::OsString::from("seed.enc")]);
    Ok(())
}

#[cfg(unix)]
#[test]
fn file_is_owner_only() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, path, _) = saved()?;
    let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "mode is {mode:o}");
    // `save` created the missing network directory, private to the user.
    let dir_mode = std::fs::metadata(path.parent().ok_or("no parent")?)?
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700, "dir mode is {dir_mode:o}");
    Ok(())
}

#[test]
fn absurd_kdf_params_are_rejected_before_hashing() -> TestResult {
    let (_dir, path, _) = saved()?;
    let original = read_json(&path)?;
    // Each of these would take hours or terabytes of RAM if Argon2 actually ran.
    for (field, value) in [
        ("m_kib", Value::from(u32::MAX)),
        ("m_kib", Value::from(1024 * 1024 + 1)),
        ("t", Value::from(1_000_000)),
        ("p", Value::from(4096)),
        ("t", Value::from(0)),
    ] {
        let mut json = original.clone();
        json["kdf"][field] = value;
        write_json(&path, &json)?;
        assert!(
            matches!(load_regtest(&path), Err(WalletError::Keystore(_))),
            "{field} was accepted"
        );
    }
    Ok(())
}

#[test]
fn malformed_files_are_keystore_errors() -> TestResult {
    let (_dir, path, _) = saved()?;
    let original = read_json(&path)?;
    let edits: [Edit; 7] = [
        ("unknown version", |j| j["version"] = Value::from(2)),
        ("unknown kdf", |j| j["kdf"]["alg"] = Value::from("scrypt")),
        ("unknown cipher", |j| {
            j["cipher"]["alg"] = Value::from("aes256gcm")
        }),
        ("unknown network", |j| {
            j["network"] = Value::from("dogecoin")
        }),
        ("short nonce", |j| {
            j["cipher"]["nonce"] = Value::from(B64.encode([0u8; 12]))
        }),
        ("bad base64 salt", |j| {
            j["kdf"]["salt"] = Value::from("not base64!")
        }),
        ("unexpected field", |j| j["extra"] = Value::from(true)),
    ];
    for (what, edit) in edits {
        let mut json = original.clone();
        edit(&mut json);
        write_json(&path, &json)?;
        assert!(
            matches!(load_regtest(&path), Err(WalletError::Keystore(_))),
            "{what} was accepted"
        );
    }

    std::fs::write(&path, b"{ not json")?;
    assert!(matches!(load_regtest(&path), Err(WalletError::Keystore(_))));
    std::fs::write(&path, vec![b' '; 128 * 1024])?;
    assert!(matches!(load_regtest(&path), Err(WalletError::Keystore(_))));
    Ok(())
}
