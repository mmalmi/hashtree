//! Git object types and serialization
//!
//! Git has four object types: blob, tree, commit, and tag.
//! Each is content-addressed by SHA-1 hash of: "{type} {size}\0{content}"

use super::{Error, Result};
use sha1::{Digest, Sha1};
use std::fmt;

/// The four git object types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ObjectType::Blob => "blob",
            ObjectType::Tree => "tree",
            ObjectType::Commit => "commit",
            ObjectType::Tag => "tag",
        }
    }

    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "blob" => Some(ObjectType::Blob),
            "tree" => Some(ObjectType::Tree),
            "commit" => Some(ObjectType::Commit),
            "tag" => Some(ObjectType::Tag),
            _ => None,
        }
    }
}

impl fmt::Display for ObjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A 20-byte SHA-1 object ID
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId([u8; 20]);

impl ObjectId {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() == 20 {
            let mut arr = [0u8; 20];
            arr.copy_from_slice(bytes);
            Some(ObjectId(arr))
        } else {
            None
        }
    }

    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() != 40 {
            return None;
        }
        let bytes = hex::decode(hex).ok()?;
        Self::from_bytes(&bytes)
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Compute object ID from raw object data (type + content)
    pub fn hash_object(obj_type: ObjectType, content: &[u8]) -> Self {
        let header = format!("{} {}\0", obj_type.as_str(), content.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(content);
        let result = hasher.finalize();
        let mut id = [0u8; 20];
        id.copy_from_slice(&result);
        ObjectId(id)
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.to_hex())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// A git object with type and content
#[derive(Debug, Clone)]
pub struct GitObject {
    pub obj_type: ObjectType,
    pub content: Vec<u8>,
}

impl GitObject {
    pub fn new(obj_type: ObjectType, content: Vec<u8>) -> Self {
        Self { obj_type, content }
    }

    pub fn id(&self) -> ObjectId {
        ObjectId::hash_object(self.obj_type, &self.content)
    }

    /// Serialize to loose object format (for storage)
    pub fn to_loose_format(&self) -> Vec<u8> {
        let header = format!("{} {}\0", self.obj_type.as_str(), self.content.len());
        let mut data = header.into_bytes();
        data.extend_from_slice(&self.content);
        data
    }

    /// Parse from loose object format
    #[allow(dead_code)]
    pub fn from_loose_format(data: &[u8]) -> Result<Self> {
        let null_pos = data
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::InvalidObjectFormat("missing null byte".into()))?;

        let header = std::str::from_utf8(&data[..null_pos])
            .map_err(|_| Error::InvalidObjectFormat("invalid header".into()))?;

        let mut parts = header.split(' ');
        let type_str = parts
            .next()
            .ok_or_else(|| Error::InvalidObjectFormat("missing type".into()))?;
        let size_str = parts
            .next()
            .ok_or_else(|| Error::InvalidObjectFormat("missing size".into()))?;

        let obj_type = ObjectType::from_str(type_str)
            .ok_or_else(|| Error::InvalidObjectType(type_str.into()))?;
        let size: usize = size_str
            .parse()
            .map_err(|_| Error::InvalidObjectFormat("invalid size".into()))?;

        let content = data[null_pos + 1..].to_vec();
        if content.len() != size {
            return Err(Error::InvalidObjectFormat(format!(
                "size mismatch: expected {}, got {}",
                size,
                content.len()
            )));
        }

        Ok(Self { obj_type, content })
    }
}

/// Tree entry (mode, name, object id)
#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: String,
    pub oid: ObjectId,
}

impl TreeEntry {
    pub fn is_tree(&self) -> bool {
        self.mode == 0o40000
    }
}

/// Parse tree content into entries
pub fn parse_tree(content: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut pos = 0;

    while pos < content.len() {
        let space_pos = content[pos..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| Error::InvalidObjectFormat("tree: missing space".into()))?;
        let mode_str = std::str::from_utf8(&content[pos..pos + space_pos])
            .map_err(|_| Error::InvalidObjectFormat("tree: invalid mode".into()))?;
        let mode = u32::from_str_radix(mode_str, 8)
            .map_err(|_| Error::InvalidObjectFormat("tree: invalid mode octal".into()))?;
        pos += space_pos + 1;

        let null_pos = content[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::InvalidObjectFormat("tree: missing null".into()))?;
        let name = std::str::from_utf8(&content[pos..pos + null_pos])
            .map_err(|_| Error::InvalidObjectFormat("tree: invalid name".into()))?
            .to_string();
        pos += null_pos + 1;

        if pos + 20 > content.len() {
            return Err(Error::InvalidObjectFormat("tree: truncated sha".into()));
        }
        let oid = ObjectId::from_bytes(&content[pos..pos + 20])
            .ok_or_else(|| Error::InvalidObjectFormat("tree: invalid sha".into()))?;
        pos += 20;

        entries.push(TreeEntry { mode, name, oid });
    }

    Ok(entries)
}

// Test helpers
#[cfg(test)]
pub use test_helpers::*;

#[cfg(test)]
mod test_helpers {
    use super::*;

    impl TreeEntry {
        pub fn new(mode: u32, name: String, oid: ObjectId) -> Self {
            Self { mode, name, oid }
        }
    }

    pub fn serialize_tree(entries: &[TreeEntry]) -> Vec<u8> {
        let mut content = Vec::new();
        for entry in entries {
            content.extend_from_slice(format!("{:o}", entry.mode).as_bytes());
            content.push(b' ');
            content.extend_from_slice(entry.name.as_bytes());
            content.push(0);
            content.extend_from_slice(&entry.oid.0);
        }
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_object_id_hex() {
        let hex = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let oid = ObjectId::from_hex(hex).unwrap();
        assert_eq!(oid.to_hex(), hex);
    }

    #[test]
    fn test_blob_hash() {
        let empty_blob_hash = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        let oid = ObjectId::hash_object(ObjectType::Blob, &[]);
        assert_eq!(oid.to_hex(), empty_blob_hash);
    }

    #[test]
    fn test_hello_world_blob() {
        let content = b"hello world\n";
        let expected = "3b18e512dba79e4c8300dd08aeb37f8e728b8dad";
        let oid = ObjectId::hash_object(ObjectType::Blob, content);
        assert_eq!(oid.to_hex(), expected);
    }

    #[test]
    fn test_loose_format_roundtrip() {
        let obj = GitObject::new(ObjectType::Blob, b"test content".to_vec());
        let loose = obj.to_loose_format();
        let parsed = GitObject::from_loose_format(&loose).unwrap();
        assert_eq!(parsed.obj_type, ObjectType::Blob);
        assert_eq!(parsed.content, b"test content");
    }
}
