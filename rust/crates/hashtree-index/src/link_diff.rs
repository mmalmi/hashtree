//! Bounded logical comparison of CID-link trees with different shapes.
//!
//! An unchanged, identically routed subtree with an unchanged positive count
//! is a Merkle witness from the accepted baseline. All rewritten paths are
//! decoded strictly. Unknown (zero) counts are expanded, never guessed.
use std::collections::BTreeMap;
use std::sync::Arc;

use hashtree_core::{
    decode_tree_node, decrypt_object, encode_tree_node, sha256, Cid, EncryptionMode, Hash,
    LinkType, ObjectKey, Store,
};

use crate::{escape_key, unescape_key, BTreeError};

#[derive(Debug, Clone, Copy)]
pub struct LinkComparisonLimits {
    pub nodes: usize,
    pub bytes: usize,
    pub block_bytes: usize,
    pub depth: usize,
    pub links_per_node: usize,
    pub expanded_links: usize,
}
impl Default for LinkComparisonLimits {
    fn default() -> Self {
        Self {
            nodes: 4096,
            bytes: 16 * 1024 * 1024,
            block_bytes: 2 * 1024 * 1024,
            depth: 64,
            links_per_node: 4096,
            expanded_links: 262_144,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkComparisonReport {
    pub before_count: u64,
    pub after_count: u64,
    pub opaque_subtrees: usize,
}

#[derive(Clone)]
struct Entry {
    key: String,
    cid: Cid,
    count: u64,
}
struct Node {
    entries: Vec<Entry>,
    leaf: bool,
}
#[derive(Clone)]
struct Span {
    cid: Cid,
    lower: Option<String>,
    upper: Option<String>,
    count: u64,
    depth: usize,
}
#[derive(Clone)]
enum Item {
    Node(Span),
    Value(Entry),
    End { start: u64, declared: u64 },
}
struct Cursor {
    stack: Vec<Item>,
    consumed: u64,
}
impl Cursor {
    fn new(cid: &Cid) -> Self {
        Self {
            stack: vec![Item::Node(Span {
                cid: cid.clone(),
                lower: None,
                upper: None,
                count: 0,
                depth: 0,
            })],
            consumed: 0,
        }
    }
    fn peek(&mut self) -> Result<Option<Item>, BTreeError> {
        while let Some(Item::End { start, declared }) = self.stack.last() {
            let actual = self.consumed - start;
            if *declared != 0 && actual != *declared {
                return Err(invalid("rewritten subtree count differs from its entries"));
            }
            self.stack.pop();
        }
        Ok(self.stack.last().cloned())
    }
    fn consume(&mut self, count: u64) -> Result<(), BTreeError> {
        self.stack.pop();
        self.consumed = self
            .consumed
            .checked_add(count)
            .ok_or_else(|| invalid("subtree count overflow"))?;
        Ok(())
    }
}
fn invalid(message: &str) -> BTreeError {
    BTreeError::InvalidLinkTree(message.to_owned())
}

/// Reuse one verifier across all projections so its read/CPU budget is aggregate.
/// This is read-only: it never rebuilds or repairs either tree.
pub struct LinkTreeVerifier<S: Store> {
    store: Arc<S>,
    limits: LinkComparisonLimits,
    cache: BTreeMap<(Hash, Option<ObjectKey>), Arc<Node>>,
    bytes: usize,
    expanded_links: usize,
}
impl<S: Store> LinkTreeVerifier<S> {
    pub fn new(store: Arc<S>, limits: LinkComparisonLimits) -> Self {
        Self {
            store,
            limits,
            cache: BTreeMap::new(),
            bytes: 0,
            expanded_links: 0,
        }
    }

    async fn read_node(&mut self, cid: &Cid) -> Result<Arc<Node>, BTreeError> {
        let key = (cid.hash, cid.key);
        if let Some(node) = self.cache.get(&key) {
            return Ok(node.clone());
        }
        if self.cache.len() >= self.limits.nodes {
            return Err(invalid("comparison node budget exceeded"));
        }
        let bytes = self
            .store
            .get(&cid.hash)
            .await
            .map_err(|err| invalid(&format!("comparison read failed: {err}")))?
            .ok_or_else(|| invalid("comparison node is missing"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| invalid("comparison byte overflow"))?;
        if bytes.len() > self.limits.block_bytes || self.bytes > self.limits.bytes {
            return Err(invalid("comparison byte budget exceeded"));
        }
        if sha256(&bytes) != cid.hash {
            return Err(invalid("comparison node hash mismatch"));
        }
        let plain = match &cid.key {
            Some(key) => {
                let plain = decrypt_object(&bytes, key)
                    .map_err(|_| invalid("comparison decryption failed"))?;
                if matches!(
                    key.mode(),
                    EncryptionMode::LegacyChk | EncryptionMode::ChkV1
                ) && sha256(&plain) != key.raw_key()
                {
                    return Err(invalid("comparison CHK content key mismatch"));
                }
                plain
            }
            None => bytes,
        };
        let node =
            decode_tree_node(&plain).map_err(|_| invalid("comparison node is not a tree"))?;
        if node.node_type != LinkType::Dir || node.links.len() > self.limits.links_per_node {
            return Err(invalid("comparison requires a bounded direct directory"));
        }
        if encode_tree_node(&node).map_err(|_| invalid("comparison encode failed"))? != plain {
            return Err(invalid("comparison node encoding is noncanonical"));
        }
        let leaf = node
            .links
            .iter()
            .all(|entry| entry.link_type == LinkType::File);
        if !leaf
            && !node
                .links
                .iter()
                .all(|entry| entry.link_type == LinkType::Dir)
        {
            return Err(invalid("comparison node mixes internal and leaf types"));
        }
        let mut entries = Vec::with_capacity(node.links.len());
        for link in node.links {
            let name = link
                .name
                .as_ref()
                .ok_or_else(|| invalid("comparison link is unnamed"))?;
            let key = unescape_key(name);
            if name.is_empty()
                || escape_key(&key) != *name
                || link.meta.is_some()
                || (leaf && link.size != 0)
            {
                return Err(invalid(
                    "comparison link key, metadata, or leaf size is noncanonical",
                ));
            }
            entries.push(Entry {
                key,
                cid: link.to_cid(),
                count: link.size,
            });
        }
        // Directory encoders order escaped names; B-tree routing orders decoded
        // keys. Rust string order is UTF-8 byte order, including non-ASCII tags.
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        if entries.windows(2).any(|pair| pair[0].key >= pair[1].key) {
            return Err(invalid("comparison node repeats a decoded key"));
        }
        let node = Arc::new(Node { entries, leaf });
        self.cache.insert((cid.hash, cid.key), node.clone());
        Ok(node)
    }

    async fn expand(&mut self, cursor: &mut Cursor, span: Span) -> Result<(), BTreeError> {
        if span.depth > self.limits.depth {
            return Err(invalid("comparison depth budget exceeded"));
        }
        let node = self.read_node(&span.cid).await?;
        self.expanded_links = self
            .expanded_links
            .checked_add(node.entries.len())
            .ok_or_else(|| invalid("comparison link overflow"))?;
        if self.expanded_links > self.limits.expanded_links {
            return Err(invalid("comparison link budget exceeded"));
        }
        if span.lower.is_some()
            && node.entries.first().map(|entry| &entry.key) != span.lower.as_ref()
        {
            return Err(invalid(
                "comparison child first key differs from routing separator",
            ));
        }
        if node.entries.is_empty() && span.depth != 0 {
            return Err(invalid("comparison child is empty"));
        }
        if node
            .entries
            .iter()
            .any(|entry| span.upper.as_ref().is_some_and(|upper| &entry.key >= upper))
        {
            return Err(invalid("comparison child exceeds routing range"));
        }
        cursor.stack.pop();
        cursor.stack.push(Item::End {
            start: cursor.consumed,
            declared: span.count,
        });
        for (index, entry) in node.entries.iter().enumerate().rev() {
            cursor.stack.push(if node.leaf {
                Item::Value(entry.clone())
            } else {
                Item::Node(Span {
                    cid: entry.cid.clone(),
                    lower: Some(entry.key.clone()),
                    upper: node
                        .entries
                        .get(index + 1)
                        .map(|next| next.key.clone())
                        .or_else(|| span.upper.clone()),
                    count: entry.count,
                    depth: span.depth + 1,
                })
            });
        }
        Ok(())
    }

    /// Require identical logical mappings, or exactly one specified absent→CID
    /// insertion. No deletions, replacement values, or additional keys pass.
    pub async fn compare(
        &mut self,
        before: &Cid,
        after: &Cid,
        insertion: Option<(&str, &Cid)>,
    ) -> Result<LinkComparisonReport, BTreeError> {
        let mut old = Cursor::new(before);
        let mut new = Cursor::new(after);
        let mut inserted = false;
        let mut opaque_subtrees = 0;
        loop {
            let left = old.peek()?;
            let right = new.peek()?;
            match (left, right) {
                (None, None) => break,
                (Some(Item::Node(a)), Some(Item::Node(b))) => {
                    // A shared node is still part of both new routing paths;
                    // pruning must not bypass their structural depth bound.
                    if a.depth > self.limits.depth || b.depth > self.limits.depth {
                        return Err(invalid("comparison depth budget exceeded"));
                    }
                    if a.cid == b.cid
                        && a.lower == b.lower
                        && a.upper == b.upper
                        && a.count != 0
                        && a.count == b.count
                    {
                        old.consume(a.count)?;
                        new.consume(b.count)?;
                        opaque_subtrees += 1;
                    } else {
                        // Expand the wider next interval first so a split or
                        // merged level can still align identical descendants.
                        match (&a.upper, &b.upper) {
                            _ if a.lower != b.lower => {
                                self.expand(&mut old, a).await?;
                                self.expand(&mut new, b).await?;
                            }
                            (None, Some(_)) => self.expand(&mut old, a).await?,
                            (Some(_), None) => self.expand(&mut new, b).await?,
                            (Some(x), Some(y)) if x > y => self.expand(&mut old, a).await?,
                            (Some(x), Some(y)) if y > x => self.expand(&mut new, b).await?,
                            _ => {
                                self.expand(&mut old, a).await?;
                                self.expand(&mut new, b).await?;
                            }
                        }
                    }
                }
                (Some(Item::Node(a)), _) => self.expand(&mut old, a).await?,
                (_, Some(Item::Node(b))) => self.expand(&mut new, b).await?,
                (Some(Item::Value(a)), Some(Item::Value(b)))
                    if a.key == b.key && a.cid == b.cid =>
                {
                    old.consume(1)?;
                    new.consume(1)?;
                }
                (left, Some(Item::Value(value))) => {
                    let comes_first = match left {
                        None => true,
                        Some(Item::Value(ref old)) => value.key < old.key,
                        _ => unreachable!(),
                    };
                    if !inserted
                        && comes_first
                        && insertion.is_some_and(|(key, cid)| value.key == key && &value.cid == cid)
                    {
                        new.consume(1)?;
                        inserted = true;
                    } else {
                        return Err(invalid("comparison changed an unexpected mapping"));
                    }
                }
                _ => return Err(invalid("comparison deleted a mapping")),
            }
        }
        let expected_delta = u64::from(insertion.is_some());
        if inserted != insertion.is_some()
            || old.consumed.checked_add(expected_delta) != Some(new.consumed)
        {
            return Err(invalid(
                "comparison did not make exactly the requested insertion",
            ));
        }
        Ok(LinkComparisonReport {
            before_count: old.consumed,
            after_count: new.consumed,
            opaque_subtrees,
        })
    }
}
