//! Encrypted storage for all app data written to the user's device.
//!
//! Design:
//! - A single random 256-bit master key is generated on first use and stored
//!   in the **OS keychain** (macOS Keychain, Linux Secret Service/keyutils,
//!   Windows Credential Manager) via the `keyring` crate.
//! - Every file the app writes (config, GitHub token, Claude credentials)
//!   is encrypted with **AES-256-GCM** using that key and a fresh random
//!   nonce per write.
//! - If the keychain is unavailable (headless Linux without a secret
//!   service), the key falls back to a mode-600 file next to the data,
//!   which still protects against casual reads by other users.
//! - Legacy plaintext files are migrated transparently: reads fall back to
//!   plaintext once, and the next save encrypts.
//!
//! File format: magic bytes `GMENC1` + 12-byte nonce + ciphertext.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 6] = b"GMENC1";
const KEYRING_SERVICE: &str = "git-manage";
const KEYRING_USER: &str = "master-key";

/// Errors from encrypted storage.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StoreError(pub String);

pub type Result<T> = std::result::Result<T, StoreError>;

fn err<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError(e.to_string())
}

/// App config directory (`~/.config/git-manage` on Linux).
pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("git-manage")
}

// ---------------------------------------------------------------------------
// Master key
// ---------------------------------------------------------------------------

/// Loads (or creates on first use) the master encryption key.
///
/// Prefers the OS keychain; falls back to a mode-600 key file when no
/// keychain is available.
fn master_key() -> Result<[u8; 32]> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;

    // 1. OS keychain
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
        match entry.get_password() {
            Ok(encoded) => {
                if let Ok(bytes) = b64.decode(&encoded) {
                    if bytes.len() == 32 {
                        let mut key = [0u8; 32];
                        key.copy_from_slice(&bytes);
                        return Ok(key);
                    }
                }
            }
            Err(keyring::Error::NoEntry) => {
                let key: [u8; 32] = Aes256Gcm::generate_key(OsRng).into();
                if entry.set_password(&b64.encode(key)).is_ok() {
                    return Ok(key);
                }
                // Keychain write failed: fall through to file fallback.
            }
            Err(_) => { /* keychain unavailable: fall through */ }
        }
    }

    // 2. File fallback (mode 600)
    let path = config_dir().join(".master.key");
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
    }
    let key: [u8; 32] = Aes256Gcm::generate_key(OsRng).into();
    std::fs::create_dir_all(config_dir()).map_err(err)?;
    std::fs::write(&path, key).map_err(err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(key)
}

// ---------------------------------------------------------------------------
// Encrypt / decrypt
// ---------------------------------------------------------------------------

fn encrypt(plaintext: &[u8]) -> Result<Vec<u8>> {
    let key_bytes = master_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher.encrypt(&nonce, plaintext).map_err(err)?;
    let mut out = Vec::with_capacity(MAGIC.len() + nonce.len() + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

fn decrypt(data: &[u8]) -> Result<Vec<u8>> {
    let payload = data
        .strip_prefix(MAGIC.as_slice())
        .ok_or_else(|| StoreError("not an encrypted file".into()))?;
    if payload.len() < 12 {
        return Err(StoreError("encrypted file truncated".into()));
    }
    let (nonce, ciphertext) = payload.split_at(12);
    let key_bytes = master_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| StoreError("decryption failed (wrong key or corrupted file)".into()))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Writes `contents` encrypted to `path` (creating parent directories).
/// The file itself is also chmod 600 on Unix.
pub fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    let data = encrypt(contents.as_bytes())?;
    std::fs::write(path, data).map_err(err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Reads and decrypts `path`. Falls back to plaintext for files written by
/// older versions (they become encrypted on the next save).
pub fn read(path: &Path) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    if raw.starts_with(MAGIC) {
        decrypt(&raw).ok().and_then(|b| String::from_utf8(b).ok())
    } else {
        // Legacy plaintext file: accept it once; next save encrypts.
        String::from_utf8(raw).ok()
    }
}

/// Removes a stored file if present.
pub fn remove(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path).map_err(err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_encrypt_decrypt() {
        let secret = "token: very-secret-value-123";
        let encrypted = encrypt(secret.as_bytes()).unwrap();
        assert!(encrypted.starts_with(MAGIC));
        // Ciphertext must not contain the plaintext.
        assert!(!encrypted.windows(secret.len()).any(|w| w == secret.as_bytes()));
        let decrypted = decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, secret.as_bytes());
    }

    #[test]
    fn nonces_differ_between_writes() {
        let a = encrypt(b"same input").unwrap();
        let b = encrypt(b"same input").unwrap();
        assert_ne!(a, b, "fresh nonce per write must give different ciphertexts");
    }

    #[test]
    fn write_read_roundtrip_and_legacy_plaintext() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("data.json");

        write(&path, "{\"k\":\"v\"}").unwrap();
        let on_disk = std::fs::read(&path).unwrap();
        assert!(on_disk.starts_with(MAGIC));
        assert!(!on_disk.windows(3).any(|w| w == b"\"v\""));
        assert_eq!(read(&path).unwrap(), "{\"k\":\"v\"}");

        // Legacy plaintext files are still readable.
        let legacy = tmp.path().join("old.json");
        std::fs::write(&legacy, "{\"old\":true}").unwrap();
        assert_eq!(read(&legacy).unwrap(), "{\"old\":true}");
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let mut data = encrypt(b"payload").unwrap();
        let last = data.len() - 1;
        data[last] ^= 0xff;
        assert!(decrypt(&data).is_err());
    }
}
