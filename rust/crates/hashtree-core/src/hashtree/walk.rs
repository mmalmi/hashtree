use super::*;

impl<S: Store> HashTree<S> {
    fn decode_node_or_blob(data: &[u8]) -> Result<Option<TreeNode>, HashTreeError> {
        match decode_tree_node(data) {
            Ok(node) => Ok(Some(node)),
            Err(err) if is_tree_node(data) => Err(HashTreeError::Codec(err)),
            Err(_) => Ok(None),
        }
    }

    /// Walk entire tree depth-first (returns Vec)
    pub async fn walk(&self, cid: &Cid, path: &str) -> Result<Vec<WalkEntry>, HashTreeError> {
        let mut entries = Vec::new();
        self.walk_recursive(cid, path, &mut entries).await?;
        Ok(entries)
    }

    async fn walk_recursive(
        &self,
        cid: &Cid,
        path: &str,
        entries: &mut Vec<WalkEntry>,
    ) -> Result<(), HashTreeError> {
        let data = match self
            .store
            .get(&cid.hash)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?
        {
            Some(d) => d,
            None => return Ok(()),
        };

        // Decrypt if key is present
        let data = if let Some(key) = &cid.key {
            decrypt_chk(&data, key).map_err(|e| HashTreeError::Decryption(e.to_string()))?
        } else {
            data
        };

        let node = match Self::decode_node_or_blob(&data)? {
            Some(node) => node,
            None => {
                entries.push(WalkEntry {
                    path: path.to_string(),
                    hash: cid.hash,
                    link_type: LinkType::Blob,
                    size: data.len() as u64,
                    key: cid.key,
                });
                return Ok(());
            }
        };

        let node_size: u64 = node.links.iter().map(|l| l.size).sum();
        entries.push(WalkEntry {
            path: path.to_string(),
            hash: cid.hash,
            link_type: node.node_type,
            size: node_size,
            key: cid.key,
        });

        for link in &node.links {
            let child_path = match &link.name {
                Some(name) => {
                    if Self::is_internal_directory_link(&node, link) {
                        let sub_cid = Cid {
                            hash: link.hash,
                            key: link.key,
                        };
                        Box::pin(self.walk_recursive(&sub_cid, path, entries)).await?;
                        continue;
                    }
                    if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{}/{}", path, name)
                    }
                }
                None => path.to_string(),
            };

            // Child nodes use their own key from link
            let child_cid = Cid {
                hash: link.hash,
                key: link.key,
            };
            Box::pin(self.walk_recursive(&child_cid, &child_path, entries)).await?;
        }

        Ok(())
    }

    /// Walk entire tree with parallel fetching
    /// Uses a work-stealing approach: always keeps `concurrency` requests in flight
    pub async fn walk_parallel(
        &self,
        cid: &Cid,
        path: &str,
        concurrency: usize,
    ) -> Result<Vec<WalkEntry>, HashTreeError> {
        self.walk_parallel_with_progress(cid, path, concurrency, None)
            .await
    }

    /// Walk entire tree with parallel fetching and optional progress counter
    /// The counter is incremented for each node fetched (not just entries found)
    ///
    /// OPTIMIZATION: Blobs are NOT fetched - their metadata (hash, size, link_type)
    /// comes from the parent node's link, so we just add them directly to entries.
    /// This avoids downloading file contents during tree traversal.
    pub async fn walk_parallel_with_progress(
        &self,
        cid: &Cid,
        path: &str,
        concurrency: usize,
        progress: Option<&std::sync::atomic::AtomicUsize>,
    ) -> Result<Vec<WalkEntry>, HashTreeError> {
        use futures::stream::{FuturesUnordered, StreamExt};
        use std::collections::VecDeque;
        use std::sync::atomic::Ordering;

        let mut entries = Vec::new();
        let mut pending: VecDeque<(Cid, String)> = VecDeque::new();
        let mut active = FuturesUnordered::new();

        // Seed with root
        pending.push_back((cid.clone(), path.to_string()));

        loop {
            // Fill up to concurrency limit from pending queue
            while active.len() < concurrency {
                if let Some((node_cid, node_path)) = pending.pop_front() {
                    let store = &self.store;
                    let fut = async move {
                        let data = store
                            .get(&node_cid.hash)
                            .await
                            .map_err(|e| HashTreeError::Store(e.to_string()))?;
                        Ok::<_, HashTreeError>((node_cid, node_path, data))
                    };
                    active.push(fut);
                } else {
                    break;
                }
            }

            // If nothing active, we're done
            if active.is_empty() {
                break;
            }

            // Wait for any future to complete
            if let Some(result) = active.next().await {
                let (node_cid, node_path, data) = result?;

                // Update progress counter
                if let Some(counter) = progress {
                    counter.fetch_add(1, Ordering::Relaxed);
                }

                let data = match data {
                    Some(d) => d,
                    None => continue,
                };

                // Decrypt if key is present
                let data = if let Some(key) = &node_cid.key {
                    decrypt_chk(&data, key).map_err(|e| {
                        HashTreeError::Decryption(format!(
                            "{} at path '{}' hash {} key {}",
                            e,
                            node_path,
                            hex::encode(node_cid.hash),
                            hex::encode(key)
                        ))
                    })?
                } else {
                    data
                };

                let node = match Self::decode_node_or_blob(&data)? {
                    Some(node) => node,
                    None => {
                        // It's a blob/file - this case only happens for root
                        entries.push(WalkEntry {
                            path: node_path,
                            hash: node_cid.hash,
                            link_type: LinkType::Blob,
                            size: data.len() as u64,
                            key: node_cid.key,
                        });
                        continue;
                    }
                };

                // It's a directory/file node
                let node_size: u64 = node.links.iter().map(|l| l.size).sum();
                entries.push(WalkEntry {
                    path: node_path.clone(),
                    hash: node_cid.hash,
                    link_type: node.node_type,
                    size: node_size,
                    key: node_cid.key,
                });

                // Queue children - but DON'T fetch blobs, just add them directly
                for link in &node.links {
                    let child_path = match &link.name {
                        Some(name) => {
                            if Self::is_internal_directory_link(&node, link) {
                                let sub_cid = Cid {
                                    hash: link.hash,
                                    key: link.key,
                                };
                                pending.push_back((sub_cid, node_path.clone()));
                                continue;
                            }
                            if node_path.is_empty() {
                                name.clone()
                            } else {
                                format!("{}/{}", node_path, name)
                            }
                        }
                        None => node_path.clone(),
                    };

                    // OPTIMIZATION: If it's a blob, add entry directly without fetching
                    // The link already contains all the metadata we need
                    if link.link_type == LinkType::Blob {
                        entries.push(WalkEntry {
                            path: child_path,
                            hash: link.hash,
                            link_type: LinkType::Blob,
                            size: link.size,
                            key: link.key,
                        });
                        if let Some(counter) = progress {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                        continue;
                    }

                    // For tree nodes (File/Dir), we need to fetch to see their children
                    let child_cid = Cid {
                        hash: link.hash,
                        key: link.key,
                    };
                    pending.push_back((child_cid, child_path));
                }
            }
        }

        Ok(entries)
    }

    /// Walk tree as stream
    pub fn walk_stream(
        &self,
        cid: Cid,
        initial_path: String,
    ) -> Pin<Box<dyn Stream<Item = Result<WalkEntry, HashTreeError>> + Send + '_>> {
        Box::pin(stream::unfold(
            WalkStreamState::Init {
                cid,
                path: initial_path,
                tree: self,
            },
            |state| async move {
                match state {
                    WalkStreamState::Init { cid, path, tree } => {
                        let data = match tree.store.get(&cid.hash).await {
                            Ok(Some(d)) => d,
                            Ok(None) => return None,
                            Err(e) => {
                                return Some((
                                    Err(HashTreeError::Store(e.to_string())),
                                    WalkStreamState::Done,
                                ))
                            }
                        };

                        // Decrypt if key is present
                        let data = if let Some(key) = &cid.key {
                            match decrypt_chk(&data, key) {
                                Ok(d) => d,
                                Err(e) => {
                                    return Some((
                                        Err(HashTreeError::Decryption(format!(
                                            "{} at path '{}' hash {} key {}",
                                            e,
                                            path,
                                            hex::encode(cid.hash),
                                            hex::encode(key)
                                        ))),
                                        WalkStreamState::Done,
                                    ))
                                }
                            }
                        } else {
                            data
                        };

                        let node = match Self::decode_node_or_blob(&data) {
                            Ok(Some(node)) => node,
                            Ok(None) => {
                                // Blob data
                                let entry = WalkEntry {
                                    path,
                                    hash: cid.hash,
                                    link_type: LinkType::Blob,
                                    size: data.len() as u64,
                                    key: cid.key,
                                };
                                return Some((Ok(entry), WalkStreamState::Done));
                            }
                            Err(err) => return Some((Err(err), WalkStreamState::Done)),
                        };

                        let node_size: u64 = node.links.iter().map(|l| l.size).sum();
                        let entry = WalkEntry {
                            path: path.clone(),
                            hash: cid.hash,
                            link_type: node.node_type,
                            size: node_size,
                            key: cid.key,
                        };

                        // Create stack with children to process
                        let mut stack: Vec<WalkStackItem> = Vec::new();
                        for link in node.links.iter().rev() {
                            let is_internal = Self::is_internal_directory_link(&node, link);
                            let child_path = match &link.name {
                                Some(name) if !is_internal => {
                                    if path.is_empty() {
                                        name.clone()
                                    } else {
                                        format!("{}/{}", path, name)
                                    }
                                }
                                _ => path.clone(),
                            };
                            // Child nodes use their own key from link
                            stack.push(WalkStackItem {
                                hash: link.hash,
                                path: child_path,
                                key: link.key,
                            });
                        }

                        Some((Ok(entry), WalkStreamState::Processing { stack, tree }))
                    }
                    WalkStreamState::Processing { mut stack, tree } => {
                        tree.process_walk_stack(&mut stack).await
                    }
                    WalkStreamState::Done => None,
                }
            },
        ))
    }

    async fn process_walk_stack<'a>(
        &'a self,
        stack: &mut Vec<WalkStackItem>,
    ) -> Option<(Result<WalkEntry, HashTreeError>, WalkStreamState<'a, S>)> {
        while let Some(item) = stack.pop() {
            let data = match self.store.get(&item.hash).await {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                Err(e) => {
                    return Some((
                        Err(HashTreeError::Store(e.to_string())),
                        WalkStreamState::Done,
                    ))
                }
            };

            let data = if let Some(key) = &item.key {
                match decrypt_chk(&data, key) {
                    Ok(d) => d,
                    Err(e) => {
                        return Some((
                            Err(HashTreeError::Decryption(format!(
                                "{} at path '{}' hash {} key {}",
                                e,
                                item.path,
                                hex::encode(item.hash),
                                hex::encode(key)
                            ))),
                            WalkStreamState::Done,
                        ))
                    }
                }
            } else {
                data
            };

            let node = match Self::decode_node_or_blob(&data) {
                Ok(Some(node)) => node,
                Ok(None) => {
                    // Blob data
                    let entry = WalkEntry {
                        path: item.path,
                        hash: item.hash,
                        link_type: LinkType::Blob,
                        size: data.len() as u64,
                        key: item.key,
                    };
                    return Some((
                        Ok(entry),
                        WalkStreamState::Processing {
                            stack: std::mem::take(stack),
                            tree: self,
                        },
                    ));
                }
                Err(err) => return Some((Err(err), WalkStreamState::Done)),
            };

            let node_size: u64 = node.links.iter().map(|l| l.size).sum();
            let entry = WalkEntry {
                path: item.path.clone(),
                hash: item.hash,
                link_type: node.node_type,
                size: node_size,
                key: item.key,
            };

            // Push children to stack
            for link in node.links.iter().rev() {
                let is_internal = Self::is_internal_directory_link(&node, link);
                let child_path = match &link.name {
                    Some(name) if !is_internal => {
                        if item.path.is_empty() {
                            name.clone()
                        } else {
                            format!("{}/{}", item.path, name)
                        }
                    }
                    _ => item.path.clone(),
                };
                stack.push(WalkStackItem {
                    hash: link.hash,
                    path: child_path,
                    key: link.key,
                });
            }

            return Some((
                Ok(entry),
                WalkStreamState::Processing {
                    stack: std::mem::take(stack),
                    tree: self,
                },
            ));
        }
        None
    }
}

struct WalkStackItem {
    hash: Hash,
    path: String,
    key: Option<[u8; 32]>,
}

enum WalkStreamState<'a, S: Store> {
    Init {
        cid: Cid,
        path: String,
        tree: &'a HashTree<S>,
    },
    Processing {
        stack: Vec<WalkStackItem>,
        tree: &'a HashTree<S>,
    },
    Done,
}
