use super::*;
use sha2::{Digest, Sha256};

pub(super) fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        canonical(&map[key])
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        _ => serde_json::to_string(value).unwrap(),
    }
}

pub(super) fn forward(temp: &TempDir, digest: &str) -> Command {
    let mut cmd = command(temp);
    cmd.args(["nostr-index", "append-forward", "--batch"])
        .arg(temp.path().join("forward.json"))
        .args([
            "--batch-sha256",
            digest,
            "--config-sha256",
            &"c".repeat(64),
            "--since",
            "10",
            "--min-free-bytes",
            "0",
            "--authors-file",
        ])
        .arg(temp.path().join("authors.txt"));
    cmd
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_forward_keeps_cursor_then_resumes_and_replays_original_receipt() {
    let temp = TempDir::new().unwrap();
    let first = Keys::generate();
    let second = Keys::generate();
    let outside = Keys::generate();
    let old = event(&first, 1, "protected historical root");
    let root = import(&temp, &old);
    std::fs::write(
        temp.path().join("authors.txt"),
        format!("{}\n{}\n", first.public_key(), second.public_key()),
    )
    .unwrap();
    let later = event(&second, 70, "historical work still pending");
    let relay = Relay::new(vec![event(&first, 50, "first author"), later.clone()]).await;
    success(
        catchup(&temp, &root, &relay)
            .args(["--until", "100", "--max-authors-per-run", "1"])
            .output()
            .unwrap(),
    );
    let before = checkpoint(&temp);
    assert_eq!(before["next_author"], 1);
    let video = EventBuilder::new(Kind::from(21), "outside graph video")
        .custom_created_at(Timestamp::from_secs(80))
        .sign_with_keys(&outside)
        .unwrap();
    // The existing archive stores signed deletion requests. Even a cross-author
    // target reference must not delete history during this append operation.
    let deletion = EventBuilder::new(Kind::from(5), "deletion request")
        .tags([nostr::Tag::parse(["e", &old.id.to_hex()]).unwrap()])
        .custom_created_at(Timestamp::from_secs(81))
        .sign_with_keys(&outside)
        .unwrap();
    let batch = canonical(&json!({"format":"iris-social/archive-forward-batch@1",
        "configSha256":"c".repeat(64), "since":10, "events":[video, deletion]}))
    .into_bytes();
    let digest = hex::encode(Sha256::digest(&batch));
    std::fs::write(temp.path().join("forward.json"), &batch).unwrap();
    assert!(!forward(&temp, &"a".repeat(64))
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(checkpoint(&temp), before);
    let output = forward(&temp, &digest).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    let after = checkpoint(&temp);
    assert_eq!(receipt["before"], before);
    assert_eq!(receipt["after"], after);
    let mut frontier = after.clone();
    frontier["root"] = before["root"].clone();
    frontier["coverage_head"] = before["coverage_head"].clone();
    assert_eq!(frontier, before);
    let new_root = after["root"].as_str().unwrap();
    assert_eq!(
        query_id_count(
            &temp,
            new_root,
            &[video.id.to_hex(), deletion.id.to_hex(), old.id.to_hex()]
        ),
        3
    );
    assert_eq!(query_id_count(&temp, &root, &[old.id.to_hex()]), 1);
    let kinds = success(
        command(&temp)
            .args([
                "nostr-index",
                "query",
                "--root",
                new_root,
                "--filter",
                "{\"kinds\":[21]}",
                "--limit",
                "10",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(kinds["count"], 1);
    success(
        catchup(&temp, &root, &relay)
            .args(["--until", "100"])
            .output()
            .unwrap(),
    );
    let completed = checkpoint(&temp);
    assert_eq!(completed["next_author"], 2);
    assert_eq!(completed["pass_until"], 100);
    assert_eq!(
        query_id_count(
            &temp,
            completed["root"].as_str().unwrap(),
            &[video.id.to_hex(), later.id.to_hex()]
        ),
        2
    );
    let replay = forward(&temp, &digest).output().unwrap();
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(replay.stdout, output.stdout);
    assert_eq!(checkpoint(&temp), completed);
    success(
        catchup(&temp, &root, &relay)
            .args(["--until", "200"])
            .output()
            .unwrap(),
    );
    let rolled = checkpoint(&temp);
    assert_eq!(rolled["pass_since"], 10); // existing default overlap exceeds prior window
    assert_eq!(rolled["pass_until"], 200);
    assert_eq!(
        query_id_count(
            &temp,
            rolled["root"].as_str().unwrap(),
            &[video.id.to_hex(), old.id.to_hex()]
        ),
        2
    );
}
