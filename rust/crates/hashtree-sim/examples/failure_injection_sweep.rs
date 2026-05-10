use hashtree_network::MESH_EVENT_POLICY;
use hashtree_sim::{
    run_mesh_pubsub_htl_flood_baseline, run_mesh_pubsub_htl_gossipsub_baseline,
    run_mesh_pubsub_htl_gossipsub_v11_baseline, run_mesh_pubsub_htl_inv_want_baseline,
    run_mesh_pubsub_htl_plumtree_baseline, run_mesh_pubsub_htl_plumtree_baseline_with_timer,
    MeshPubsubWorkloadConfig, PoolConfig,
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!(" broken | strategy        | delivery% | bytes/event | dupes  | p50ms | p95ms");
    for broken in [0.0_f64, 0.10, 0.20, 0.30, 0.50, 0.70] {
        let cfg = MeshPubsubWorkloadConfig {
            seed: 17,
            node_count: 64,
            author_count: 3,
            subscribers_per_author: 16,
            publish_rounds: 4,
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
            broken_edge_fraction: broken,
            ..Default::default()
        };
        let f = run_mesh_pubsub_htl_flood_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl).await;
        let i = run_mesh_pubsub_htl_inv_want_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl).await;
        let pt_inf =
            run_mesh_pubsub_htl_plumtree_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl).await;
        let pt_t1 = run_mesh_pubsub_htl_plumtree_baseline_with_timer(
            cfg.clone(),
            MESH_EVENT_POLICY.max_htl,
            1,
        )
        .await;
        let g_no_t =
            run_mesh_pubsub_htl_gossipsub_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl, 6, None)
                .await;
        let g_t1 = run_mesh_pubsub_htl_gossipsub_baseline(
            cfg.clone(),
            MESH_EVENT_POLICY.max_htl,
            6,
            Some(1),
        )
        .await;
        let g_v11 = run_mesh_pubsub_htl_gossipsub_v11_baseline(
            cfg.clone(),
            MESH_EVENT_POLICY.max_htl,
            6,
            Some(1),
            4,
        )
        .await;
        for (label, r) in [
            ("flood", &f),
            ("invwant", &i),
            ("plumtree-tInf", &pt_inf),
            ("plumtree-t1", &pt_t1),
            ("gossipsub-no-t", &g_no_t),
            ("gossipsub-t1", &g_t1),
            ("gossipsub-v11", &g_v11),
        ] {
            println!(
                " {:>5.2} | {:<15} | {:>8.2}% | {:>11.1} | {:>6} | {:>5} | {:>5}",
                broken,
                label,
                r.delivery_rate * 100.0,
                r.bytes_sent_per_delivered_event,
                r.duplicate_deliveries,
                r.delivery_latency_p50_ms,
                r.delivery_latency_p95_ms
            );
        }
    }
}
