use std::pin::Pin;

use futures::stream::{self, Stream};

use crate::codec::{decode_tree_node, is_tree_node};
use crate::store::Store;
use crate::types::{Cid, Hash, Link};

use super::{HashTree, HashTreeError};

impl<S: Store> HashTree<S> {
    /// Read content as a stream of chunks by Cid (handles decryption automatically)
    ///
    /// Returns an async stream that yields chunks as they are read.
    /// Useful for large files or when you want to process data incrementally.
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
                            // Single blob - yield decrypted data
                            return Some((Ok(decrypted), ReadStreamState::Done));
                        }

                        // Tree node - parse and traverse
                        let node = match decode_tree_node(&decrypted) {
                            Ok(n) => n,
                            Err(e) => {
                                return Some((Err(HashTreeError::Codec(e)), ReadStreamState::Done))
                            }
                        };

                        let mut stack = node.links.into_iter().rev().collect();

                        tree.process_stream_stack(&mut stack).await
                    }
                    ReadStreamState::Processing { mut stack, tree } => {
                        tree.process_stream_stack(&mut stack).await
                    }
                    ReadStreamState::Done => None,
                }
            },
        ))
    }

    async fn process_stream_stack<'a>(
        &'a self,
        stack: &mut Vec<Link>,
    ) -> Option<(Result<Vec<u8>, HashTreeError>, ReadStreamState<'a, S>)> {
        while let Some(item) = stack.pop() {
            let decrypted = match self.read_link_bytes(&item).await {
                Ok(bytes) => bytes,
                Err(e) => return Some((Err(e), ReadStreamState::Done)),
            };
            match Self::decode_linked_file_node(&item, &decrypted) {
                Ok(Some(node)) => stack.extend(node.links.into_iter().rev()),
                Ok(None) => {
                    return Some((
                        Ok(decrypted),
                        ReadStreamState::Processing {
                            stack: std::mem::take(stack),
                            tree: self,
                        },
                    ))
                }
                Err(e) => return Some((Err(e), ReadStreamState::Done)),
            }
        }
        None
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

// Internal state types for streaming

enum ReadStreamState<'a, S: Store> {
    Init {
        cid: Cid,
        tree: &'a HashTree<S>,
    },
    Processing {
        stack: Vec<Link>,
        tree: &'a HashTree<S>,
    },
    Done,
}
