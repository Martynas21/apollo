//! AES-256-GCM encryption for OAuth tokens at rest.
//!
//! Ciphertext is stored as `base64(nonce(12B) || ciphertext || tag)` so it
//! fits directly into the existing `TEXT` columns in `users` — no schema
//! change needed to move from plaintext to encrypted tokens.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

pub type TokenKey = aes_gcm::Key<Aes256Gcm>;

const NONCE_LEN: usize = 12;

/// Parses a base64-encoded 256-bit key, e.g. from `TOKEN_ENCRYPTION_KEY`.
pub fn parse_key(base64_key: &str) -> Result<TokenKey> {
    let bytes = STANDARD
        .decode(base64_key.trim())
        .context("TOKEN_ENCRYPTION_KEY is not valid base64")?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "TOKEN_ENCRYPTION_KEY must decode to exactly 32 bytes, got {}",
            bytes.len()
        );
    }
    Ok(*TokenKey::from_slice(&bytes))
}

/// Encrypts `plaintext` under `key`, returning a base64 string safe to store
/// in a `TEXT` column.
pub fn encrypt(key: &TokenKey, plaintext: &str) -> String {
    let cipher = Aes256Gcm::new(key);
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .expect("AES-256-GCM encryption of a bounded plaintext cannot fail");

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    STANDARD.encode(out)
}

/// Decrypts a value previously produced by [`encrypt`]. Fails on invalid
/// base64, a truncated payload, a wrong key, or (deliberately) on legacy
/// plaintext that predates encryption — callers migrating old rows rely on
/// that failure to detect them.
pub fn decrypt(key: &TokenKey, encoded: &str) -> Result<String> {
    let data = STANDARD
        .decode(encoded)
        .context("stored token is not valid base64")?;
    if data.len() < NONCE_LEN {
        anyhow::bail!("stored token ciphertext is too short");
    }
    let (nonce_bytes, ciphertext) = data.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);

    let cipher = Aes256Gcm::new(key);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("failed to decrypt stored token (wrong key or corrupted data)"))?;

    String::from_utf8(plaintext).context("decrypted token is not valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> TokenKey {
        parse_key("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").expect("valid test key")
    }

    #[test]
    fn round_trips() {
        let key = test_key();
        let encrypted = encrypt(&key, "super-secret-token");
        assert_eq!(decrypt(&key, &encrypted).unwrap(), "super-secret-token");
    }

    #[test]
    fn two_encryptions_of_the_same_plaintext_differ() {
        let key = test_key();
        assert_ne!(encrypt(&key, "same"), encrypt(&key, "same"));
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let key = test_key();
        let other_key = parse_key("enp6enp6enp6enp6enp6enp6enp6enp6enp6enp6eno=").expect("valid key");
        let encrypted = encrypt(&key, "super-secret-token");
        assert!(decrypt(&other_key, &encrypted).is_err());
    }

    #[test]
    fn legacy_plaintext_fails_to_decrypt() {
        let key = test_key();
        assert!(decrypt(&key, "this-was-never-encrypted").is_err());
    }

    #[test]
    fn rejects_short_key() {
        assert!(parse_key("dG9vc2hvcnQ=").is_err());
    }
}
