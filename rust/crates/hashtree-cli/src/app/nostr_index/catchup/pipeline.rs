//! Two supervised author slots, including the author currently being committed.
//!
//! Network jobs own reusable sources and run independently of synchronous store
//! writes. No source is recycled until its author's durable commit finishes.
use std::{
    collections::BTreeMap,
    future::Future,
    ops::Range,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{Context, Result};
use tokio::task::JoinSet;

async fn fetch_one<S, T>(
    ordinal: usize,
    failed: Arc<AtomicBool>,
    fetch: impl Future<Output = (S, Result<T>)>,
) -> (usize, S, Result<T>) {
    let (source, result) = fetch.await;
    if result.is_err() {
        failed.store(true, Ordering::Release);
    }
    (ordinal, source, result)
}

pub(super) async fn run<S, T, W, Fetch, FetchFuture, Commit, CommitFuture>(
    sources: [S; 2],
    authors: Range<usize>,
    mut fetch: Fetch,
    mut writer: W,
    mut commit: Commit,
) -> Result<W>
where
    S: Send + 'static,
    T: Send + 'static,
    Fetch: FnMut(S, usize) -> FetchFuture,
    FetchFuture: Future<Output = (S, Result<T>)> + Send + 'static,
    Commit: FnMut(W, usize, T) -> CommitFuture,
    CommitFuture: Future<Output = Result<W>>,
{
    let mut next_to_fetch = authors.clone();
    let mut fetching = JoinSet::new();
    let mut ready = BTreeMap::new();
    let failed = Arc::new(AtomicBool::new(false));
    for source in sources {
        if let Some(ordinal) = next_to_fetch.next() {
            fetching.spawn(fetch_one(ordinal, failed.clone(), fetch(source, ordinal)));
        }
    }
    let result = async {
        for ordinal in authors {
            while !ready.contains_key(&ordinal) {
                let (index, source, result) = fetching
                    .join_next()
                    .await
                    .expect("occupied author slot")
                    .context("catchup fetch task")?;
                ready.insert(index, (source, result));
            }
            let (source, result) = ready.remove(&ordinal).expect("ordered author result");
            let writing = commit(writer, ordinal, result?);
            tokio::pin!(writing);
            writer = loop {
                if fetching.is_empty() {
                    break writing.await?;
                }
                tokio::select! {
                    result = &mut writing => break result?,
                    Some(result) = fetching.join_next() => {
                        let (index, source, result) = result.context("catchup fetch task")?;
                        ready.insert(index, (source, result));
                    }
                }
            };
            // The writer, buffered results and network jobs together occupy at
            // most two slots. A failed future author stops further admission,
            // while earlier successful authors still reach durable checkpoints.
            if !failed.load(Ordering::Acquire) {
                if let Some(index) = next_to_fetch.next() {
                    fetching.spawn(fetch_one(index, failed.clone(), fetch(source, index)));
                }
            }
        }
        Ok(writer)
    }
    .await;
    // Ordinary failure waits for all network jobs to stop before returning.
    // Cancellation drops this owned JoinSet, which aborts (rather than detaches)
    // its jobs and their sockets; there are no independent worker/queue tasks.
    fetching.abort_all();
    while fetching.join_next().await.is_some() {}
    result
}

#[cfg(test)]
mod tests;
