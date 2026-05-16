use axum::http::StatusCode;
use hashtree_core::is_tree_node;

/// Minimum plausible encrypted CHK blob size.
const MIN_CHK_SIZE: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestRejection {
    pub status: StatusCode,
    pub reason: String,
}

#[inline]
pub fn content_type_base(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

pub fn is_chk_content_type(content_type: &str) -> bool {
    let base = content_type_base(content_type);
    base.is_empty() || base == "application/octet-stream"
}

#[inline]
pub fn looks_random(data: &[u8]) -> (bool, usize, usize) {
    let len = data.len();

    if len < MIN_CHK_SIZE {
        return (false, 0, MIN_CHK_SIZE);
    }

    let sample_size = if len > 256 { 256 } else { len };

    if sample_size < 64 {
        return (true, sample_size, 0);
    }

    let threshold = if sample_size >= 256 {
        140
    } else {
        (sample_size * 55) / 100
    };

    let mut seen = [0u32; 8];
    let mut unique = 0usize;

    for &b in data.iter().take(sample_size) {
        let idx = (b >> 5) as usize;
        let bit = 1u32 << (b & 31);
        if (seen[idx] & bit) == 0 {
            seen[idx] |= bit;
            unique += 1;
            if unique >= threshold {
                return (true, unique, threshold);
            }
        }
    }

    (unique >= threshold, unique, threshold)
}

pub fn validate_untrusted_blob(data: &[u8], require_random: bool) -> Result<(), IngestRejection> {
    if !require_random {
        return Ok(());
    }

    if is_tree_node(data) {
        return Ok(());
    }

    let (is_random, unique, threshold) = looks_random(data);
    if is_random {
        return Ok(());
    }

    if data.len() < MIN_CHK_SIZE {
        return Err(IngestRejection {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            reason: "Blob too small".to_string(),
        });
    }

    Err(IngestRejection {
        status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
        reason: format!("Data not encrypted. Unique: {unique} (min: {threshold})"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::{encode_tree_node, Link, LinkType, TreeNode};

    #[test]
    fn rejects_too_small_blobs() {
        let err = validate_untrusted_blob(&[0u8; 12], true).expect_err("rejected");
        assert_eq!(err.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(err.reason, "Blob too small");
    }

    #[test]
    fn accepts_tiny_plausible_blobs() {
        let data: Vec<u8> = (0..32).collect();
        assert!(validate_untrusted_blob(&data, true).is_ok());
    }

    #[test]
    fn accepts_high_entropy_sample() {
        let data: Vec<u8> = (0..=255).collect();
        assert!(validate_untrusted_blob(&data, true).is_ok());
    }

    #[test]
    fn accepts_low_entropy_hashtree_metadata_node() {
        let links = (0..20)
            .map(|_| {
                Link::new([0u8; 32])
                    .with_name("root.json")
                    .with_link_type(LinkType::File)
            })
            .collect();
        let data = encode_tree_node(&TreeNode::dir(links)).expect("tree node");

        assert!(data.len() >= 64);
        assert!(!looks_random(&data).0);
        assert!(validate_untrusted_blob(&data, true).is_ok());
    }

    #[test]
    fn rejects_plain_text() {
        let data = b"Hello world! This is plain text that should be rejected because it has low entropy. The quick brown fox jumps over the lazy dog. Lorem ipsum dolor sit amet, consectetur adipiscing elit.";
        let err = validate_untrusted_blob(data, true).expect_err("rejected");
        assert_eq!(err.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(err.reason.starts_with("Data not encrypted."));
    }

    #[test]
    fn accepts_when_filter_disabled() {
        assert!(validate_untrusted_blob(&[0u8; 256], false).is_ok());
    }
}
