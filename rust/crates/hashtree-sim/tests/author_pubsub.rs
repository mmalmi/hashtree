use hashtree_sim::author_pubsub::{AdmissionPolicy, AuthorPubsubSim, NodeBehavior, PubsubConfig};

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
    sim.set_social_trust("source", "hub", 1.0);

    for id in spam {
        sim.add_node(id, NodeBehavior::Honest);
        sim.link("hub", id);
    }
    for id in honest {
        sim.add_node(id, NodeBehavior::Honest);
        sim.link("hub", id);
        sim.set_social_trust("hub", id, 1.0);
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
fn social_reciprocal_admission_keeps_spam_from_crowding_out_honest_subscribers() {
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
        admission_policy: AdmissionPolicy::SocialReciprocal,
        anonymous_free_credit_bytes: 0,
        social_credit_bytes: 64 * 1024,
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
        "social admission should protect honest subscribers (open={open_honest}, protected={protected_honest})"
    );
    assert_eq!(protected_honest, honest.len());
    assert!(protected_report.rejected_subscriptions > open_report.rejected_subscriptions);
}

#[test]
fn reciprocal_credit_admits_a_stranger_who_has_served_bandwidth() {
    let mut sim = AuthorPubsubSim::new(PubsubConfig {
        max_children_per_author: 4,
        admission_policy: AdmissionPolicy::SocialReciprocal,
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
    sim.set_social_trust("source", "hub", 1.0);
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
