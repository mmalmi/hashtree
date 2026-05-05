use hashtree_sim::author_pubsub::{
    run_author_pubsub_sweep, run_author_pubsub_workload, AdmissionPolicy, AuthorPubsubSim,
    NodeBehavior, PubsubConfig, PubsubWorkloadConfig,
};

fn spam_ids() -> Vec<String> {
    (0..16).map(|i| format!("spam-{i:02}")).collect()
}

fn honest_ids() -> Vec<String> {
    (0..6).map(|i| format!("honest-{i:02}")).collect()
}

fn add_star_topology(sim: &mut AuthorPubsubSim, spam: &[String], honest: &[String]) {
    sim.add_node("source", NodeBehavior::Honest);
    sim.add_node("hub", NodeBehavior::Honest);
    sim.link("source", "hub");
    sim.record_bytes_received("source", "hub", 64 * 1024);

    for id in spam {
        sim.add_node(id, NodeBehavior::Honest);
        sim.link("hub", id);
    }
    for id in honest {
        sim.add_node(id, NodeBehavior::Honest);
        sim.link("hub", id);
        sim.record_bytes_received("hub", id, 64 * 1024);
    }

    sim.set_author_publisher("author-a", "source");
    assert!(sim.subscribe("hub", "author-a"));
}

#[test]
fn leased_tree_delivers_author_updates_to_subscribers() {
    let mut sim = AuthorPubsubSim::new(PubsubConfig {
        max_children_per_author: 2,
        admission_policy: AdmissionPolicy::Open,
        ..Default::default()
    });
    sim.add_node("source", NodeBehavior::Honest);
    sim.add_node("relay", NodeBehavior::Honest);
    sim.add_node("alice", NodeBehavior::Honest);
    sim.add_node("bob", NodeBehavior::Honest);
    sim.link("source", "relay");
    sim.link("relay", "alice");
    sim.link("relay", "bob");
    sim.set_author_publisher("author-a", "source");

    assert!(sim.subscribe("alice", "author-a"));
    assert!(sim.subscribe("bob", "author-a"));

    sim.publish("author-a", 1, 512);
    let report = sim.report("author-a");

    assert!(sim.received("alice", "author-a", 1));
    assert!(sim.received("bob", "author-a", 1));
    assert_eq!(report.subscribers, 2);
    assert_eq!(report.delivered_subscribers, 2);
    assert!(report.forwarded_bytes >= 1024);
}

#[test]
fn reciprocal_admission_keeps_spam_from_crowding_out_honest_subscribers() {
    let spam = spam_ids();
    let honest = honest_ids();

    let mut open = AuthorPubsubSim::new(PubsubConfig {
        max_children_per_author: 8,
        admission_policy: AdmissionPolicy::Open,
        ..Default::default()
    });
    add_star_topology(&mut open, &spam, &honest);
    for id in spam.iter().chain(honest.iter()) {
        open.subscribe(id, "author-a");
    }
    open.publish("author-a", 1, 1024);
    let open_report = open.report("author-a");

    let mut protected = AuthorPubsubSim::new(PubsubConfig {
        max_children_per_author: 8,
        admission_policy: AdmissionPolicy::Reciprocal,
        anonymous_free_credit_bytes: 0,
        ..Default::default()
    });
    add_star_topology(&mut protected, &spam, &honest);
    for id in spam.iter().chain(honest.iter()) {
        protected.subscribe(id, "author-a");
    }
    protected.publish("author-a", 1, 1024);
    let protected_report = protected.report("author-a");

    let open_honest = honest
        .iter()
        .filter(|id| open.received(id, "author-a", 1))
        .count();
    let protected_honest = honest
        .iter()
        .filter(|id| protected.received(id, "author-a", 1))
        .count();

    assert!(
        protected_honest > open_honest,
        "reciprocal admission should protect useful subscribers (open={open_honest}, protected={protected_honest})"
    );
    assert_eq!(protected_honest, honest.len());
    assert!(protected_report.rejected_subscriptions > open_report.rejected_subscriptions);
}

#[test]
fn reciprocal_credit_admits_a_stranger_who_has_served_bandwidth() {
    let mut sim = AuthorPubsubSim::new(PubsubConfig {
        max_children_per_author: 4,
        admission_policy: AdmissionPolicy::Reciprocal,
        anonymous_free_credit_bytes: 0,
        reciprocal_credit_multiplier: 2.0,
        ..Default::default()
    });
    sim.add_node("source", NodeBehavior::Honest);
    sim.add_node("hub", NodeBehavior::Honest);
    sim.add_node("useful-stranger", NodeBehavior::Honest);
    sim.add_node("leech", NodeBehavior::Honest);
    sim.link("source", "hub");
    sim.link("hub", "useful-stranger");
    sim.link("hub", "leech");
    sim.record_bytes_received("source", "hub", 64 * 1024);
    sim.set_author_publisher("author-a", "source");
    assert!(sim.subscribe("hub", "author-a"));

    sim.record_bytes_received("hub", "useful-stranger", 32 * 1024);

    assert!(sim.subscribe("useful-stranger", "author-a"));
    assert!(!sim.subscribe("leech", "author-a"));

    sim.publish("author-a", 1, 1024);
    assert!(sim.received("useful-stranger", "author-a", 1));
    assert!(!sim.received("leech", "author-a", 1));
}

#[test]
fn redundant_parents_survive_a_malicious_forwarder() {
    let mut sim = AuthorPubsubSim::new(PubsubConfig {
        max_children_per_author: 4,
        max_parents_per_author: 2,
        admission_policy: AdmissionPolicy::Open,
        ..Default::default()
    });
    sim.add_node("source", NodeBehavior::Honest);
    sim.add_node("bad", NodeBehavior::DropsPublications);
    sim.add_node("good", NodeBehavior::Honest);
    sim.add_node("alice", NodeBehavior::Honest);
    sim.link("source", "bad");
    sim.link("source", "good");
    sim.link("bad", "alice");
    sim.link("good", "alice");
    sim.set_author_publisher("author-a", "source");

    assert!(sim.subscribe("alice", "author-a"));

    sim.publish("author-a", 1, 1024);
    let report = sim.report("author-a");

    assert!(sim.received("alice", "author-a", 1));
    assert!(report.malicious_drops > 0);
}

#[test]
fn large_workload_is_deterministic_for_same_seed() {
    let config = PubsubWorkloadConfig {
        pubsub: PubsubConfig {
            max_children_per_author: 8,
            max_parents_per_author: 2,
            admission_policy: AdmissionPolicy::Reciprocal,
            anonymous_free_credit_bytes: 0,
            reciprocal_credit_multiplier: 1.0,
            subscription_cost_bytes: 1024,
        },
        seed: 1_337,
        node_count: 320,
        author_count: 8,
        subscriber_attempts_per_author: 90,
        publish_rounds: 5,
        target_degree: 10,
        reciprocal_provider_fraction: 0.80,
        reciprocal_credit_bytes: 512 * 1024,
        malicious_forwarder_fraction: 0.03,
        ..Default::default()
    };

    let first = run_author_pubsub_workload(config.clone());
    let second = run_author_pubsub_workload(config);

    assert_eq!(first, second);
    assert!(
        first.delivery_rate >= 0.70,
        "delivery_rate={}",
        first.delivery_rate
    );
    assert!(
        first.cooperative_delivery_rate >= 0.80,
        "cooperative_delivery_rate={}",
        first.cooperative_delivery_rate
    );
    assert!(first.tree_edges > 0);
}

#[test]
fn churn_workload_repairs_rejoined_reciprocal_subscribers() {
    let report = run_author_pubsub_workload(PubsubWorkloadConfig {
        pubsub: PubsubConfig {
            max_children_per_author: 8,
            max_parents_per_author: 2,
            admission_policy: AdmissionPolicy::Reciprocal,
            anonymous_free_credit_bytes: 0,
            reciprocal_credit_multiplier: 1.0,
            subscription_cost_bytes: 1024,
        },
        seed: 9_001,
        node_count: 260,
        author_count: 6,
        subscriber_attempts_per_author: 70,
        publish_rounds: 8,
        target_degree: 10,
        reciprocal_provider_fraction: 0.85,
        reciprocal_credit_bytes: 512 * 1024,
        malicious_forwarder_fraction: 0.04,
        churn_rate: 0.04,
        allow_rejoin: true,
        ..Default::default()
    });

    assert!(report.churn_leaves > 0);
    assert!(report.churn_rejoins > 0);
    assert!(
        report.cooperative_delivery_rate >= 0.45,
        "cooperative_delivery_rate={}",
        report.cooperative_delivery_rate
    );
}

#[test]
fn sweep_compares_open_and_reciprocal_policies_under_uncredited_pressure() {
    let base = PubsubWorkloadConfig {
        pubsub: PubsubConfig {
            max_children_per_author: 6,
            max_parents_per_author: 1,
            admission_policy: AdmissionPolicy::Open,
            anonymous_free_credit_bytes: 0,
            reciprocal_credit_multiplier: 1.0,
            subscription_cost_bytes: 1024,
        },
        seed: 44,
        node_count: 240,
        author_count: 6,
        subscriber_attempts_per_author: 220,
        publish_rounds: 3,
        payload_bytes: 1024,
        target_degree: 8,
        reciprocal_provider_fraction: 0.35,
        reciprocal_credit_bytes: 256 * 1024,
        malicious_forwarder_fraction: 0.0,
        churn_rate: 0.0,
        allow_rejoin: false,
        prefer_uncredited_subscribers: true,
    };
    let mut reciprocal = base.clone();
    reciprocal.pubsub.admission_policy = AdmissionPolicy::Reciprocal;

    let results = run_author_pubsub_sweep(&[base, reciprocal]);
    assert_eq!(results.len(), 2);

    let open = &results[0].report;
    let reciprocal = &results[1].report;

    assert!(reciprocal.rejected_subscriptions > open.rejected_subscriptions);
    assert!(reciprocal.forwarded_bytes < open.forwarded_bytes);
    assert!(
        reciprocal.cooperative_accepted_subscribers > 0,
        "reciprocal policy should still admit peers with prior served bandwidth"
    );
}
