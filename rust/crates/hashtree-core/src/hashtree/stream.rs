use std::pin::Pin;

use futures::stream::{self, Stream};

use crate::codec::{decode_tree_node, is_tree_node};
use crate::crypto::{decrypt_chk, EncryptionKey};
use crate::store::Store;
use crate::types::{to_hex, Cid, Hash, Link};

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
        let hash = cid.hash;
        let key = cid.key;

        if let Some(k) = key {
            // Encrypted stream
            Box::pin(self.read_file_stream_encrypted(hash, k))
        } else {
            // Unencrypted stream
            self.read_file_stream(hash)
        }
    }

    /// Read encrypted file as stream (internal)
    fn read_file_stream_encrypted(
        &self,
        hash: Hash,
        key: EncryptionKey,
    ) -> impl Stream<Item = Result<Vec<u8>, HashTreeError>> + Send + '_ {
        stream::unfold(
            EncryptedStreamState::Init {
                hash,
                key,
                tree: self,
            },
            |state| async move {
                match state {
                    EncryptedStreamState::Init { hash, key, tree } => {
                        let data = match tree.store.get(&hash).await {
                            Ok(Some(d)) => d,
                            Ok(None) => return None,
                            Err(e) => {
                                return Some((
                                    Err(HashTreeError::Store(e.to_string())),
                                    EncryptedStreamState::Done,
                                ))
                            }
                        };

                        // Try to decrypt
                        let decrypted = match decrypt_chk(&data, &key) {
                            Ok(d) => d,
                            Err(e) => {
                                return Some((
                                    Err(HashTreeError::Decryption(e.to_string())),
                                    EncryptedStreamState::Done,
                                ))
                            }
                        };

                        if !is_tree_node(&decrypted) {
                            // Single blob - yield decrypted data
                            return Some((Ok(decrypted), EncryptedStreamState::Done));
                        }

                        // Tree node - parse and traverse
                        let node = match decode_tree_node(&decrypted) {
                            Ok(n) => n,
                            Err(e) => {
                                return Some((
                                    Err(HashTreeError::Codec(e)),
                                    EncryptedStreamState::Done,
                                ))
                            }
                        };

                        let mut stack = node.links.into_iter().rev().collect();

                        tree.process_encrypted_stream_stack(&mut stack).await
                    }
                    EncryptedStreamState::Processing { mut stack, tree } => {
                        tree.process_encrypted_stream_stack(&mut stack).await
                    }
                    EncryptedStreamState::Done => None,
                }
            },
        )
    }

    async fn process_encrypted_stream_stack<'a>(
        &'a self,
        stack: &mut Vec<Link>,
    ) -> Option<(Result<Vec<u8>, HashTreeError>, EncryptedStreamState<'a, S>)> {
        while let Some(item) = stack.pop() {
            let data = match self.store.get(&item.hash).await {
                Ok(Some(d)) => d,
                Ok(None) => {
                    return Some((
                        Err(HashTreeError::MissingChunk(to_hex(&item.hash))),
                        EncryptedStreamState::Done,
                    ))
                }
                Err(e) => {
                    return Some((
                        Err(HashTreeError::Store(e.to_string())),
                        EncryptedStreamState::Done,
                    ))
                }
            };

            // Decrypt if we have a key
            let decrypted = if let Some(key) = item.key {
                match decrypt_chk(&data, &key) {
                    Ok(d) => d,
                    Err(e) => {
                        return Some((
                            Err(HashTreeError::Decryption(e.to_string())),
                            EncryptedStreamState::Done,
                        ))
                    }
                }
            } else {
                data
            };

            match Self::decode_linked_file_node(&item, &decrypted) {
                Ok(Some(node)) => stack.extend(node.links.into_iter().rev()),
                Ok(None) => {
                    return Some((
                        Ok(decrypted),
                        EncryptedStreamState::Processing {
                            stack: std::mem::take(stack),
                            tree: self,
                        },
                    ))
                }
                Err(e) => return Some((Err(e), EncryptedStreamState::Done)),
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
        Box::pin(stream::unfold(
            ReadStreamState::Init { hash, tree: self },
            |state| async move {
                match state {
                    ReadStreamState::Init { hash, tree } => {
                        let data = match tree.store.get(&hash).await {
                            Ok(Some(d)) => d,
                            Ok(None) => return None,
                            Err(e) => {
                                return Some((
                                    Err(HashTreeError::Store(e.to_string())),
                                    ReadStreamState::Done,
                                ))
                            }
                        };

                        if !is_tree_node(&data) {
                            // Single blob - yield it and finish
                            return Some((Ok(data), ReadStreamState::Done));
                        }

                        // Tree node - start streaming chunks
                        let node = match decode_tree_node(&data) {
                            Ok(n) => n,
                            Err(e) => {
                                return Some((Err(HashTreeError::Codec(e)), ReadStreamState::Done))
                            }
                        };

                        // Create stack with all links to process
                        let mut stack = node.links.into_iter().rev().collect();

                        // Process first item
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
            let data = match self.store.get(&item.hash).await {
                Ok(Some(d)) => d,
                Ok(None) => {
                    return Some((
                        Err(HashTreeError::MissingChunk(to_hex(&item.hash))),
                        ReadStreamState::Done,
                    ))
                }
                Err(e) => {
                    return Some((
                        Err(HashTreeError::Store(e.to_string())),
                        ReadStreamState::Done,
                    ))
                }
            };

            match Self::decode_linked_file_node(&item, &data) {
                Ok(Some(node)) => stack.extend(node.links.into_iter().rev()),
                Ok(None) => {
                    return Some((
                        Ok(data),
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
}

// Internal state types for streaming

enum ReadStreamState<'a, S: Store> {
    Init {
        hash: Hash,
        tree: &'a HashTree<S>,
    },
    Processing {
        stack: Vec<Link>,
        tree: &'a HashTree<S>,
    },
    Done,
}

// Encrypted stream state types
enum EncryptedStreamState<'a, S: Store> {
    Init {
        hash: Hash,
        key: [u8; 32],
        tree: &'a HashTree<S>,
    },
    Processing {
        stack: Vec<Link>,
        tree: &'a HashTree<S>,
    },
    Done,
}
