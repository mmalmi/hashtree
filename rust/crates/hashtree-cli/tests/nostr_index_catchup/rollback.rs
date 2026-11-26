//! Explicit qualification against an externally authenticated historical CLI.
//! Its source/artifact binding belongs in the qualification receipt, not in
//! this repository's machine-independent fixtures.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

fn old_binary() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("HTREE_FORWARD_ROLLBACK_BINARY")
            .expect("explicit rollback qualification needs the historical binary"),
    );
    let expected = std::env::var("HTREE_FORWARD_ROLLBACK_SHA256")
        .expect("explicit rollback qualification needs its authenticated SHA256");
    let metadata = std::fs::symlink_metadata(&path).unwrap();
    assert!(metadata.is_file() && !metadata.file_type().is_symlink());
    let mut file = std::fs::File::open(&path).unwrap();
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    assert_eq!(hex::encode(hasher.finalize()), expected);
    path
}

fn using_binary(template: Command, executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command.args(template.get_args());
    for (key, value) in template.get_envs() {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    if let Some(directory) = template.get_current_dir() {
        command.current_dir(directory);
    }
    command
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an authenticated historical CLI artifact; run the explicit rollback qualification"]
async fn old_cli_rejects_bridge_then_resumes_after_one_real_historical_checkpoint() {
    let old_binary = old_binary();
    let temp = TempDir::new().unwrap();
    let authors = [Keys::generate(), Keys::generate(), Keys::generate()];
    let outside = Keys::generate();
    let old = event(&authors[0], 1, "retained history");
    let root = import(&temp, &old);
    std::fs::write(
        temp.path().join("authors.txt"),
        authors
            .iter()
            .map(|key| format!("{}\n", key.public_key()))
            .collect::<String>(),
    )
    .unwrap();
    let historical = authors
        .iter()
        .enumerate()
        .map(|(i, key)| event(key, 20 + i as u64, "real historical author result"))
        .collect::<Vec<_>>();
    let relay = Relay::new(historical.clone()).await;
    // Demonstrate that this exact old executable works with the fixture before
    // using its later nonzero result as evidence of the format boundary.
    success(
        using_binary(catchup(&temp, &root, &relay), &old_binary)
            .args(["--until", "100", "--max-authors-per-run", "1"])
            .output()
            .unwrap(),
    );
    assert_eq!(checkpoint(&temp)["next_author"], 1);
    let video = EventBuilder::new(Kind::from(21), "forward content retained during rollback")
        .custom_created_at(Timestamp::from_secs(80))
        .sign_with_keys(&outside)
        .unwrap();
    let batch = forward::canonical(&json!({"format":"iris-social/archive-forward-batch@1",
        "configSha256":"c".repeat(64), "since":10, "events":[video]}))
    .into_bytes();
    let digest = hex::encode(Sha256::digest(&batch));
    std::fs::write(temp.path().join("forward.json"), batch).unwrap();
    let receipt = success(forward::forward(&temp, &digest).output().unwrap());
    let directory = temp.path().join("data/nostr-index");
    let checkpoint_path = directory.join("catchup-state.json");
    let bridge_state = std::fs::read(&checkpoint_path).unwrap();
    let rejected = using_binary(catchup(&temp, &root, &relay), &old_binary)
        .args(["--until", "100"])
        .output()
        .unwrap();
    assert!(
        !rejected.status.success(),
        "historical executable unexpectedly accepted a forward head"
    );
    let error = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        error.contains("unknown field") && error.contains("afterRoot"),
        "unexpected old-CLI failure: {error}"
    );
    assert_eq!(std::fs::read(&checkpoint_path).unwrap(), bridge_state);
    assert!(!directory.join("catchup-append.json").exists());

    // One actual completed author writes an ordinary coverage receipt and
    // retains the entire current root, including the outside-graph event.
    success(
        catchup(&temp, &root, &relay)
            .args(["--until", "100", "--max-authors-per-run", "1"])
            .output()
            .unwrap(),
    );
    let compatible = checkpoint(&temp);
    assert_eq!(compatible["next_author"], 2);
    let head = compatible["coverage_head"].as_str().unwrap();
    let coverage: Value = serde_json::from_slice(
        &std::fs::read(
            directory
                .join("catchup-coverage")
                .join(format!("{head}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        coverage["format"],
        "hashtree/nostr-index-catchup-coverage@1"
    );
    assert_eq!(coverage["previous_head"], receipt["after"]["coverage_head"]);
    assert_eq!(coverage["before_root"], receipt["after"]["root"]);
    assert!(!directory.join("catchup-forward-pending.json").exists());
    assert!(!directory.join("catchup-append.json").exists());
    success(
        using_binary(catchup(&temp, &root, &relay), &old_binary)
            .args(["--until", "100"])
            .output()
            .unwrap(),
    );
    let resumed = checkpoint(&temp);
    assert_eq!(resumed["next_author"], 3);
    assert_eq!(resumed["policy"], receipt["before"]["policy"]);
    assert_eq!(resumed["pass_until"], 100);
    let readback = success(
        using_binary(command(&temp), &old_binary)
            .args([
                "nostr-index",
                "query",
                "--root",
                resumed["root"].as_str().unwrap(),
                "--filter",
                &json!({"ids":[video.id.to_hex(), old.id.to_hex(), historical[2].id.to_hex()]})
                    .to_string(),
                "--limit",
                "10",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(readback["count"], 3);
    for expected in [&video, &old, &historical[2]] {
        let expected = serde_json::to_value(expected).unwrap();
        assert!(readback["events"].as_array().unwrap().contains(&expected));
    }
    let by_kind = success(
        using_binary(command(&temp), &old_binary)
            .args([
                "nostr-index",
                "query",
                "--root",
                resumed["root"].as_str().unwrap(),
                "--filter",
                "{\"kinds\":[21]}",
                "--limit",
                "10",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(by_kind["count"], 1);
    assert_eq!(by_kind["events"][0], serde_json::to_value(&video).unwrap());
    assert_eq!(query_id_count(&temp, &root, &[old.id.to_hex()]), 1);
    // This is native checkpoint compatibility. Signed publication and publisher
    // rollback are separate gates; this isolated test publishes nothing.
}
