use super::StorageRouter;

#[test]
fn s3_sync_helper_runs_from_plain_threads_and_reuses_one_runtime() {
    let first = StorageRouter::run_s3_future_sync(async { tokio::runtime::Handle::current().id() })
        .expect("first call");
    let second =
        StorageRouter::run_s3_future_sync(async { tokio::runtime::Handle::current().id() })
            .expect("second call");
    assert_eq!(first, second, "calls share one runtime");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_sync_helper_runs_from_inside_another_runtime() {
    let outer = tokio::runtime::Handle::current().id();
    let inner = tokio::task::spawn_blocking(|| {
        StorageRouter::run_s3_future_sync(async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            tokio::runtime::Handle::current().id()
        })
    })
    .await
    .expect("join")
    .expect("helper");
    assert_ne!(outer, inner, "work runs on the dedicated S3 runtime");
}
