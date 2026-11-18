use super::{BlobIoAdmission, BlobIoClass, BlobIoLimits};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[test]
fn legacy_concurrency_overrides_cannot_consume_blocking_pool_reserve() {
    let limits = BlobIoLimits::derive(8, 64, Some(usize::MAX), Some(usize::MAX));

    assert_eq!(limits.blocking_threads, 8);
    assert_eq!(limits.reserved_blocking_threads, 2);
    assert_eq!(limits.total_limit, 6);
    assert!(
        limits.total_limit + limits.reserved_blocking_threads <= limits.blocking_threads,
        "storage admission consumed the reserved blocking threads"
    );
    assert!(
        limits.metadata_read_limit >= 1,
        "metadata reads must retain an admission lane"
    );
    assert!(
        limits.bulk_limit < limits.total_limit,
        "bulk reads and writes must not consume the metadata lane"
    );
}

#[test]
fn automatic_budget_scales_with_hardware_without_exhausting_runtime() {
    let small = BlobIoLimits::derive(8, 2, None, None);
    let medium = BlobIoLimits::derive(64, 16, None, None);
    let large = BlobIoLimits::derive(256, 64, None, None);

    assert!(small.total_limit < medium.total_limit);
    assert!(medium.total_limit < large.total_limit);
    for limits in [small, medium, large] {
        assert!(limits.total_limit + limits.reserved_blocking_threads <= limits.blocking_threads);
        assert!(limits.metadata_read_limit >= 1);
        assert!(limits.write_limit >= 1);
    }
}

#[tokio::test]
async fn single_read_ceiling_shares_one_lane_without_disabling_blob_bodies() {
    let limits = BlobIoLimits::derive(8, 1, Some(1), Some(1));
    assert_eq!(limits.read_limit, 1);
    assert_eq!(limits.data_read_limit, 0);

    let admission = BlobIoAdmission::new_for_test(
        limits,
        Duration::from_millis(30),
        Duration::from_millis(30),
        Duration::from_millis(30),
    );
    assert_eq!(
        admission
            .run_data_read(|| 42)
            .await
            .expect("single shared read lane should serve blob bodies"),
        42
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_out_read_keeps_admission_until_blocking_work_finishes() {
    let limits = BlobIoLimits::derive(8, 1, Some(1), Some(1));
    let admission = BlobIoAdmission::new_for_test(
        limits,
        Duration::from_millis(30),
        Duration::from_millis(30),
        Duration::from_millis(30),
    );

    for attempt in 0..3 {
        let blocker = Arc::new((Mutex::new(false), Condvar::new()));
        let started = Arc::new(AtomicBool::new(false));
        let blocked = Arc::clone(&blocker);
        let task_started = Arc::clone(&started);

        let error = admission
            .run_metadata_read(move || {
                task_started.store(true, Ordering::Release);
                let (released, wake) = &*blocked;
                let mut released = released.lock().expect("blocker lock poisoned");
                while !*released {
                    released = wake.wait(released).expect("blocker lock poisoned");
                }
                attempt
            })
            .await
            .expect_err("blocked read should time out");
        assert!(error.is_timeout(), "unexpected read error: {error}");
        assert!(
            started.load(Ordering::Acquire),
            "blocking read did not start before its timeout"
        );

        let snapshot = admission.snapshot();
        assert_eq!(
            snapshot.metadata_read_in_use, 1,
            "timed-out caller released its metadata permit early"
        );
        assert_eq!(
            snapshot.total_in_use, 1,
            "timed-out caller released its total permit early"
        );

        let replacement_ran = Arc::new(AtomicUsize::new(0));
        let ran = Arc::clone(&replacement_ran);
        let error = admission
            .run_metadata_read(move || {
                ran.fetch_add(1, Ordering::Relaxed);
            })
            .await
            .expect_err("replacement read should remain queued");
        assert!(error.is_busy(), "unexpected replacement error: {error}");
        assert_eq!(
            replacement_ran.load(Ordering::Relaxed),
            0,
            "replacement blocking work ran before the timed-out read finished"
        );

        let (released, wake) = &*blocker;
        *released.lock().expect("blocker lock poisoned") = true;
        wake.notify_all();

        let deadline = Instant::now() + Duration::from_secs(1);
        while admission.snapshot().metadata_read_in_use != 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let snapshot = admission.snapshot();
        assert_eq!(snapshot.metadata_read_in_use, 0);
        assert_eq!(snapshot.total_in_use, 0);

        assert_eq!(
            admission
                .run_metadata_read(|| 42)
                .await
                .expect("admission did not recover"),
            42
        );
    }
}

#[tokio::test]
async fn optional_cache_write_waits_for_capacity_and_warms_the_cache() {
    let admission = BlobIoAdmission::new_for_test(
        BlobIoLimits::derive(8, 1, Some(1), Some(1)),
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_secs(30),
    );
    let writer = admission.acquire(BlobIoClass::Write).await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let blob = temp.path().join("verified-blob");
    let destination = blob.clone();
    let mut fill = Box::pin(
        admission
            .run_optional_write(move || std::fs::write(destination, b"verified cache contents")),
    );

    assert!(futures::poll!(&mut fill).is_pending());
    assert_eq!(admission.snapshot().write_in_use, 1);
    assert!(!blob.exists());

    drop(writer);
    fill.await.unwrap().unwrap();
    assert_eq!(std::fs::read(blob).unwrap(), b"verified cache contents");
    assert_eq!(admission.snapshot().write_in_use, 0);
}

#[tokio::test]
async fn optional_cache_write_honors_the_queue_deadline_without_leaking_permits() {
    let admission = BlobIoAdmission::new_for_test(
        BlobIoLimits::derive(8, 1, Some(1), Some(1)),
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_millis(1),
    );
    let writer = admission.acquire(BlobIoClass::Write).await.unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        admission.run_optional_write(|| panic!("optional write reached a saturated storage pool")),
    )
    .await
    .expect("optional cache write must honor the configured deadline");
    assert!(result.unwrap_err().is_busy());
    assert_eq!(admission.snapshot().write_in_use, 1);

    drop(writer);
    assert_eq!(admission.run_optional_write(|| 42).await.unwrap(), 42);
    assert_eq!(admission.snapshot().write_in_use, 0);
}

#[tokio::test]
async fn cancelling_the_last_blob_waiter_releases_partial_admission() {
    let admission = Arc::new(BlobIoAdmission::new_for_test(
        BlobIoLimits::derive(8, 1, Some(1), Some(1)),
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_secs(1),
    ));
    let bulk = admission.bulk.clone().acquire_owned().await.unwrap();
    let inflight = crate::server::inflight::InFlight::default();
    let first = admission.clone();
    let mut cancelled = Box::pin(inflight.run([1; 32], || async move {
        first
            .run_optional_write(|| 17)
            .await
            .map_err(|e| e.to_string())
    }));

    assert!(futures::poll!(&mut cancelled).is_pending());
    assert_eq!(admission.snapshot().write_in_use, 1);
    drop(cancelled);
    assert_eq!(admission.snapshot().write_in_use, 0);
    drop(bulk);

    let another = admission.clone();
    assert_eq!(
        inflight
            .run([2; 32], || async move {
                another
                    .run_optional_write(|| 42)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await
            .unwrap(),
        42
    );
}

#[tokio::test]
async fn cancelling_one_blob_waiter_keeps_the_same_work_for_the_other() {
    let inflight = crate::server::inflight::InFlight::default();
    let (release, blocked) = tokio::sync::oneshot::channel();
    let mut first = Box::pin(inflight.run([1; 32], || async move { blocked.await.unwrap() }));
    let mut second = Box::pin(inflight.run([1; 32], || async { panic!("duplicate work") }));
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());

    drop(first);
    release.send(42).unwrap();
    assert_eq!(second.await, 42);
    assert_eq!(inflight.run([1; 32], || async { 43 }).await, 43);
}
