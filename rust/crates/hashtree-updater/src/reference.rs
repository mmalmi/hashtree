use crate::error::UpdateError;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateRef {
    pub npub: String,
    pub tree_name: String,
    pub path: Option<String>,
}

impl UpdateRef {
    pub fn parse(input: &str) -> Result<Self, UpdateError> {
        let input = input.strip_prefix("htree://").unwrap_or(input);
        let input = input.split('#').next().unwrap_or(input);
        let input = input.split('?').next().unwrap_or(input).trim_matches('/');

        if !input.starts_with("npub1") {
            return Err(UpdateError::InvalidReference(
                "expected npub/path or htree://npub/path".to_string(),
            ));
        }

        let mut parts = input.split('/');
        let npub = parts
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| UpdateError::InvalidReference("missing npub".to_string()))?;
        let tree_name = parts
            .next()
            .map(decode_segment)
            .filter(|part| !part.is_empty())
            .ok_or_else(|| UpdateError::InvalidReference("missing tree name".to_string()))?;
        let path_parts = parts.map(decode_segment).collect::<Vec<_>>();

        Ok(Self {
            npub: npub.to_string(),
            tree_name,
            path: (!path_parts.is_empty()).then(|| path_parts.join("/")),
        })
    }

    pub fn resolver_key(&self) -> String {
        format!("{}/{}", self.npub, self.tree_name)
    }
}

fn decode_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hi = bytes[index + 1] as char;
            let lo = bytes[index + 2] as char;
            if let (Some(hi), Some(lo)) = (hi.to_digit(16), lo.to_digit(16)) {
                decoded.push(((hi << 4) | lo) as u8);
                index += 3;
                continue;
            }
        }

        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(decoded).unwrap_or_else(|_| segment.to_string())
}
