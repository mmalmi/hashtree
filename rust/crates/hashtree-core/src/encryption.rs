//! Explicit encryption suites with legacy CHK key compatibility.

use crate::crypto::{self, CryptoError, EncryptionKey};

/// Writer selection. New writes use versioned CHK keys by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncryptionMode {
    LegacyChk,
    #[default]
    ChkV1,
    RndV1,
}

/// A validated legacy 32-byte key or a 33-byte suite-prefixed key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey {
    bytes: [u8; 33],
    length: u8,
}

impl std::fmt::Debug for ObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectKey")
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

impl ObjectKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        match bytes.len() {
            32 => {}
            33 if matches!(bytes[0], 1 | 2) => {}
            33 => return Err(CryptoError::UnsupportedSuite(bytes[0])),
            _ => return Err(CryptoError::InvalidKeyLength),
        }
        let mut stored = [0; 33];
        stored[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            bytes: stored,
            length: bytes.len() as u8,
        })
    }

    pub fn from_hex(value: &str) -> Result<Self, CryptoError> {
        let bytes = hex::decode(value).map_err(|_| CryptoError::InvalidKeyLength)?;
        Self::from_bytes(&bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.length as usize]
    }

    pub fn mode(&self) -> EncryptionMode {
        if self.length == 32 {
            EncryptionMode::LegacyChk
        } else if self.bytes[0] == 1 {
            EncryptionMode::ChkV1
        } else {
            EncryptionMode::RndV1
        }
    }

    pub fn raw_key(&self) -> EncryptionKey {
        self.as_bytes()[self.length as usize - 32..]
            .try_into()
            .expect("validated key")
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.as_bytes())
    }

    fn versioned(suite: u8, key: EncryptionKey) -> Self {
        let mut bytes = [0; 33];
        bytes[0] = suite;
        bytes[1..].copy_from_slice(&key);
        Self { bytes, length: 33 }
    }
}

impl From<EncryptionKey> for ObjectKey {
    fn from(key: EncryptionKey) -> Self {
        let mut bytes = [0; 33];
        bytes[..32].copy_from_slice(&key);
        Self { bytes, length: 32 }
    }
}

impl AsRef<[u8]> for ObjectKey {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

pub fn encrypt_object(
    plaintext: &[u8],
    mode: EncryptionMode,
) -> Result<(Vec<u8>, ObjectKey), CryptoError> {
    match mode {
        EncryptionMode::LegacyChk | EncryptionMode::ChkV1 => {
            let (bytes, key) = crypto::encrypt_chk(plaintext)?;
            let key = if mode == EncryptionMode::LegacyChk {
                key.into()
            } else {
                ObjectKey::versioned(1, key)
            };
            Ok((bytes, key))
        }
        EncryptionMode::RndV1 => {
            let key = crypto::generate_key();
            Ok((
                crypto::encrypt(plaintext, &key)?,
                ObjectKey::versioned(2, key),
            ))
        }
    }
}

pub fn decrypt_object(bytes: &[u8], key: &ObjectKey) -> Result<Vec<u8>, CryptoError> {
    match key.mode() {
        EncryptionMode::LegacyChk | EncryptionMode::ChkV1 => {
            crypto::decrypt_chk(bytes, &key.raw_key())
        }
        EncryptionMode::RndV1 => crypto::decrypt(bytes, &key.raw_key()),
    }
}

impl std::ops::Deref for ObjectKey {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl serde::Serialize for ObjectKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_bytes().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ObjectKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        Self::from_bytes(&bytes).map_err(serde::de::Error::custom)
    }
}
