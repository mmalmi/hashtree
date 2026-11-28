use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;

use futures::stream::{self, FuturesOrdered, Stream, StreamExt};

use crate::codec::{decode_tree_node, is_tree_node};
use crate::store::Store;
use crate::types::{Cid, Hash, Link, LinkType};

use super::{HashTree, HashTreeError};

/// Bounded ordered read-ahead for streamed chunked files.
const STREAM_READ_AHEAD_CHUNKS: usize = 8;
const STREAM_READ_AHEAD_BYTES: u64 = 8 * 1024 * 1024;

type ReadLinkFuture<'a> =
    Pin<Box<dyn Future<Output = (Link, Result<Vec<u8>, HashTreeError>)> + Send + 'a>>;

impl<S: Store> HashTree<S> {
    /// Read content as a stream of chunks by Cid (handles decryption automatically).
    ///
    /// Prefetches bounded upcoming leaf chunks while yielding bytes strictly in
    /// file order and canceling pending work on error or stream drop.
    pub fn get_stream(
        &self,
        cid: &Cid,
    ) -> Pin<Box<dyn Stream<Item = Result<Vec<u8>, HashTreeError>> + Send + '_>> {
        Box::pin(stream::unfold(
            ReadStreamState::Init {
                cid: cid.clone(),
                tree: self,
            },
            |state| async move {
                match state {
                    ReadStreamState::Init { cid, tree } => {
                        let decrypted = match tree.get_cid_root_bytes(&cid).await {
                            Ok(Some(data)) => data,
                            Ok(None) => return None,
                            Err(e) => return Some((Err(e), ReadStreamState::Done)),
                        };

                        if !is_tree_node(&decrypted) {
                            return Some((Ok(decrypted), ReadStreamState::Done));
                        }

                        let node = match decode_tree_node(&decrypted) {
                            Ok(n) => n,
                            Err(e) => {
                                return Some((Err(HashTreeError::Codec(e)), ReadStreamState::Done))
                            }
                        };

                        let mut reader = OrderedStreamReader {
                            tree,
                            stack: node.links.into_iter().rev().collect(),
                            in_flight: FuturesOrdered::new(),
                            in_flight_links: VecDeque::new(),
                            in_flight_bytes: 0,
                        };
                        match reader.next_chunk().await {
                            Some(Ok(chunk)) => {
                                Some((Ok(chunk), ReadStreamState::Processing(reader)))
                            }
                            Some(Err(e)) => Some((Err(e), ReadStreamState::Done)),
                            None => None,
                        }
                    }
                    ReadStreamState::Processing(mut reader) => match reader.next_chunk().await {
                        Some(Ok(chunk)) => Some((Ok(chunk), ReadStreamState::Processing(reader))),
                        Some(Err(e)) => Some((Err(e), ReadStreamState::Done)),
                        None => None,
                    },
                    ReadStreamState::Done => None,
                }
            },
        ))
    }

    /// Read a file as stream of chunks
    /// Returns an async stream that yields chunks as they are read
    pub fn read_file_stream(
        &self,
        hash: Hash,
    ) -> Pin<Box<dyn Stream<Item = Result<Vec<u8>, HashTreeError>> + Send + '_>> {
        self.get_stream(&Cid::public(hash))
    }
}

struct OrderedStreamReader<'a, S: Store> {
    tree: &'a HashTree<S>,
    stack: Vec<Link>,
    in_flight: FuturesOrdered<ReadLinkFuture<'a>>,
    in_flight_links: VecDeque<Link>,
    in_flight_bytes: u64,
}

impl<'a, S: Store> OrderedStreamReader<'a, S> {
    fn fill_prefetch(&mut self) {
        while self.in_flight.len() < STREAM_READ_AHEAD_CHUNKS {
            let Some(next) = self.stack.last() else {
                break;
            };
            let allow = self.in_flight.is_empty()
                || (next.link_type == LinkType::Blob
                    // Descend into a known subtree before fetching its siblings.
                    && self.in_flight_links.back().is_some_and(|link| link.link_type == LinkType::Blob)
                    && self.in_flight_bytes.saturating_add(next.size) <= STREAM_READ_AHEAD_BYTES);
            if !allow {
                break;
            }
            let link = self.stack.pop().expect("checked non-empty stack");
            self.in_flight_bytes = self.in_flight_bytes.saturating_add(link.size);
            self.in_flight_links.push_back(link.clone());
            let tree = self.tree;
            self.in_flight.push_back(Box::pin(async move {
                let res = tree.read_link_bytes(&link).await;
                (link, res)
            }));
        }
    }

    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, HashTreeError>> {
        loop {
            self.fill_prefetch();
            let (link, result) = self.in_flight.next().await?;
            let _ = self.in_flight_links.pop_front();
            self.in_flight_bytes = self.in_flight_bytes.saturating_sub(link.size);

            let decrypted = match result {
                Ok(bytes) => bytes,
                Err(e) => return Some(Err(e)),
            };
            match HashTree::<S>::decode_linked_file_node(&link, &decrypted) {
                Ok(Some(node)) => {
                    // Preserve exact file order if a legacy Blob link expanded into a subtree.
                    if !self.in_flight_links.is_empty() {
                        self.in_flight = FuturesOrdered::new();
                        self.in_flight_bytes = 0;
                        while let Some(sibling) = self.in_flight_links.pop_back() {
                            self.stack.push(sibling);
                        }
                    }
                    self.stack.extend(node.links.into_iter().rev());
                }
                Ok(None) => {
                    self.fill_prefetch();
                    return Some(Ok(decrypted));
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

enum ReadStreamState<'a, S: Store> {
    Init { cid: Cid, tree: &'a HashTree<S> },
    Processing(OrderedStreamReader<'a, S>),
    Done,
}
