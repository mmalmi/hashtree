//! Hashing utilities using SHA256

use sha2::{Sha256, Digest};
use crate::types::Hash;

/// Compute SHA256 hash of data
pub fn sha256(data: &[u8]) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&result);
    hash
}

/// Verify that data matches expected hash
pub fn verify(hash: &Hash, data: &[u8]) -> bool {
    let computed = sha256(data);
    computed == *hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::to_hex;

    #[test]
    fn test_sha256_empty() {
        let hash = sha256(&[]);
        assert_eq!(
            to_hex(&hash),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_sha256_hello_world() {
        let data = b"hello world";
        let hash = sha256(data);
        assert_eq!(
            to_hex(&hash),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_sha256_consistent() {
        let data = [1u8, 2, 3, 4, 5];
        let hash1 = sha256(&data);
        let hash2 = sha256(&data);
        assert_eq!(to_hex(&hash1), to_hex(&hash2));
    }

    #[test]
    fn test_sha256_length() {
        let data = [1u8, 2, 3];
        let hash = sha256(&data);
        assert_eq!(hash.len(), 32);
    }

    #[test]
    fn test_verify() {
        let data = b"test data";
        let hash = sha256(data);
        assert!(verify(&hash, data));
        assert!(!verify(&hash, b"different data"));
    }
}
