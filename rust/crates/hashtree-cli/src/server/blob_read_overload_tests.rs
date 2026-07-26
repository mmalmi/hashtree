use super::{BlobIoAdmission, BlobIoLimits};
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
