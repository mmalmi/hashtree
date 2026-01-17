//! Content Hash Key (CHK) encryption for HashTree
//!
//! **⚠️ EXPERIMENTAL: Encryption API is unstable and may change.**
//!
//! Uses convergent encryption where the key is derived from the content itself.
//! This enables deduplication: same content → same ciphertext.
//!
//! Algorithm:
//! 1. content_hash = SHA256(plaintext)
//! 2. key = HKDF-SHA256(content_hash, salt="hashtree-chk", info="encryption-key")
//! 3. ciphertext = AES-256-GCM(key, zero_nonce, plaintext)
//!
//! Zero nonce is safe because CHK guarantees same key = same content.
//!
//! Format: [ciphertext][16-byte auth tag]
//!
//! The content_hash acts as the "decryption key" - store it securely.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// 32-byte encryption key (256 bits) - this is the content hash
pub type EncryptionKey = [u8; 32];

/// Nonce size for AES-GCM (96 bits)
const NONCE_SIZE: usize = 12;

/// Auth tag size for AES-GCM
const TAG_SIZE: usize = 16;

/// HKDF salt for CHK derivation
const CHK_SALT: &[u8] = b"hashtree-chk";

/// Encryption error
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("Encryption failed: {0}")]
    EncryptionFailed(String),
    #[error("Decryption failed: {0}")]
    DecryptionFailed(String),
    #[error("Encrypted data too short")]
    DataTooShort,
    #[error("Invalid key length")]
    InvalidKeyLength,
    #[error("Key derivation failed")]
    KeyDerivationFailed,
}

/// Derive encryption key from content hash using HKDF
fn derive_key(content_hash: &[u8; 32]) -> Result<[u8; 32], CryptoError> {
    let hk = Hkdf::<Sha256>::new(Some(CHK_SALT), content_hash);

    let mut key = [0u8; 32];
    hk.expand(b"encryption-key", &mut key)
        .map_err(|_| CryptoError::KeyDerivationFailed)?;

    Ok(key)
}

/// Generate a random 32-byte key (for non-CHK encryption)
pub fn generate_key() -> EncryptionKey {
    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    key
}

/// Compute content hash (SHA256) - this becomes the decryption key for CHK
pub fn content_hash(data: &[u8]) -> EncryptionKey {
    let hash = Sha256::digest(data);
    let mut result = [0u8; 32];
    result.copy_from_slice(&hash);
    result
}

/// CHK encrypt: derive key from content, encrypt with zero nonce
///
/// Returns: (ciphertext with auth tag, content_hash as decryption key)
///
/// Zero nonce is safe because CHK guarantees: same key = same content.
/// We never encrypt different content with the same key.
///
/// The content_hash is both:
/// - The decryption key (store securely, share with authorized users)
/// - Enables dedup: same content → same ciphertext
pub fn encrypt_chk(plaintext: &[u8]) -> Result<(Vec<u8>, EncryptionKey), CryptoError> {
    let chash = content_hash(plaintext);
    let key = derive_key(&chash)?;
    let zero_nonce = [0u8; NONCE_SIZE];

    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&zero_nonce), plaintext)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    Ok((ciphertext, chash))
}

/// CHK decrypt: derive key from content_hash, decrypt with zero nonce
///
/// The key parameter is the content_hash returned from encrypt_chk
pub fn decrypt_chk(ciphertext: &[u8], key: &EncryptionKey) -> Result<Vec<u8>, CryptoError> {
    if ciphertext.len() < TAG_SIZE {
        return Err(CryptoError::DataTooShort);
    }

    let enc_key = derive_key(key)?;
    let zero_nonce = [0u8; NONCE_SIZE];

    let cipher = Aes256Gcm::new_from_slice(&enc_key)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

    cipher
        .decrypt(Nonce::from_slice(&zero_nonce), ciphertext)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))
}

/// Encrypt with a provided key (non-CHK, random nonce)
///
/// Returns: [12-byte nonce][ciphertext][16-byte auth tag]
pub fn encrypt(plaintext: &[u8], key: &EncryptionKey) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    let mut nonce_bytes = [0u8; NONCE_SIZE];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);

    Ok(result)
}

/// Decrypt with a provided key (non-CHK)
///
/// Input: [12-byte nonce][ciphertext][auth tag]
pub fn decrypt(encrypted: &[u8], key: &EncryptionKey) -> Result<Vec<u8>, CryptoError> {
    if encrypted.len() < NONCE_SIZE + TAG_SIZE {
        return Err(CryptoError::DataTooShort);
    }

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;

    let nonce = Nonce::from_slice(&encrypted[..NONCE_SIZE]);
    let ciphertext = &encrypted[NONCE_SIZE..];

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))
}

/// Check if data could be encrypted (based on minimum size for non-CHK)
pub fn could_be_encrypted(data: &[u8]) -> bool {
    data.len() >= NONCE_SIZE + TAG_SIZE
}

/// Calculate encrypted size for given plaintext size (non-CHK with nonce prefix)
pub fn encrypted_size(plaintext_size: usize) -> usize {
    NONCE_SIZE + plaintext_size + TAG_SIZE
}

/// Calculate encrypted size for CHK (no nonce prefix)
pub fn encrypted_size_chk(plaintext_size: usize) -> usize {
    plaintext_size + TAG_SIZE
}

/// Calculate plaintext size from encrypted size (non-CHK)
pub fn plaintext_size(encrypted_size: usize) -> usize {
    encrypted_size.saturating_sub(NONCE_SIZE + TAG_SIZE)
}

/// Convert key to hex string
pub fn key_to_hex(key: &EncryptionKey) -> String {
    hex::encode(key)
}

/// Convert hex string to key
pub fn key_from_hex(hex_str: &str) -> Result<EncryptionKey, CryptoError> {
    let bytes = hex::decode(hex_str).map_err(|_| CryptoError::InvalidKeyLength)?;
    if bytes.len() != 32 {
        return Err(CryptoError::InvalidKeyLength);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chk_encrypt_decrypt() {
        let plaintext = b"Hello, World!";

        let (ciphertext, key) = encrypt_chk(plaintext).unwrap();
        let decrypted = decrypt_chk(&ciphertext, &key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_chk_deterministic() {
        let plaintext = b"Same content produces same ciphertext";

        let (ciphertext1, key1) = encrypt_chk(plaintext).unwrap();
        let (ciphertext2, key2) = encrypt_chk(plaintext).unwrap();

        // Same content = same key = same ciphertext (dedup works!)
        assert_eq!(key1, key2);
        assert_eq!(ciphertext1, ciphertext2);
    }

    #[test]
    fn test_chk_different_content() {
        let (ciphertext1, key1) = encrypt_chk(b"Content A").unwrap();
        let (ciphertext2, key2) = encrypt_chk(b"Content B").unwrap();

        // Different content = different everything
        assert_ne!(key1, key2);
        assert_ne!(ciphertext1, ciphertext2);
    }

    #[test]
    fn test_chk_wrong_key_fails() {
        let (ciphertext, _key) = encrypt_chk(b"Secret data").unwrap();
        let wrong_key = generate_key();

        let result = decrypt_chk(&ciphertext, &wrong_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_non_chk_encrypt_decrypt() {
        let key = generate_key();
        let plaintext = b"Hello, World!";

        let encrypted = encrypt(plaintext, &key).unwrap();
        let decrypted = decrypt(&encrypted, &key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_non_chk_random_nonce() {
        let key = generate_key();
        let plaintext = b"Same content";

        let encrypted1 = encrypt(plaintext, &key).unwrap();
        let encrypted2 = encrypt(plaintext, &key).unwrap();

        // Random nonce = different ciphertext
        assert_ne!(encrypted1, encrypted2);

        // But both decrypt correctly
        assert_eq!(decrypt(&encrypted1, &key).unwrap(), plaintext);
        assert_eq!(decrypt(&encrypted2, &key).unwrap(), plaintext);
    }

    #[test]
    fn test_empty_data() {
        let (ciphertext, key) = encrypt_chk(b"").unwrap();
        let decrypted = decrypt_chk(&ciphertext, &key).unwrap();
        assert_eq!(decrypted, b"");
    }

    #[test]
    fn test_large_data() {
        let plaintext = vec![0u8; 1024 * 1024]; // 1MB

        let (ciphertext, key) = encrypt_chk(&plaintext).unwrap();
        let decrypted = decrypt_chk(&ciphertext, &key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_key_hex_roundtrip() {
        let key = generate_key();
        let hex_str = key_to_hex(&key);
        let key2 = key_from_hex(&hex_str).unwrap();
        assert_eq!(key, key2);
    }

    #[test]
    fn test_encrypted_size_chk() {
        let plaintext = b"Test data";
        let (ciphertext, _) = encrypt_chk(plaintext).unwrap();
        assert_eq!(ciphertext.len(), encrypted_size_chk(plaintext.len()));
    }

    #[test]
    fn test_tampered_data_fails() {
        let (mut ciphertext, key) = encrypt_chk(b"Important data").unwrap();

        // Tamper with ciphertext
        if let Some(byte) = ciphertext.last_mut() {
            *byte ^= 0xFF;
        }

        let result = decrypt_chk(&ciphertext, &key);
        assert!(result.is_err());
    }
}
