//! One bounded worker queue for independent copy-on-write subtrees.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

enum UpdatePlan {
    Ready(LinkNodeUpdate),
    Task(BuiltNode, usize, usize),
    Branch(Cid, Vec<usize>),
}

impl<S: Store> BTree<S> {
    pub(super) fn link_child_work(
        entries: &[TreeEntry],
        changes: &[(String, Option<Cid>)],
    ) -> Vec<(BuiltNode, usize, usize)> {
        let mut start = 0;
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let end = entries
                    .get(index + 1)
                    .map(|next| {
                        let key = unescape_key(&next.name);
                        start + changes[start..].partition_point(|(candidate, _)| candidate < &key)
                    })
                    .unwrap_or(changes.len());
                let row = (
                    BuiltNode {
                        first_key: unescape_key(&entry.name),
                        cid: entry_cid(entry),
                        count: stored_link_subtree_count(entry),
                    },
                    start,
                    end,
                );
                start = end;
                row
            })
            .collect()
    }

    pub(super) fn record_replaced_link_node(node: Cid, update: &mut LinkNodeUpdate) {
        // A split can retain the old node verbatim; it must not be reclaimed.
        if !update
            .nodes
            .iter()
            .any(|replacement| replacement.cid == node)
        {
            update.superseded_nodes.push(node);
        }
    }

    pub(super) async fn update_link_children_parallel(
        &self,
        work: Vec<(BuiltNode, usize, usize)>,
        changes: &[(String, Option<Cid>)],
    ) -> Result<LinkNodeUpdate, BTreeError> {
        fn plan(node: BuiltNode, start: usize, end: usize) -> UpdatePlan {
            if start == end {
                UpdatePlan::Ready(LinkNodeUpdate {
                    nodes: vec![node],
                    superseded_nodes: Vec::new(),
                })
            } else {
                UpdatePlan::Task(node, start, end)
            }
        }
        let root_count = work.len();
        let mut plans = work
            .into_iter()
            .map(|(node, start, end)| Some(plan(node, start, end)))
            .collect::<Vec<_>>();
        let target = changes.len().div_ceil(self.update_child_concurrency);
        // A two-child root must not limit the entire operation to two workers.
        // Expand large ranges once, remembering their original boundaries so
        // rebuilding preserves exact nodes, roots and superseded-node order.
        while let Some(index) = plans
            .iter()
            .enumerate()
            .filter_map(|(index, plan)| match plan {
                Some(UpdatePlan::Task(_, start, end)) if end - start > target => {
                    Some((index, end - start))
                }
                _ => None,
            })
            .max_by_key(|(_, count)| *count)
            .map(|(index, _)| index)
        {
            let Some(UpdatePlan::Task(node, start, end)) = plans[index].take() else {
                unreachable!()
            };
            let entries = sort_entries(self.tree.list_directory_required(&node.cid).await?);
            if is_leaf_node(&entries) {
                let mut result = LinkNodeUpdate {
                    nodes: self.update_link_leaf(entries, &changes[start..end]).await?,
                    superseded_nodes: Vec::new(),
                };
                Self::record_replaced_link_node(node.cid, &mut result);
                plans[index] = Some(UpdatePlan::Ready(result));
            } else {
                let mut children = Vec::new();
                for (child, begin, finish) in Self::link_child_work(&entries, &changes[start..end])
                {
                    children.push(plans.len());
                    plans.push(Some(plan(child, start + begin, start + finish)));
                }
                plans[index] = Some(UpdatePlan::Branch(node.cid, children));
            }
        }
        let jobs = plans
            .iter()
            .enumerate()
            .filter_map(|(index, plan)| match plan {
                Some(UpdatePlan::Task(node, start, end)) => {
                    Some((index, node.cid.clone(), *start, *end))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let next = AtomicUsize::new(0);
        let results = std::thread::scope(|scope| {
            let handles = (0..self.update_child_concurrency.min(jobs.len()))
                .map(|_| {
                    let jobs = &jobs;
                    let next = &next;
                    scope.spawn(move || {
                        let mut results = Vec::new();
                        while let Some((index, node, start, end)) =
                            jobs.get(next.fetch_add(1, Ordering::Relaxed))
                        {
                            let result = futures::executor::block_on(self.update_link_node(
                                node.clone(),
                                &changes[*start..*end],
                                false,
                            ))?;
                            results.push((*index, result));
                        }
                        Ok::<_, BTreeError>(results)
                    })
                })
                .collect::<Vec<_>>();
            // Join every worker even on a missing node or other error.
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                })
                .collect::<Vec<_>>()
        });
        for result in results {
            for (index, update) in result? {
                plans[index] = Some(UpdatePlan::Ready(update));
            }
        }
        fn take_ready(plan: &mut Option<UpdatePlan>) -> LinkNodeUpdate {
            match plan.take() {
                Some(UpdatePlan::Ready(update)) => update,
                _ => unreachable!(),
            }
        }
        // Descendants are appended after parents. Rebuild bottom-up, but merge
        // each parent's children in their original order, exactly as serial.
        for index in (0..plans.len()).rev() {
            if matches!(plans[index], Some(UpdatePlan::Branch(_, _))) {
                let Some(UpdatePlan::Branch(node, children)) = plans[index].take() else {
                    unreachable!()
                };
                let mut update = LinkNodeUpdate::default();
                for child in children {
                    let result = take_ready(&mut plans[child]);
                    update.nodes.extend(result.nodes);
                    update.superseded_nodes.extend(result.superseded_nodes);
                }
                update.nodes = self.create_link_node_level(update.nodes).await?;
                Self::record_replaced_link_node(node, &mut update);
                plans[index] = Some(UpdatePlan::Ready(update));
            }
        }
        let mut update = LinkNodeUpdate::default();
        for plan in &mut plans[..root_count] {
            let result = take_ready(plan);
            update.nodes.extend(result.nodes);
            update.superseded_nodes.extend(result.superseded_nodes);
        }
        Ok(update)
    }
}
