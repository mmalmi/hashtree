//! Explicit hosted diagnostic; never invoked by normal tests or installs.
use futures::StreamExt;
use hashtree_client::{BlobStore, ClientConfig};
use hashtree_core::Store;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

const BLOCKS: [&str; 2] = [
    "8e2fd80abaf5b1bc08937cf8c473fc092cda5c3ea407acfc1129d1057ca1cf0e",
    "4111540f8bd9e8b342d92f1d3640e17cebb23fdc045a5d231fd563bdb4072743",
];
const SERVERS: [&str; 2] = ["https://cdn.iris.to", "https://upload.iris.to"];

async fn raw(client: &reqwest::Client, server: &str, hash: &str, connection: &str, attempt: usize) {
    let start = Instant::now();
    let mut bytes = Vec::new();
    let mut chunks = 0usize;
    let mut last_progress_ms = None;
    let mut status = None;
    let mut protocol = None;
    let outcome: anyhow::Result<()> = async {
        let response = client.get(format!("{server}/{hash}.bin")).send().await?;
        status = Some(response.status().as_u16());
        protocol = Some(format!("{:?}", response.version()));
        println!("{}", json!({"phase":"headers","server":server,"block":hash,"connection":connection,"attempt":attempt,"elapsed_ms":start.elapsed().as_millis(),"status":status,"protocol":protocol,"length":response.content_length(),"cache":response.headers().get("cf-cache-status").and_then(|v|v.to_str().ok())}));
        let mut stream = response.error_for_status()?.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            anyhow::ensure!(bytes.len() + chunk.len() <= 4 * 1024 * 1024, "oversized block");
            bytes.extend_from_slice(&chunk);
            chunks += 1;
            last_progress_ms = Some(start.elapsed().as_millis());
        }
        anyhow::ensure!(hex::encode(Sha256::digest(&bytes)) == hash, "hash mismatch");
        Ok(())
    }.await;
    println!(
        "{}",
        json!({"phase":"raw_result","server":server,"block":hash,"connection":connection,"attempt":attempt,"bytes":bytes.len(),"chunks":chunks,"last_progress_ms":last_progress_ms,"elapsed_ms":start.elapsed().as_millis(),"status":status,"protocol":protocol,"ok":outcome.is_ok(),"error":outcome.err().map(|e|format!("{e:#}"))})
    );
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    for block in BLOCKS {
        let hash: [u8; 32] = hex::decode(block)?.try_into().unwrap();
        for sources in [vec![SERVERS[0]], vec![SERVERS[1]], SERVERS.to_vec()] {
            let cache = tempfile::tempdir()?;
            let store = BlobStore::new(
                cache.path(),
                ClientConfig {
                    daemon_url: None,
                    local_only: false,
                    relays: vec![],
                    read_servers: sources.iter().map(|s| s.to_string()).collect(),
                    resolve_window: Duration::from_secs(3),
                    request_timeout: Duration::from_secs(30),
                },
            )?;
            for round in 1..=2 {
                // Retain HTTP pool, remove only this diagnostic's local block cache.
                store.delete(&hash).await?;
                let start = Instant::now();
                let result = store.get(&hash).await;
                println!(
                    "{}",
                    json!({"phase":"qualified_client","block":block,"sources":sources,"round":round,"cache":"empty","connection":if round==1 {"fresh_pool"} else {"reuse_eligible_pool"},"elapsed_ms":start.elapsed().as_millis(),"bytes":result.as_ref().ok().and_then(|b|b.as_ref()).map(Vec::len),"ok":matches!(&result,Ok(Some(_))),"error":result.err().map(|e|e.to_string()),"attempts":"internal bounded retry; not instrumented"})
                );
            }
        }
        for server in SERVERS {
            let pooled = http();
            for attempt in 1..=2 {
                raw(&pooled, server, block, "reuse_eligible_pool", attempt).await;
            }
            raw(&http(), server, block, "fresh_pool", 1).await;
        }
    }
    Ok(())
}
