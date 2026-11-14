//! Link secrets for link-visible trees: reading them from references and
//! unmasking root keys with them. Secrets stay client-side; the parsed
//! reference carries neither the query string nor the `k=` fragment.

use sha2::{Digest, Sha256};

use crate::{xor_key_bytes, LinkKeyBytes, ObjectKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LinkSecretError {
    #[error("invalid link key: expected 64 or 66 hex characters")]
    Invalid,
    #[error("conflicting link keys")]
    Conflict,
    #[error("link key does not match this tree")]
    Mismatch,
}

/// An htree reference split from its link secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSecretRef<'a> {
    /// The reference without query string or fragment.
    pub reference: &'a str,
    /// A fragment other than `k=`, such as `private`.
    pub fragment: Option<&'a str>,
    pub link_key: Option<LinkKeyBytes>,
}

impl<'a> LinkSecretRef<'a> {
    /// Reads `#k=<hex>` and a locally accepted `?k=<hex>`. Query strings are
    /// not part of the tree path, so the whole query is left out of `reference`.
    pub fn parse(input: &'a str) -> Result<Self, LinkSecretError> {
        let (rest, fragment) = split(input, '#');
        let (reference, query) = split(rest, '?');
        let mut parsed = Self {
            reference,
            fragment: None,
            link_key: None,
        };
        for param in query.into_iter().flat_map(|query| query.split('&')) {
            if let Some(hex) = param.strip_prefix("k=") {
                parsed = parsed.with_link_key(Some(hex))?;
            }
        }
        match fragment.map(|fragment| fragment.strip_prefix("k=").ok_or(fragment)) {
            Some(Ok(hex)) => parsed.with_link_key(Some(hex)),
            Some(Err(other)) => Ok(Self {
                fragment: Some(other),
                ..parsed
            }),
            None => Ok(parsed),
        }
    }

    /// Adds a separately supplied secret, rejecting one that differs.
    pub fn with_link_key(mut self, hex: Option<&str>) -> Result<Self, LinkSecretError> {
        let Some(hex) = hex else {
            return Ok(self);
        };
        let key = LinkKeyBytes::from_hex(hex).map_err(|_| LinkSecretError::Invalid)?;
        if self.link_key.is_some_and(|existing| existing != key) {
            return Err(LinkSecretError::Conflict);
        }
        self.link_key = Some(key);
        Ok(self)
    }
}

fn split(input: &str, separator: char) -> (&str, Option<&str>) {
    match input.split_once(separator) {
        Some((head, tail)) => (head, (!tail.is_empty()).then_some(tail)),
        None => (input, None),
    }
}

impl LinkKeyBytes {
    /// The `keyId` tag: first 8 bytes of SHA-256 of the secret, as lowercase hex.
    pub fn key_id(&self) -> String {
        hex::encode(&Sha256::digest(self.as_ref())[..8])
    }

    /// Recovers a root key from its `encryptedKey` mask, checking `keyId` when present.
    pub fn unmask(
        &self,
        encrypted_key: &[u8],
        key_id: Option<&str>,
    ) -> Result<ObjectKey, LinkSecretError> {
        if key_id.is_some_and(|id| !id.eq_ignore_ascii_case(&self.key_id())) {
            return Err(LinkSecretError::Mismatch);
        }
        let bytes =
            xor_key_bytes(encrypted_key, self.as_ref()).map_err(|_| LinkSecretError::Mismatch)?;
        ObjectKey::from_bytes(&bytes).map_err(|_| LinkSecretError::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixture from the Hashtree references spec (versioned randomized root and link mask).
    const SECRET: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20";
    const ENCRYPTED_KEY: &str =
        "02030001060704050a0b08090e0f0c0d12131011161714151a1b18191e1f1c1d22";
    const KEY_ID: &str = "5d8fcfefa9aeeb71";

    fn secret() -> LinkKeyBytes {
        LinkKeyBytes::from_hex(SECRET).unwrap()
    }

    #[test]
    fn parses_fragment_and_query_secrets_out_of_the_reference() {
        let hex = "ab".repeat(32);
        for input in [
            format!("htree://npub1x/tree/a.txt#k={hex}"),
            format!("npub1x/tree/a.txt?k={hex}"),
            format!("npub1x/tree/a.txt?v=1&k={hex}#k={hex}"),
        ] {
            let parsed = LinkSecretRef::parse(&input).unwrap();
            assert!(parsed.reference.ends_with("npub1x/tree/a.txt"), "{input}");
            assert_eq!(parsed.fragment, None);
            assert_eq!(parsed.link_key, Some(LinkKeyBytes::from_hex(&hex).unwrap()));
        }
    }

    #[test]
    fn keeps_other_fragments_and_plain_references() {
        let parsed = LinkSecretRef::parse("htree://self/repo#private").unwrap();
        assert_eq!(parsed.reference, "htree://self/repo");
        assert_eq!(parsed.fragment, Some("private"));
        assert_eq!(parsed.link_key, None);

        let parsed = LinkSecretRef::parse("npub1x/tree#").unwrap();
        assert_eq!((parsed.reference, parsed.fragment), ("npub1x/tree", None));
    }

    #[test]
    fn rejects_invalid_and_conflicting_secrets() {
        let (a, b) = ("aa".repeat(32), "bb".repeat(32));
        assert_eq!(
            LinkSecretRef::parse("npub1x/tree#k=abc"),
            Err(LinkSecretError::Invalid)
        );
        assert_eq!(
            LinkSecretRef::parse(&format!("npub1x/tree?k={a}#k={b}")),
            Err(LinkSecretError::Conflict)
        );
        assert_eq!(
            LinkSecretRef::parse(&format!("npub1x/tree?k={a}&k={b}")),
            Err(LinkSecretError::Conflict)
        );
        assert_eq!(
            LinkSecretRef::parse(&format!("npub1x/tree#k={a}"))
                .unwrap()
                .with_link_key(Some(&b)),
            Err(LinkSecretError::Conflict)
        );
        let input = format!("npub1x/tree#k={a}");
        let same = LinkSecretRef::parse(&input)
            .unwrap()
            .with_link_key(Some(&a))
            .unwrap();
        assert_eq!(same.link_key, Some(LinkKeyBytes::from_hex(&a).unwrap()));
    }

    #[test]
    fn key_id_and_unmask_match_the_spec_fixture() {
        assert_eq!(secret().key_id(), KEY_ID);
        let masked = hex::decode(ENCRYPTED_KEY).unwrap();
        let key = secret().unmask(&masked, Some(KEY_ID)).unwrap();
        assert_eq!(key.as_bytes(), &[2u8; 33]);
        assert_eq!(secret().unmask(&masked, None).unwrap(), key);
    }

    #[test]
    fn unmask_rejects_a_wrong_secret() {
        let masked = hex::decode(ENCRYPTED_KEY).unwrap();
        let wrong = LinkKeyBytes::from_bytes(&[7u8; 33]).unwrap();
        assert_eq!(
            wrong.unmask(&masked, Some(KEY_ID)),
            Err(LinkSecretError::Mismatch)
        );
        let short = LinkKeyBytes::from_bytes(&[7u8; 32]).unwrap();
        assert_eq!(short.unmask(&masked, None), Err(LinkSecretError::Mismatch));
    }
}
