//! Tree visibility helpers

use std::str::FromStr;

/// Tree visibility modes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeVisibility {
    Public,
    LinkVisible,
    Private,
}

impl TreeVisibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            TreeVisibility::Public => "public",
            TreeVisibility::LinkVisible => "link-visible",
            TreeVisibility::Private => "private",
        }
    }
}

impl FromStr for TreeVisibility {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "public" => Ok(TreeVisibility::Public),
            "link-visible" | "link_visible" | "linkvisible" => Ok(TreeVisibility::LinkVisible),
            "private" => Ok(TreeVisibility::Private),
            _ => Err(format!("invalid visibility: {}", s)),
        }
    }
}

/// XOR two 32-byte keys (used for link-visible key masking)
pub fn xor_keys(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// Opaque link secret or masked key. Its first byte need not identify a suite.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKeyBytes {
    Legacy([u8; 32]),
    Versioned([u8; 33]),
}

impl std::fmt::Debug for LinkKeyBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinkKeyBytes(..)")
    }
}

impl LinkKeyBytes {
    pub fn from_hex(value: &str) -> Result<Self, crate::CryptoError> {
        Self::from_bytes(&hex::decode(value).map_err(|_| crate::CryptoError::InvalidKeyLength)?)
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, crate::CryptoError> {
        match bytes.len() {
            32 => Ok(Self::Legacy(bytes.try_into().unwrap())),
            33 => Ok(Self::Versioned(bytes.try_into().unwrap())),
            _ => Err(crate::CryptoError::InvalidKeyLength),
        }
    }
}

impl AsRef<[u8]> for LinkKeyBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Legacy(bytes) => bytes,
            Self::Versioned(bytes) => bytes,
        }
    }
}

/// Preserve an existing 32-byte share secret by encoding a CHK root key in its
/// equivalent legacy form. Randomized roots always require a 33-byte secret.
pub fn mask_object_key(
    key: &crate::ObjectKey,
    secret: &[u8],
) -> Result<Vec<u8>, crate::CryptoError> {
    match (key.mode(), secret.len()) {
        (crate::EncryptionMode::ChkV1, 32) => xor_key_bytes(&key.raw_key(), secret),
        (crate::EncryptionMode::LegacyChk, 33) => {
            let mut encoded = [1u8; 33];
            encoded[1..].copy_from_slice(key.as_bytes());
            xor_key_bytes(&encoded, secret)
        }
        _ => xor_key_bytes(key.as_bytes(), secret),
    }
}

/// Mask or unmask a legacy or versioned key with an equally sized link secret.
/// The masked bytes and link secret are opaque, not parsed encryption keys.
pub fn xor_key_bytes(a: &[u8], b: &[u8]) -> Result<Vec<u8>, crate::crypto::CryptoError> {
    if a.len() != b.len() || !matches!(a.len(), 32 | 33) {
        return Err(crate::crypto::CryptoError::InvalidKeyLength);
    }
    Ok(a.iter().zip(b).map(|(a, b)| a ^ b).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_visibility_parse() {
        assert_eq!(
            TreeVisibility::from_str("public").unwrap(),
            TreeVisibility::Public
        );
        assert_eq!(
            TreeVisibility::from_str("link-visible").unwrap(),
            TreeVisibility::LinkVisible
        );
        assert_eq!(
            TreeVisibility::from_str("link_visible").unwrap(),
            TreeVisibility::LinkVisible
        );
        assert_eq!(
            TreeVisibility::from_str("private").unwrap(),
            TreeVisibility::Private
        );
        assert!(TreeVisibility::from_str("unknown").is_err());
    }

    #[test]
    fn test_xor_keys_roundtrip() {
        let a = [0x11u8; 32];
        let b = [0x22u8; 32];
        let masked = xor_keys(&a, &b);
        let unmasked = xor_keys(&masked, &b);
        assert_eq!(unmasked, a);
    }
}
