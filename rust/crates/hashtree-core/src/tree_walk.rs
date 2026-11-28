//! Interpret links in stored trees while distinguishing file content from structural nodes.

use crate::{
    decode_tree_node_by_cid, decrypt_object, encrypted_size, encrypted_size_chk, Cid,
    EncryptionMode, HashTreeError, Link, LinkType, TreeNode,
};
use std::borrow::Cow;

/// Resolve a child CID, including keys inherited by historical chunk and fanout links.
pub fn child_cid(parent: &Cid, link: &Link) -> Cid {
    let inherits_parent_key = link
        .name
        .as_deref()
        .map(|name| {
            name.starts_with("_chunk_")
                || (name.starts_with('_') && name.chars().count() == 2 && link.link_type.is_tree())
        })
        .unwrap_or(false);

    Cid {
        hash: link.hash,
        key: link.key.or(if inherits_parent_key {
            parent.key
        } else {
            None
        }),
    }
}

/// A directory entry starts a file, while a Blob child within a File may be
/// literal bytes even when those bytes are themselves a valid manifest.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WalkContext {
    Node,
    Directory,
    DirectoryBlob(u64),
    FileChunk(u64),
}

impl WalkContext {
    pub fn for_link(parent_type: LinkType, link: &Link) -> Self {
        match (parent_type, link.link_type) {
            (LinkType::File, LinkType::Blob) => Self::FileChunk(link.size),
            (_, LinkType::Dir | LinkType::Fanout) => Self::Directory,
            (_, LinkType::Blob) => Self::DirectoryBlob(link.size),
            _ => Self::Node,
        }
    }

    /// A file-shaped directory link can store the bytes of a separate directory node.
    /// File entries with identical bytes keep their ordinary file interpretation.
    pub fn is_wrapped_directory(self, node: &TreeNode) -> bool {
        self == Self::Directory && node.is_file()
    }

    pub fn is_literal_file_chunk(self, cid: &Cid, stored_len: u64) -> bool {
        let Self::FileChunk(size) = self else {
            return false;
        };
        let overhead = match cid.key.map(|key| key.mode()) {
            None => 0,
            Some(EncryptionMode::LegacyChk | EncryptionMode::ChkV1) => encrypted_size_chk(0) as u64,
            Some(EncryptionMode::RndV1) => encrypted_size(0) as u64,
        };
        size.checked_add(overhead) == Some(stored_len)
    }

    pub fn decode(self, cid: &Cid, data: Cow<'_, [u8]>) -> Result<Option<TreeNode>, HashTreeError> {
        let size = match self {
            Self::Node => return decode_tree_node_by_cid(cid, data.into_owned()),
            Self::Directory => {
                let node = decode_tree_node_by_cid(cid, data.into_owned())?.ok_or_else(|| {
                    HashTreeError::Store(format!(
                        "expected a directory node at {}",
                        crate::to_hex(&cid.hash)
                    ))
                })?;
                // list_directory_required historically yields no entries for a
                // zero-link File wrapper. There are no physical chunks to assemble.
                if node.is_file() && node.links.is_empty() {
                    return Ok(Some(TreeNode::dir(Vec::new())));
                }
                return Ok(Some(node));
            }
            Self::DirectoryBlob(size) | Self::FileChunk(size) => size,
        };
        let plaintext = match cid.key {
            Some(key) => match decrypt_object(&data, &key) {
                Ok(bytes) => Cow::Owned(bytes),
                // Historical plaintext trees may carry a legacy decryption key.
                Err(_) if key.mode() == EncryptionMode::LegacyChk && crate::is_tree_node(&data) => {
                    data
                }
                Err(err) => return Err(HashTreeError::Decryption(err.to_string())),
            },
            None => data,
        };
        if plaintext.len() as u64 == size {
            if matches!(self, Self::FileChunk(_)) {
                return Ok(None);
            }
            // An equal-length directory entry can still be a chunked file.
            // Raw MessagePack files may also contain valid but unrelated links.
            return Ok(crate::try_decode_tree_node(&plaintext).filter(|node| {
                node.is_file()
                    && node
                        .links
                        .iter()
                        .try_fold(0_u64, |total, link| total.checked_add(link.size))
                        == Some(size)
            }));
        }
        if matches!(self, Self::FileChunk(_)) {
            return Ok(crate::try_decode_tree_node(&plaintext).filter(TreeNode::is_file));
        }
        Ok(decode_tree_node_by_cid(
            &Cid::public(cid.hash),
            plaintext.into_owned(),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_directories_require_a_node_and_preserve_empty_encodings() {
        let read = |context: WalkContext, bytes: &[u8]| {
            context.decode(&Cid::public(crate::sha256(bytes)), Cow::Borrowed(bytes))
        };
        let literal = b"ordinary file contents";
        assert!(
            read(WalkContext::DirectoryBlob(literal.len() as u64), literal)
                .unwrap()
                .is_none()
        );
        for link_type in [LinkType::Dir, LinkType::Fanout] {
            let mut link = Link::new(crate::sha256(literal));
            link.link_type = link_type;
            let context = WalkContext::for_link(LinkType::Dir, &link);
            assert!(read(context, literal).is_err());
            let invalid = rmp_serde::to_vec(&(Vec::<u8>::new(), 42u8)).unwrap();
            assert!(read(context, &invalid).is_err());
            let empty_dir = crate::encode_tree_node(&TreeNode::dir(Vec::new())).unwrap();
            let empty_file_wrapper = crate::encode_tree_node(&TreeNode::file(Vec::new())).unwrap();
            // Legacy compact encoding uses the wire field order [links, type].
            let compact_dir = rmp_serde::to_vec(&(Vec::<u8>::new(), LinkType::Dir as u8)).unwrap();
            for bytes in [empty_dir, empty_file_wrapper, compact_dir] {
                let dir = read(context, &bytes).unwrap().unwrap();
                assert!(dir.is_dir() && dir.links.is_empty());
            }
        }
    }
}
