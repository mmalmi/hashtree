use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

struct Occupied(Arc<AtomicUsize>);
impl Occupied {
    fn new(active: &Arc<AtomicUsize>, peak: &Arc<AtomicUsize>) -> Self {
        let next = active.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(next, Ordering::SeqCst);
        Self(active.clone())
    }
}
impl Drop for Occupied {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_order_fetches_commit_in_order_with_only_two_total_author_slots() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (started, mut starts) = mpsc::unbounded_channel();
    let (release, hold) = oneshot::channel::<()>();
    let mut hold = Some(hold);
    let future = run(
        [0usize, 1],
        0..6,
        |source, index| {
            let occupied = Occupied::new(&active, &peak);
            let started = started.clone();
            let wait = if index == 0 { hold.take() } else { None };
            async move {
                started.send((source, index)).unwrap();
                if let Some(wait) = wait {
                    wait.await.unwrap();
                }
                (source, Ok((index, occupied)))
            }
        },
        vec![],
        |mut written, index, (fetched, occupied)| async move {
            assert_eq!(index, fetched);
            written.push(index);
            drop(occupied);
            Ok(written)
        },
    );
    tokio::pin!(future);
    let first = tokio::select! { _=&mut future=>panic!("blocked first author"), request=starts.recv()=>request.unwrap() };
    let second = tokio::select! { _=&mut future=>panic!("blocked first author"), request=starts.recv()=>request.unwrap() };
    let mut observed = [first, second];
    observed.sort();
    assert_eq!(observed, [(0, 0), (1, 1)]);
    assert_eq!(active.load(Ordering::SeqCst), 2);
    // Finishing author1 cannot admit author2 while author0 still occupies its slot.
    tokio::select! { _=&mut future=>panic!("blocked first author"), request=starts.recv()=>panic!("third slot: {request:?}"), _=tokio::task::yield_now()=>{} }
    release.send(()).unwrap();
    assert_eq!(future.await.unwrap(), vec![0, 1, 2, 3, 4, 5]);
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_author_failure_preserves_successful_earlier_commit_and_stops_admission() {
    let commits = Arc::new(AtomicUsize::new(0));
    let requested = Arc::new(AtomicUsize::new(0));
    let result = run(
        [0usize, 1],
        0..5,
        |source, index| {
            requested.fetch_add(1, Ordering::SeqCst);
            async move {
                if index == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                (
                    source,
                    if index == 1 {
                        Err(anyhow::anyhow!("source failed"))
                    } else {
                        Ok(index)
                    },
                )
            }
        },
        0,
        |frontier, index, _| {
            let commits = commits.clone();
            async move {
                assert_eq!(frontier, index);
                commits.store(frontier + 1, Ordering::SeqCst);
                Ok(frontier + 1)
            }
        },
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("source failed"));
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(requested.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writer_failure_and_cancellation_drop_all_fetches_without_detached_work() {
    for fail_writer in [true, false] {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (started, mut starts) = mpsc::unbounded_channel();
        let mut future = Box::pin(run(
            [0usize, 1],
            0..4,
            |source, index| {
                let occupied = Occupied::new(&active, &peak);
                let started = started.clone();
                async move {
                    started.send(index).unwrap();
                    if index == 1 {
                        std::future::pending::<()>().await;
                    }
                    (source, Ok(occupied))
                }
            },
            (),
            |_, _, occupied| async move {
                if fail_writer {
                    drop(occupied);
                    Err(anyhow::anyhow!("write failed"))
                } else {
                    std::future::pending().await
                }
            },
        ));
        if fail_writer {
            assert!(future
                .await
                .unwrap_err()
                .to_string()
                .contains("write failed"));
        } else {
            tokio::select! {_=&mut future=>panic!("must stay pending"), _=starts.recv()=>{}}
            drop(future);
            tokio::time::timeout(Duration::from_secs(1), async {
                while active.load(Ordering::SeqCst) != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(peak.load(Ordering::SeqCst) <= 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_writer_applies_backpressure_and_fetches_continue_while_it_waits() {
    let (started, mut starts) = mpsc::unbounded_channel();
    let (release, hold) = oneshot::channel::<()>();
    let mut hold = Some(hold);
    let future = run(
        [0usize, 1],
        0..3,
        |source, index| {
            let started = started.clone();
            async move {
                started.send(index).unwrap();
                (source, Ok(index))
            }
        },
        vec![],
        |mut written, index, _| {
            let wait = if index == 0 { hold.take() } else { None };
            async move {
                if let Some(wait) = wait {
                    wait.await.unwrap();
                }
                written.push(index);
                Ok(written)
            }
        },
    );
    tokio::pin!(future);
    let mut observed = Vec::new();
    for _ in 0..2 {
        tokio::select! {_=&mut future=>panic!("writer blocked"), value=starts.recv()=>observed.push(value.unwrap())}
    }
    observed.sort();
    assert_eq!(observed, vec![0, 1]);
    tokio::select! {_=&mut future=>panic!("writer blocked"), value=starts.recv()=>panic!("unbounded prefetch: {value:?}"), _=tokio::task::yield_now()=>{}}
    release.send(()).unwrap();
    assert_eq!(future.await.unwrap(), vec![0, 1, 2]);
}

// The production CLI's block_on writer runs outside the runtime worker pool.
// A synchronous store phase must not stop the next relay query's timer/I/O.
#[test]
fn synchronous_writer_does_not_starve_prefetched_query_deadline() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let fetched_next = Arc::new(AtomicBool::new(false));
    runtime.block_on(async {
        let written = run(
            [0usize, 1],
            0..2,
            |source, index| {
                let fetched_next = fetched_next.clone();
                async move {
                    if index == 1 {
                        tokio::time::timeout(Duration::from_millis(100), async {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            fetched_next.store(true, Ordering::SeqCst);
                        })
                        .await
                        .unwrap();
                    }
                    (source, Ok(index))
                }
            },
            vec![],
            |mut written, index, _| {
                let fetched_next = fetched_next.clone();
                async move {
                    if index == 0 {
                        std::thread::sleep(Duration::from_millis(200));
                        assert!(fetched_next.load(Ordering::SeqCst));
                    }
                    written.push(index);
                    Ok(written)
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(written, vec![0, 1]);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_source_is_reused_only_after_its_previous_author_commits() {
    let written = run(
        [Vec::<usize>::new(), Vec::new()],
        0..6,
        |mut previous, ordinal| async move {
            if let Some(last) = previous.last() {
                assert_eq!(ordinal, last + 2);
            }
            previous.push(ordinal);
            (previous.clone(), Ok(previous))
        },
        vec![],
        |mut written, ordinal, fetched| async move {
            assert_eq!(fetched.last(), Some(&ordinal));
            written.push(fetched);
            Ok(written)
        },
    )
    .await
    .unwrap();
    assert_eq!(
        written,
        vec![
            vec![0],
            vec![1],
            vec![0, 2],
            vec![1, 3],
            vec![0, 2, 4],
            vec![1, 3, 5]
        ]
    );
}
