//! Seeded overlay experiment. The carrier is simulated, and the production
//! provider policy, router, forwarding, cache, and wire codec handle each read.
mod topology;

use std::collections::BTreeSet;
use std::sync::atomic::Ordering::Relaxed;

use hashtree_core::{sha256, BlobRequest, Store};
use tokio::task::JoinSet;
use tokio::time::Instant;

use fips_core::PeerIdentity;

use self::topology::{identity, mix, Network, HOP_LIMIT, SEARCH_DEADLINE};

const BATCH_SIZE: usize = 8;
const BATCH_COUNT: usize = 8;
const PAYLOAD_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Profile {
    Steady,
    Churn,
    Adversarial,
}

#[derive(Default)]
struct Report {
    reachable: usize,
    reachable_queries: BTreeSet<usize>,
    verified_queries: BTreeSet<usize>,
    verified_ms: Vec<u128>,
    misses: usize,
    deadlines: usize,
    incomplete: usize,
}

#[tokio::test(start_paused = true)]
async fn mesh_100_nodes() {
    run_size(100).await;
}

#[tokio::test(start_paused = true)]
async fn mesh_1000_nodes() {
    run_size(1_000).await;
}

#[tokio::test(start_paused = true)]
async fn mesh_10000_nodes() {
    run_size(10_000).await;
}

async fn run_size(size: usize) {
    let seeds: Vec<u64> = match std::env::var("HASHTREE_MESH_SIM_SEEDS") {
        Ok(list) => list
            .split(',')
            .map(|seed| {
                seed.trim()
                    .parse()
                    .expect("expected comma-separated u64 seeds")
            })
            .collect(),
        Err(std::env::VarError::NotPresent) => vec![17, 29],
        Err(error) => panic!("invalid HASHTREE_MESH_SIM_SEEDS: {error}"),
    };
    let identities: Vec<_> = (0..size).map(identity).collect();
    for profile in [Profile::Steady, Profile::Churn, Profile::Adversarial] {
        for &seed in &seeds {
            run_case(&identities, seed, profile).await;
        }
    }
}

async fn run_case(identities: &[PeerIdentity], seed: u64, profile: Profile) {
    let size = identities.len();
    let adversarial = profile == Profile::Adversarial;
    let churn = profile == Profile::Churn;
    let network = Network::new(identities, seed, churn, adversarial).await;
    let simulation_start = Instant::now();
    let mut report = Report::default();
    let mut departures = 0;
    for batch in 0..BATCH_COUNT {
        if churn && batch == BATCH_COUNT / 2 {
            for (index, node) in network.nodes.iter().enumerate() {
                if mix(seed ^ index as u64).is_multiple_of(8) {
                    node.online.store(false, Relaxed);
                    departures += 1;
                }
            }
        }

        let mut reads = JoinSet::new();
        let burst = batch == 2 || batch == 6;
        for slot in 0..BATCH_SIZE {
            let probe = batch * BATCH_SIZE + if burst { 0 } else { slot };
            let sample = mix(seed.wrapping_mul(10_007) ^ probe as u64);
            let mut reader = sample as usize % size;
            while !network.nodes[reader].serves_blobs() {
                reader = (reader + 1) % size;
            }
            let replica_count = match (burst, slot) {
                (false, 0) => 0,
                (false, 1..=3) => 3,
                _ => 1,
            };
            let mut data = vec![0; PAYLOAD_BYTES];
            data[..8].copy_from_slice(&(probe as u64).to_le_bytes());
            let hash = sha256(&data);
            let mut holders = Vec::with_capacity(replica_count);
            for replica in 0..replica_count {
                let mut holder = mix(sample.wrapping_add(replica as u64)) as usize % size;
                while holder == reader || holders.contains(&holder) {
                    holder = (holder + 1) % size;
                }
                network.nodes[holder]
                    .store
                    .put(hash, data.clone())
                    .await
                    .unwrap();
                holders.push(holder);
            }
            let reachable = network.reachable(reader, &holders);
            report.reachable += usize::from(reachable);
            if reachable {
                report.reachable_queries.insert(probe);
            }
            let router = network.nodes[reader].router.clone();
            reads.spawn(async move {
                let start = Instant::now();
                let reply = router
                    .get_request(
                        BlobRequest {
                            hash,
                            htl: HOP_LIMIT,
                        },
                        Some(&["store".into()]),
                        None,
                    )
                    .await;
                let elapsed = start.elapsed();
                if let Ok(Some(received)) = &reply {
                    assert!(reachable, "a verified reply must have a reachable holder");
                    assert_eq!(*received, data);
                }
                (probe, reply, elapsed)
            });
        }

        while let Some(result) = reads.join_next().await {
            let (probe, reply, elapsed) = result.unwrap();
            match reply {
                Ok(Some(_)) => {
                    report.verified_queries.insert(probe);
                    report.verified_ms.push(elapsed.as_millis());
                }
                Ok(None) => report.misses += 1,
                Err(_) if elapsed >= SEARCH_DEADLINE => report.deadlines += 1,
                Err(_) => report.incomplete += 1,
            }
        }
    }

    let reads = BATCH_COUNT * BATCH_SIZE;
    let verified = report.verified_ms.len();
    assert_eq!(
        verified + report.misses + report.deadlines + report.incomplete,
        reads
    );
    assert!(report.reachable < reads);
    // Keep the established delivery smoke checks on the reference workloads.
    if size <= 1_000 && matches!(seed, 17 | 29) {
        assert!(verified > 0);
    }
    report.verified_ms.sort_unstable();
    let latency = |percent: usize| {
        (percent * verified)
            .div_ceil(100)
            .checked_sub(1)
            .map_or_else(|| "n/a".into(), |i| report.verified_ms[i].to_string())
    };
    let p50_ms = latency(50);
    let p95_ms = latency(95);
    let attempts = network.traffic.attempts.load(Relaxed);
    let wire_bytes = network.traffic.bytes.load(Relaxed);
    let simulated_ms = simulation_start.elapsed().as_millis();
    let wire_bps = wire_bytes as u128 * 8_000 / simulated_ms;
    let false_misses = network.traffic.false_misses.load(Relaxed);
    let corrupt_rejected = network.traffic.corrupt_rejected.load(Relaxed);
    if adversarial {
        assert!(false_misses > 0 && corrupt_rejected > 0);
        for node in &network.nodes {
            for key in node.store.keys() {
                assert_eq!(sha256(&node.store.get(&key).await.unwrap().unwrap()), key);
            }
        }
    }
    println!(
        "mesh_scale nodes={size} seed={seed} profile={profile:?} departed={departures} \
         reads={reads} reachable={} verified={verified} p50_ms={p50_ms} p95_ms={p95_ms} misses={} \
         deadlines={} incomplete={} attempts={attempts} wire_bytes={wire_bytes} \
         simulated_ms={simulated_ms} wire_bps={wire_bps} false_misses={false_misses} \
         corrupt_rejected={corrupt_rejected} distinct_reachable={} distinct_verified={}",
        report.reachable,
        report.misses,
        report.deadlines,
        report.incomplete,
        report.reachable_queries.len(),
        report.verified_queries.len(),
    );
}
