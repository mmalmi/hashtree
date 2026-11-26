//! Required multi-key lookup: visit each relevant immutable branch once.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const PARALLEL_READ_MIN_KEYS: usize = 32;

enum LookupStep {
    Found(BTreeMap<String, Cid>),
    Children(Vec<(Cid, usize, usize)>),
}

impl<S: Store> BTree<S> {
    async fn lookup_step(
        &self,
        root: &Cid,
        keys: &[String],
        required: bool,
    ) -> Result<LookupStep, BTreeError> {
        let entries = sort_entries(if required {
            self.tree.list_directory_required(root).await?
        } else {
            self.tree.list_directory(root).await?
        });
        if required {
            validate_required_link_node(&entries)?;
        }
        if is_leaf_node(&entries) {
            let links = entries
                .into_iter()
                .map(|entry| (unescape_key(&entry.name), entry_cid(&entry)))
                .collect::<BTreeMap<_, _>>();
            return Ok(LookupStep::Found(
                keys.iter()
                    .filter_map(|key| links.get(key).cloned().map(|cid| (key.clone(), cid)))
                    .collect(),
            ));
        }
        let mut work = Vec::new();
        let mut start = 0;
        for (index, entry) in entries.iter().enumerate() {
            let end = entries
                .get(index + 1)
                .map(|next| {
                    let next_key = unescape_key(&next.name);
                    start + keys[start..].partition_point(|key| key < &next_key)
                })
                .unwrap_or(keys.len());
            if start < end {
                work.push((entry_cid(entry), start, end));
            }
            start = end;
        }
        Ok(LookupStep::Children(work))
    }

    pub(super) fn get_links_recursive<'a>(
        &'a self,
        root: Cid,
        keys: &'a [String],
        required: bool,
        parallel: bool,
    ) -> BTreeFuture<'a, BTreeMap<String, Cid>> {
        Box::pin(async move {
            let work = match self.lookup_step(&root, keys, required).await? {
                LookupStep::Found(found) => return Ok(found),
                LookupStep::Children(work) => work,
            };
            if parallel
                && self.read_child_concurrency > 1
                && keys.len() >= PARALLEL_READ_MIN_KEYS
                && work.len() > 1
            {
                return self.lookup_children_parallel(work, keys, required).await;
            }
            let mut found = BTreeMap::new();
            for (cid, start, end) in work {
                found.extend(
                    self.get_links_recursive(cid, &keys[start..end], required, parallel)
                        .await?,
                );
            }
            Ok(found)
        })
    }

    async fn lookup_children_parallel(
        &self,
        mut work: Vec<(Cid, usize, usize)>,
        keys: &[String],
        required: bool,
    ) -> Result<BTreeMap<String, Cid>, BTreeError> {
        // Split the largest ranges before assigning workers. A recently split
        // two-child root otherwise limits every requested worker count to two.
        // Each expanded node is removed, so no node is read a second time.
        let target_keys = keys.len().div_ceil(self.read_child_concurrency);
        let mut found = BTreeMap::new();
        while let Some(index) = work
            .iter()
            .enumerate()
            .filter(|(_, (_, start, end))| end - start > target_keys)
            .max_by_key(|(_, (_, start, end))| end - start)
            .map(|(index, _)| index)
        {
            let (cid, start, end) = work.swap_remove(index);
            match self.lookup_step(&cid, &keys[start..end], required).await? {
                LookupStep::Found(links) => found.extend(links),
                LookupStep::Children(children) => work.extend(
                    children
                        .into_iter()
                        .map(|(cid, begin, end)| (cid, start + begin, start + end)),
                ),
            }
        }
        // Local Store futures perform blocking reads. Scoped OS workers overlap
        // those reads; async-only fanout on one executor cannot. Queue only
        // relevant branches and disable recursive fanout inside each worker.
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let handles = (0..self.read_child_concurrency.min(work.len()))
                .map(|_| {
                    let next = &next;
                    let work = &work;
                    scope.spawn(move || {
                        let mut found = BTreeMap::new();
                        while let Some((cid, start, end)) =
                            work.get(next.fetch_add(1, Ordering::Relaxed))
                        {
                            found.extend(futures::executor::block_on(self.get_links_recursive(
                                cid.clone(),
                                &keys[*start..*end],
                                required,
                                false,
                            ))?);
                        }
                        Ok::<_, BTreeError>(found)
                    })
                })
                .collect::<Vec<_>>();
            // Join on success and error: a failed branch must never return a
            // partial map or leave detached reads behind.
            let results = handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                })
                .collect::<Vec<_>>();
            for result in results {
                found.extend(result?);
            }
            Ok(found)
        })
    }
}
