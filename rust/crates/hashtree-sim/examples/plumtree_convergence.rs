use hashtree_network::MESH_EVENT_POLICY;
use hashtree_sim::{
    run_mesh_pubsub_htl_flood_baseline, run_mesh_pubsub_htl_gossipsub_baseline,
    run_mesh_pubsub_htl_inv_want_baseline, run_mesh_pubsub_htl_plumtree_baseline,
    run_mesh_pubsub_htl_plumtree_baseline_with_timer, MeshPubsubWorkloadConfig, PoolConfig,
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!(" rounds | strategy        | bytes/event | dupes  | p50ms | p95ms");
    println!("--------+-----------------+-------------+--------+-------+------");
    for rounds in [1usize, 2, 4, 8, 16] {
        let cfg = MeshPubsubWorkloadConfig {
            seed: 17,
            node_count: 64,
            author_count: 3,
            subscribers_per_author: 16,
            publish_rounds: rounds,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            spam_author_count: 3,
            spam_subscribers_per_author: 8,
            spam_publish_rounds_per_round: 2,
            subscription_churn_rate: 0.05,
            allow_rejoin: true,
            pump_steps_after_setup: 160,
            pump_steps_per_publish_round: 96,
            latency_per_pump_step_ms: 10,
            ..Default::default()
        };
        let f = run_mesh_pubsub_htl_flood_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl).await;
        let i = run_mesh_pubsub_htl_inv_want_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl).await;
        let p = run_mesh_pubsub_htl_plumtree_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl).await;
        let pt1 = run_mesh_pubsub_htl_plumtree_baseline_with_timer(
            cfg.clone(),
            MESH_EVENT_POLICY.max_htl,
            1,
        )
        .await;
        let pt0 = run_mesh_pubsub_htl_plumtree_baseline_with_timer(
            cfg.clone(),
            MESH_EVENT_POLICY.max_htl,
            0,
        )
        .await;
        let g = run_mesh_pubsub_htl_gossipsub_baseline(
            cfg.clone(),
            MESH_EVENT_POLICY.max_htl,
            6,
            Some(1),
        )
        .await;
        for (label, r) in [
            ("flood", &f),
            ("invwant", &i),
            ("plumtree-tInf", &p),
            ("plumtree-t1", &pt1),
            ("plumtree-t0", &pt0),
            ("gossipsub-d6-t1", &g),
        ] {
            println!(
                " {:>6} | {:<15} | {:>11.1} | {:>6} | {:>5} | {:>5}",
                rounds,
                label,
                r.bytes_sent_per_delivered_event,
                r.duplicate_deliveries,
                r.delivery_latency_p50_ms,
                r.delivery_latency_p95_ms
            );
        }
    }
}
