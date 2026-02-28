//! Token encryption using AES-256-GCM
//!
//! Encrypts OAuth tokens at rest using a key derived from JWT_SECRET via HKDF.
//! Each encrypted value includes a random nonce for security.
//!
//! Format: base64(nonce || ciphertext || auth_tag)

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;

const NONCE_SIZE: usize = 12; // AES-GCM standard nonce size
const KEY_SIZE: usize = 32; // AES-256 key size

/// Derives an encryption key from the JWT secret using HKDF
pub fn derive_encryption_key(jwt_secret: &str) -> Vec<u8> {
    let hkdf = Hkdf::<Sha256>::new(Some(b"rinse-oauth-tokens"), jwt_secret.as_bytes());
    let mut key = vec![0u8; KEY_SIZE];
    hkdf.expand(b"oauth-token-encryption", &mut key)
        .expect("HKDF expand should not fail with valid parameters");
    key
}

/// Encrypts a token using AES-256-GCM
///
/// Returns base64(nonce || ciphertext)
pub fn encrypt_token(token: &str, key: &[u8]) -> Result<String> {
    if key.len() != KEY_SIZE {
        anyhow::bail!("Invalid key size: expected {}, got {}", KEY_SIZE, key.len());
    }

    let cipher = Aes256Gcm::new_from_slice(key)
        .context("Failed to create cipher from key")?;

    // Generate random nonce
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt the token
    let ciphertext = cipher
        .encrypt(nonce, token.as_bytes())
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

    // Combine nonce + ciphertext and base64 encode
    let mut combined = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    Ok(BASE64.encode(&combined))
}

/// Decrypts a token that was encrypted with encrypt_token
///
/// Expects base64(nonce || ciphertext)
pub fn decrypt_token(encrypted: &str, key: &[u8]) -> Result<String> {
    if key.len() != KEY_SIZE {
        anyhow::bail!("Invalid key size: expected {}, got {}", KEY_SIZE, key.len());
    }

    let cipher = Aes256Gcm::new_from_slice(key)
        .context("Failed to create cipher from key")?;

    // Decode base64
    let combined = BASE64
        .decode(encrypted)
        .context("Failed to decode base64")?;

    if combined.len() < NONCE_SIZE {
        anyhow::bail!("Encrypted data too short");
    }

    // Split nonce and ciphertext
    let (nonce_bytes, ciphertext) = combined.split_at(NONCE_SIZE);
    let nonce = Nonce::from_slice(nonce_bytes);

    // Decrypt
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))?;

    String::from_utf8(plaintext).context("Decrypted data is not valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = derive_encryption_key("test-jwt-secret");
        let token = "ya29.a0AfH6SMA...very_long_access_token";

        let encrypted = encrypt_token(token, &key).unwrap();
        let decrypted = decrypt_token(&encrypted, &key).unwrap();

        assert_eq!(token, decrypted);
    }

    #[test]
    fn test_different_encryptions_produce_different_ciphertext() {
        let key = derive_encryption_key("test-jwt-secret");
        let token = "same_token";

        let encrypted1 = encrypt_token(token, &key).unwrap();
        let encrypted2 = encrypt_token(token, &key).unwrap();

        // Different nonces should produce different ciphertexts
        assert_ne!(encrypted1, encrypted2);

        // But both should decrypt to the same value
        assert_eq!(decrypt_token(&encrypted1, &key).unwrap(), token);
        assert_eq!(decrypt_token(&encrypted2, &key).unwrap(), token);
    }

    #[test]
    fn test_wrong_key_fails() {
        let key1 = derive_encryption_key("secret1");
        let key2 = derive_encryption_key("secret2");
        let token = "test_token";

        let encrypted = encrypt_token(token, &key1).unwrap();
        let result = decrypt_token(&encrypted, &key2);

        assert!(result.is_err());
    }

    #[test]
    fn test_tampered_ciphertext_fails() {
        let key = derive_encryption_key("test-jwt-secret");
        let token = "test_token";

        let encrypted = encrypt_token(token, &key).unwrap();

        // Tamper with the ciphertext
        let mut bytes = BASE64.decode(&encrypted).unwrap();
        if let Some(byte) = bytes.last_mut() {
            *byte ^= 0xFF;
        }
        let tampered = BASE64.encode(&bytes);

        let result = decrypt_token(&tampered, &key);
        assert!(result.is_err());
    }
}
