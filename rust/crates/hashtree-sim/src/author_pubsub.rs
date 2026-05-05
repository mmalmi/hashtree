//! Deterministic simulation for author-key pubsub trees.
//!
//! This is intentionally separate from production mesh forwarding. It models the
//! small protocol shape we want to evaluate first: leased author interests,
//! bounded fanout, redundant parents, and publish scheduling that prioritizes
//! reciprocal bandwidth credit without blocking uncredited subscribers.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use hashtree_network::{
    reciprocal_virtual_finish, PeerTrafficSnapshot, PubsubCandidate, PubsubSchedulerConfig,
};

pub use hashtree_network::PubsubSchedulingPolicy as SchedulingPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeBehavior {
    Honest,
    DropsPublications,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PubsubConfig {
    pub max_children_per_author: usize,
    pub max_parents_per_author: usize,
    pub scheduling_policy: SchedulingPolicy,
    pub anonymous_free_credit_bytes: u64,
    pub reciprocal_credit_multiplier: f64,
    pub aging_credit_bytes: u64,
    pub max_outbound_bytes_per_node_per_round: u64,
    pub latency_per_hop_ms: u64,
}

impl Default for PubsubConfig {
    fn default() -> Self {
        Self {
            max_children_per_author: 8,
            max_parents_per_author: 1,
            scheduling_policy: SchedulingPolicy::Reciprocal,
            anonymous_free_credit_bytes: 4 * 1024,
            reciprocal_credit_multiplier: 1.0,
            aging_credit_bytes: 2 * 1024,
            max_outbound_bytes_per_node_per_round: u64::MAX,
            latency_per_hop_ms: 25,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PubsubReport {
    pub subscribers: usize,
    pub delivered_subscribers: usize,
    pub requested_subscriptions: u64,
    pub accepted_edges: u64,
    pub rejected_subscriptions: u64,
    pub forwarded_bytes: u64,
    pub duplicate_deliveries: u64,
    pub duplicate_bytes: u64,
    pub malicious_drops: u64,
    pub deferred_sends: u64,
    pub budget_deferred_sends: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PubsubWorkloadConfig {
    pub pubsub: PubsubConfig,
    pub seed: u64,
    pub node_count: usize,
    pub author_count: usize,
    pub subscriber_attempts_per_author: usize,
    pub publish_rounds: usize,
    pub payload_bytes: u64,
    pub target_degree: usize,
    pub reciprocal_provider_fraction: f64,
    pub reciprocal_credit_bytes: u64,
    pub malicious_forwarder_fraction: f64,
    pub churn_rate: f64,
    pub allow_rejoin: bool,
    pub prefer_uncredited_subscribers: bool,
    pub spam_author_count: usize,
    pub spam_subscriber_attempts_per_author: usize,
    pub spam_publish_rounds_per_round: usize,
}

impl Default for PubsubWorkloadConfig {
    fn default() -> Self {
        Self {
            pubsub: PubsubConfig::default(),
            seed: 42,
            node_count: 100,
            author_count: 4,
            subscriber_attempts_per_author: 32,
            publish_rounds: 4,
            payload_bytes: 1024,
            target_degree: 8,
            reciprocal_provider_fraction: 0.75,
            reciprocal_credit_bytes: 256 * 1024,
            malicious_forwarder_fraction: 0.0,
            churn_rate: 0.0,
            allow_rejoin: false,
            prefer_uncredited_subscribers: false,
            spam_author_count: 0,
            spam_subscriber_attempts_per_author: 0,
            spam_publish_rounds_per_round: 0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PubsubWorkloadReport {
    pub seed: u64,
    pub node_count: usize,
    pub active_nodes: usize,
    pub author_count: usize,
    pub publish_rounds: usize,
    pub subscriber_attempts: u64,
    pub accepted_subscribers: u64,
    pub cooperative_attempts: u64,
    pub cooperative_accepted_subscribers: u64,
    pub delivery_opportunities: u64,
    pub delivered_events: u64,
    pub cooperative_delivery_opportunities: u64,
    pub cooperative_delivered_events: u64,
    pub delivery_rate: f64,
    pub cooperative_delivery_rate: f64,
    pub loss_rate: f64,
    pub accepted_edges: u64,
    pub rejected_subscriptions: u64,
    pub forwarded_bytes: u64,
    pub duplicate_deliveries: u64,
    pub duplicate_bytes: u64,
    pub bytes_per_delivered_event: f64,
    pub delivery_latency_p50_ms: u64,
    pub delivery_latency_p95_ms: u64,
    pub delivery_latency_max_ms: u64,
    pub malicious_drops: u64,
    pub deferred_sends: u64,
    pub budget_deferred_sends: u64,
    pub tree_edges: usize,
    pub churn_leaves: u64,
    pub churn_rejoins: u64,
    pub spam_author_count: usize,
    pub spam_publish_events: u64,
    pub spam_delivery_opportunities: u64,
    pub spam_delivered_events: u64,
    pub spam_delivery_rate: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PubsubSweepResult {
    pub config: PubsubWorkloadConfig,
    pub report: PubsubWorkloadReport,
}

#[derive(Debug, Clone)]
struct Node {
    behavior: NodeBehavior,
    active: bool,
    reciprocal_provider: bool,
    links: BTreeSet<String>,
    received: BTreeSet<(String, u64)>,
    received_hops: BTreeMap<(String, u64), u64>,
}

impl Node {
    fn new(behavior: NodeBehavior) -> Self {
        Self {
            behavior,
            active: true,
            reciprocal_provider: true,
            links: BTreeSet::new(),
            received: BTreeSet::new(),
            received_hops: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct WireStats {
    bytes_sent: u64,
    bytes_received: u64,
    useful_bytes_received: u64,
    bandwidth_debt: f64,
}

#[derive(Debug, Clone)]
struct ScheduledChildren {
    children: Vec<String>,
    deferred_children: Vec<String>,
    deferred_count: usize,
}

#[derive(Debug, Clone)]
struct AuthorState {
    publisher: String,
    explicit_subscribers: BTreeSet<String>,
    parents: BTreeMap<String, BTreeSet<String>>,
    children: BTreeMap<String, BTreeSet<String>>,
    last_seq: Option<u64>,
    stats: PubsubReport,
}

impl AuthorState {
    fn new(publisher: String) -> Self {
        Self {
            publisher,
            explicit_subscribers: BTreeSet::new(),
            parents: BTreeMap::new(),
            children: BTreeMap::new(),
            last_seq: None,
            stats: PubsubReport::default(),
        }
    }

    fn has_edge(&self, parent: &str, child: &str) -> bool {
        self.children
            .get(parent)
            .is_some_and(|children| children.contains(child))
    }

    fn parent_count(&self, child: &str) -> usize {
        self.parents.get(child).map(BTreeSet::len).unwrap_or(0)
    }

    fn reachable_tree_nodes(&self) -> BTreeSet<String> {
        let mut reachable = BTreeSet::new();
        let mut queue = VecDeque::from([self.publisher.clone()]);
        while let Some(node_id) = queue.pop_front() {
            if !reachable.insert(node_id.clone()) {
                continue;
            }
            if let Some(children) = self.children.get(&node_id) {
                queue.extend(children.iter().cloned());
            }
        }
        reachable
    }
}

#[derive(Debug, Default)]
pub struct AuthorPubsubSim {
    config: PubsubConfig,
    nodes: BTreeMap<String, Node>,
    authors: BTreeMap<String, AuthorState>,
    wire: BTreeMap<(String, String), WireStats>,
    deferred_by_edge: BTreeMap<(String, String), u64>,
    round_outbound_bytes: BTreeMap<String, u64>,
}

impl AuthorPubsubSim {
    pub fn new(config: PubsubConfig) -> Self {
        Self {
            config,
            nodes: BTreeMap::new(),
            authors: BTreeMap::new(),
            wire: BTreeMap::new(),
            deferred_by_edge: BTreeMap::new(),
            round_outbound_bytes: BTreeMap::new(),
        }
    }

    pub fn add_node(&mut self, node_id: impl Into<String>, behavior: NodeBehavior) {
        self.nodes.insert(node_id.into(), Node::new(behavior));
    }

    pub fn set_node_behavior(&mut self, node_id: &str, behavior: NodeBehavior) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.behavior = behavior;
        }
    }

    pub fn set_reciprocal_provider(&mut self, node_id: &str, enabled: bool) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.reciprocal_provider = enabled;
        }
    }

    pub fn link(&mut self, a: &str, b: &str) {
        if let Some(node) = self.nodes.get_mut(a) {
            node.links.insert(b.to_string());
        }
        if let Some(node) = self.nodes.get_mut(b) {
            node.links.insert(a.to_string());
        }
    }

    pub fn record_bytes_received(&mut self, node_id: &str, from_peer: &str, bytes: u64) {
        self.record_peer_bytes_received(node_id, from_peer, bytes, true);
    }

    pub fn record_raw_bytes_received(&mut self, node_id: &str, from_peer: &str, bytes: u64) {
        self.record_peer_bytes_received(node_id, from_peer, bytes, false);
    }

    fn record_peer_bytes_received(
        &mut self,
        node_id: &str,
        from_peer: &str,
        bytes: u64,
        useful_for_reciprocity: bool,
    ) {
        let entry = self
            .wire
            .entry((node_id.to_string(), from_peer.to_string()))
            .or_default();
        entry.bytes_received = entry.bytes_received.saturating_add(bytes);
        if useful_for_reciprocity {
            entry.useful_bytes_received = entry.useful_bytes_received.saturating_add(bytes);
        }
    }

    pub fn set_author_publisher(&mut self, author: &str, publisher: &str) {
        self.authors
            .insert(author.to_string(), AuthorState::new(publisher.to_string()));
    }

    pub fn set_node_active(&mut self, node_id: &str, active: bool) {
        let Some(node) = self.nodes.get_mut(node_id) else {
            return;
        };
        if node.active == active {
            return;
        }
        node.active = active;
        if !active {
            self.remove_tree_edges_for_node(node_id);
        }
    }

    pub fn subscribe(&mut self, node_id: &str, author: &str) -> bool {
        if !self.is_active(node_id) {
            return false;
        }
        let Some(state) = self.authors.get_mut(author) else {
            return false;
        };
        state.stats.requested_subscriptions = state.stats.requested_subscriptions.saturating_add(1);

        if state.publisher == node_id {
            state.explicit_subscribers.insert(node_id.to_string());
            return true;
        }

        let mut attached = 0usize;
        while attached < self.config.max_parents_per_author {
            let Some(path) = self.find_attach_path(author, node_id) else {
                break;
            };
            if path.len() < 2 {
                break;
            }
            self.attach_path(author, &path);
            attached += 1;
        }

        let Some(state) = self.authors.get_mut(author) else {
            return false;
        };
        if attached == 0 {
            state.stats.rejected_subscriptions =
                state.stats.rejected_subscriptions.saturating_add(1);
            return false;
        }

        state.explicit_subscribers.insert(node_id.to_string());
        true
    }

    pub fn begin_publish_round(&mut self) {
        self.round_outbound_bytes.clear();
    }

    pub fn publish(&mut self, author: &str, seq: u64, bytes: u64) {
        self.publish_with_reciprocity(author, seq, bytes, true);
    }

    fn publish_with_reciprocity(
        &mut self,
        author: &str,
        seq: u64,
        bytes: u64,
        useful_for_reciprocity: bool,
    ) {
        let Some(publisher) = self
            .authors
            .get(author)
            .map(|state| state.publisher.clone())
        else {
            return;
        };
        if !self.is_active(&publisher) {
            return;
        }

        let mut seen = BTreeSet::from([publisher.clone()]);
        let mut queue = VecDeque::from([(publisher, 0u64)]);

        if let Some(state) = self.authors.get_mut(author) {
            state.last_seq = Some(seq);
        }

        while let Some((parent, hop_count)) = queue.pop_front() {
            let children = self
                .authors
                .get(author)
                .and_then(|state| state.children.get(&parent).cloned())
                .unwrap_or_default();
            if children.is_empty() {
                continue;
            }

            if self
                .nodes
                .get(&parent)
                .is_some_and(|node| node.behavior == NodeBehavior::DropsPublications)
            {
                if let Some(state) = self.authors.get_mut(author) {
                    state.stats.malicious_drops = state
                        .stats
                        .malicious_drops
                        .saturating_add(children.len() as u64);
                }
                continue;
            }

            let selected_children =
                self.select_publish_children(author, seq, &parent, bytes, children);
            if let Some(state) = self.authors.get_mut(author) {
                state.stats.deferred_sends = state.stats.deferred_sends.saturating_add(
                    selected_children
                        .deferred_count
                        .try_into()
                        .unwrap_or(u64::MAX),
                );
            }
            for child in &selected_children.deferred_children {
                self.record_deferred_child(&parent, child);
            }

            for child in selected_children.children {
                if !self.is_active(&child) {
                    continue;
                }
                if !self.reserve_outbound_bytes(&parent, bytes) {
                    self.record_deferred_child(&parent, &child);
                    if let Some(state) = self.authors.get_mut(author) {
                        state.stats.deferred_sends = state.stats.deferred_sends.saturating_add(1);
                        state.stats.budget_deferred_sends =
                            state.stats.budget_deferred_sends.saturating_add(1);
                    }
                    continue;
                }

                let bandwidth_debt = self.peer_virtual_finish(&parent, &child, bytes);
                self.record_wire_sent(&parent, &child, bytes, useful_for_reciprocity);
                self.set_peer_bandwidth_debt(&parent, &child, bandwidth_debt);
                if let Some(state) = self.authors.get_mut(author) {
                    state.stats.forwarded_bytes = state.stats.forwarded_bytes.saturating_add(bytes);
                }

                let event_key = (author.to_string(), seq);
                let first_delivery = self
                    .nodes
                    .get_mut(&child)
                    .map(|node| {
                        let inserted = node.received.insert(event_key.clone());
                        if inserted {
                            node.received_hops
                                .insert(event_key.clone(), hop_count.saturating_add(1));
                        }
                        inserted
                    })
                    .unwrap_or(false);

                if first_delivery && seen.insert(child.clone()) {
                    self.clear_deferred_child(&parent, &child);
                    queue.push_back((child, hop_count.saturating_add(1)));
                } else {
                    self.clear_deferred_child(&parent, &child);
                    if let Some(state) = self.authors.get_mut(author) {
                        state.stats.duplicate_deliveries =
                            state.stats.duplicate_deliveries.saturating_add(1);
                        state.stats.duplicate_bytes =
                            state.stats.duplicate_bytes.saturating_add(bytes);
                    }
                }
            }
        }
    }

    pub fn received(&self, node_id: &str, author: &str, seq: u64) -> bool {
        self.nodes
            .get(node_id)
            .is_some_and(|node| node.received.contains(&(author.to_string(), seq)))
    }

    pub fn received_hops(&self, node_id: &str, author: &str, seq: u64) -> Option<u64> {
        self.nodes
            .get(node_id)
            .and_then(|node| node.received_hops.get(&(author.to_string(), seq)).copied())
    }

    pub fn report(&self, author: &str) -> PubsubReport {
        let Some(state) = self.authors.get(author) else {
            return PubsubReport::default();
        };
        let mut report = state.stats.clone();
        report.subscribers = state.explicit_subscribers.len();
        report.delivered_subscribers = state
            .last_seq
            .map(|seq| {
                state
                    .explicit_subscribers
                    .iter()
                    .filter(|node_id| self.received(node_id, author, seq))
                    .count()
            })
            .unwrap_or(0);
        report
    }

    pub fn author_ids(&self) -> Vec<String> {
        self.authors.keys().cloned().collect()
    }

    pub fn author_publisher(&self, author: &str) -> Option<&str> {
        self.authors
            .get(author)
            .map(|state| state.publisher.as_str())
    }

    pub fn subscribers(&self, author: &str) -> Vec<String> {
        self.authors
            .get(author)
            .map(|state| state.explicit_subscribers.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn is_active(&self, node_id: &str) -> bool {
        self.nodes.get(node_id).is_some_and(|node| node.active)
    }

    pub fn is_reciprocal_provider(&self, node_id: &str) -> bool {
        self.nodes
            .get(node_id)
            .is_some_and(|node| node.reciprocal_provider)
    }

    pub fn active_node_count(&self) -> usize {
        self.nodes.values().filter(|node| node.active).count()
    }

    pub fn tree_edge_count(&self) -> usize {
        self.authors
            .values()
            .map(|state| state.children.values().map(BTreeSet::len).sum::<usize>())
            .sum()
    }

    pub fn repair_author(&mut self, author: &str) {
        let subscribers = self.subscribers(author);
        for subscriber in subscribers {
            if self.is_active(&subscriber) && !self.is_reachable_from_publisher(author, &subscriber)
            {
                self.remove_parent_edges_for_node(author, &subscriber);
                let _ = self.subscribe(&subscriber, author);
            }
        }
    }

    fn attach_path(&mut self, author: &str, path: &[String]) {
        for edge in path.windows(2) {
            let parent = &edge[0];
            let child = &edge[1];
            let Some(state) = self.authors.get_mut(author) else {
                return;
            };
            if state.has_edge(parent, child) {
                continue;
            }
            state
                .children
                .entry(parent.clone())
                .or_default()
                .insert(child.clone());
            state
                .parents
                .entry(child.clone())
                .or_default()
                .insert(parent.clone());
            state.stats.accepted_edges = state.stats.accepted_edges.saturating_add(1);
        }
    }

    fn find_attach_path(&self, author: &str, target: &str) -> Option<Vec<String>> {
        let state = self.authors.get(author)?;
        if !self.is_active(target) {
            return None;
        }
        if state.parent_count(target) >= self.config.max_parents_per_author {
            return None;
        }

        let mut queue = VecDeque::new();
        for start in state.reachable_tree_nodes() {
            if start == target {
                continue;
            }
            queue.push_back(vec![start]);
        }

        let mut visited_edges = BTreeSet::<(String, String)>::new();
        while let Some(path) = queue.pop_front() {
            let Some(parent) = path.last().cloned() else {
                continue;
            };
            if !self.is_active(&parent) {
                continue;
            }
            let Some(parent_node) = self.nodes.get(&parent) else {
                continue;
            };
            for child in &parent_node.links {
                if !self.is_active(child) {
                    continue;
                }
                if path.contains(child) {
                    continue;
                }
                if child == target
                    && state
                        .parents
                        .get(child)
                        .is_some_and(|p| p.contains(&parent))
                {
                    continue;
                }
                if !visited_edges.insert((parent.clone(), child.clone())) {
                    continue;
                }
                if !self.can_admit_child(author, &parent, child) {
                    continue;
                }

                let mut next_path = path.clone();
                next_path.push(child.clone());
                if child == target {
                    return Some(next_path);
                }
                queue.push_back(next_path);
            }
        }

        None
    }

    fn can_admit_child(&self, author: &str, parent: &str, child: &str) -> bool {
        let Some(state) = self.authors.get(author) else {
            return false;
        };
        if state.has_edge(parent, child) {
            return true;
        }
        if state.parent_count(child) >= self.config.max_parents_per_author {
            return false;
        }
        true
    }

    fn select_publish_children(
        &self,
        author: &str,
        seq: u64,
        parent: &str,
        bytes: u64,
        children: BTreeSet<String>,
    ) -> ScheduledChildren {
        let candidates = children
            .into_iter()
            .filter(|child| self.is_active(child))
            .map(|child| PubsubCandidate {
                deferred_count: self
                    .deferred_by_edge
                    .get(&(parent.to_string(), child.clone()))
                    .copied()
                    .unwrap_or_default(),
                traffic: self.peer_traffic_snapshot(parent, &child),
                peer_id: child,
            })
            .collect::<Vec<_>>();
        let scheduler = PubsubSchedulerConfig {
            policy: self.config.scheduling_policy,
            fanout: self.config.max_children_per_author,
            anonymous_free_credit_bytes: self.config.anonymous_free_credit_bytes,
            reciprocal_credit_multiplier: self.config.reciprocal_credit_multiplier,
            aging_credit_bytes: self.config.aging_credit_bytes,
        };
        let selection = scheduler.select(author, seq, parent, bytes, &candidates);
        let deferred_count = selection.deferred.len();
        ScheduledChildren {
            children: selection.selected,
            deferred_children: selection.deferred,
            deferred_count,
        }
    }

    fn peer_traffic_snapshot(&self, parent: &str, child: &str) -> PeerTrafficSnapshot {
        let wire = self
            .wire
            .get(&(parent.to_string(), child.to_string()))
            .cloned()
            .unwrap_or_default();
        PeerTrafficSnapshot {
            bytes_sent: wire.bytes_sent,
            bytes_received: wire.bytes_received,
            useful_bytes_received: wire.useful_bytes_received,
            bandwidth_debt: wire.bandwidth_debt,
        }
    }

    fn peer_virtual_finish(&self, parent: &str, child: &str, bytes: u64) -> f64 {
        reciprocal_virtual_finish(self.peer_traffic_snapshot(parent, child), bytes)
    }

    fn record_deferred_child(&mut self, parent: &str, child: &str) {
        let entry = self
            .deferred_by_edge
            .entry((parent.to_string(), child.to_string()))
            .or_default();
        *entry = entry.saturating_add(1);
    }

    fn clear_deferred_child(&mut self, parent: &str, child: &str) {
        self.deferred_by_edge
            .remove(&(parent.to_string(), child.to_string()));
    }

    fn reserve_outbound_bytes(&mut self, node_id: &str, bytes: u64) -> bool {
        let budget = self.config.max_outbound_bytes_per_node_per_round;
        if budget == u64::MAX {
            return true;
        }
        let used = self
            .round_outbound_bytes
            .get(node_id)
            .copied()
            .unwrap_or_default();
        if used.saturating_add(bytes) > budget {
            return false;
        }
        self.round_outbound_bytes
            .insert(node_id.to_string(), used.saturating_add(bytes));
        true
    }

    fn record_wire_sent(&mut self, from: &str, to: &str, bytes: u64, useful_for_reciprocity: bool) {
        let sent = self
            .wire
            .entry((from.to_string(), to.to_string()))
            .or_default();
        sent.bytes_sent = sent.bytes_sent.saturating_add(bytes);

        let received = self
            .wire
            .entry((to.to_string(), from.to_string()))
            .or_default();
        received.bytes_received = received.bytes_received.saturating_add(bytes);
        if useful_for_reciprocity {
            received.useful_bytes_received = received.useful_bytes_received.saturating_add(bytes);
        }
    }

    fn set_peer_bandwidth_debt(&mut self, from: &str, to: &str, bandwidth_debt: f64) {
        let entry = self
            .wire
            .entry((from.to_string(), to.to_string()))
            .or_default();
        entry.bandwidth_debt = bandwidth_debt;
    }

    fn is_reachable_from_publisher(&self, author: &str, node_id: &str) -> bool {
        self.authors
            .get(author)
            .is_some_and(|state| state.reachable_tree_nodes().contains(node_id))
    }

    fn remove_parent_edges_for_node(&mut self, author: &str, node_id: &str) {
        let Some(state) = self.authors.get_mut(author) else {
            return;
        };
        let Some(parents) = state.parents.remove(node_id) else {
            return;
        };
        for parent in parents {
            if let Some(children) = state.children.get_mut(&parent) {
                children.remove(node_id);
            }
        }
        state.children.retain(|_, children| !children.is_empty());
    }

    fn remove_tree_edges_for_node(&mut self, node_id: &str) {
        for state in self.authors.values_mut() {
            if let Some(parents) = state.parents.remove(node_id) {
                for parent in parents {
                    if let Some(children) = state.children.get_mut(&parent) {
                        children.remove(node_id);
                    }
                }
            }

            if let Some(children) = state.children.remove(node_id) {
                for child in children {
                    if let Some(parents) = state.parents.get_mut(&child) {
                        parents.remove(node_id);
                    }
                }
            }

            state.children.retain(|_, children| !children.is_empty());
            state.parents.retain(|_, parents| !parents.is_empty());
        }
    }
}

pub fn run_author_pubsub_workload(config: PubsubWorkloadConfig) -> PubsubWorkloadReport {
    if config.node_count == 0 || config.author_count == 0 || config.publish_rounds == 0 {
        return PubsubWorkloadReport {
            seed: config.seed,
            node_count: config.node_count,
            author_count: config.author_count,
            publish_rounds: config.publish_rounds,
            ..Default::default()
        };
    }

    let mut rng = StdRng::seed_from_u64(config.seed);
    let mut sim = AuthorPubsubSim::new(config.pubsub.clone());
    let node_ids = (0..config.node_count)
        .map(|i| format!("node-{i:04}"))
        .collect::<Vec<_>>();

    for node_id in &node_ids {
        sim.add_node(node_id, NodeBehavior::Honest);
        let is_provider = rng.gen::<f64>() < config.reciprocal_provider_fraction.clamp(0.0, 1.0);
        sim.set_reciprocal_provider(node_id, is_provider);
    }

    connect_workload_topology(&mut sim, &node_ids, config.target_degree, &mut rng);

    let total_author_count = config.author_count.saturating_add(config.spam_author_count);
    let mut publisher_pool = node_ids.clone();
    publisher_pool.shuffle(&mut rng);
    let publisher_ids = (0..total_author_count)
        .map(|i| publisher_pool[i % publisher_pool.len()].clone())
        .collect::<Vec<_>>();
    let useful_publisher_ids = publisher_ids
        .iter()
        .take(config.author_count)
        .cloned()
        .collect::<Vec<_>>();
    let spam_publisher_ids = publisher_ids
        .iter()
        .skip(config.author_count)
        .cloned()
        .collect::<Vec<_>>();
    let protected_publishers = publisher_ids.iter().cloned().collect::<BTreeSet<_>>();

    for publisher in &publisher_ids {
        sim.set_reciprocal_provider(publisher, true);
        sim.set_node_behavior(publisher, NodeBehavior::Honest);
    }

    for node_id in &node_ids {
        if protected_publishers.contains(node_id) {
            continue;
        }
        if rng.gen::<f64>() < config.malicious_forwarder_fraction.clamp(0.0, 1.0) {
            sim.set_node_behavior(node_id, NodeBehavior::DropsPublications);
        }
    }

    seed_reciprocal_link_credit(&mut sim, config.reciprocal_credit_bytes);

    let mut report = PubsubWorkloadReport {
        seed: config.seed,
        node_count: config.node_count,
        author_count: config.author_count,
        publish_rounds: config.publish_rounds,
        spam_author_count: config.spam_author_count,
        ..Default::default()
    };

    for (author_idx, publisher) in useful_publisher_ids.iter().enumerate() {
        let author = author_id(author_idx);
        sim.set_author_publisher(&author, publisher);

        let mut candidates = node_ids
            .iter()
            .filter(|node_id| *node_id != publisher)
            .cloned()
            .collect::<Vec<_>>();
        candidates.shuffle(&mut rng);
        if config.prefer_uncredited_subscribers {
            candidates.sort_by(|left, right| {
                sim.is_reciprocal_provider(left)
                    .cmp(&sim.is_reciprocal_provider(right))
                    .then_with(|| left.cmp(right))
            });
        }

        for subscriber in candidates.into_iter().take(
            config
                .subscriber_attempts_per_author
                .min(config.node_count - 1),
        ) {
            report.subscriber_attempts = report.subscriber_attempts.saturating_add(1);
            if sim.is_reciprocal_provider(&subscriber) {
                report.cooperative_attempts = report.cooperative_attempts.saturating_add(1);
            }
            let _ = sim.subscribe(&subscriber, &author);
        }
    }

    for (author_idx, publisher) in spam_publisher_ids.iter().enumerate() {
        let author = spam_author_id(author_idx);
        sim.set_author_publisher(&author, publisher);

        let mut candidates = node_ids
            .iter()
            .filter(|node_id| *node_id != publisher)
            .cloned()
            .collect::<Vec<_>>();
        candidates.shuffle(&mut rng);
        for subscriber in candidates.into_iter().take(
            config
                .spam_subscriber_attempts_per_author
                .min(config.node_count - 1),
        ) {
            let _ = sim.subscribe(&subscriber, &author);
        }
    }

    let mut delivery_latencies_ms = Vec::new();
    for round in 0..config.publish_rounds {
        if round > 0 && config.churn_rate > 0.0 {
            apply_workload_churn(
                &mut sim,
                &node_ids,
                &protected_publishers,
                &config,
                &mut rng,
                &mut report,
            );
        }

        for author in sim.author_ids() {
            sim.repair_author(&author);
        }

        sim.begin_publish_round();

        for spam_round in 0..config.spam_publish_rounds_per_round {
            let seq = (round * config.spam_publish_rounds_per_round + spam_round + 1) as u64;
            for author_idx in 0..config.spam_author_count {
                let author = spam_author_id(author_idx);
                report.spam_publish_events = report.spam_publish_events.saturating_add(1);
                sim.publish_with_reciprocity(&author, seq, config.payload_bytes, false);
                record_spam_delivery_round(&sim, &author, seq, &mut report);
            }
        }

        let seq = (round + 1) as u64;
        for author_idx in 0..config.author_count {
            let author = author_id(author_idx);
            sim.publish(&author, seq, config.payload_bytes);
            record_delivery_round(&sim, &author, seq, &mut report, &mut delivery_latencies_ms);
        }
    }

    finalize_workload_report(&sim, &mut report, &delivery_latencies_ms);
    report
}

pub fn run_author_pubsub_sweep(configs: &[PubsubWorkloadConfig]) -> Vec<PubsubSweepResult> {
    configs
        .iter()
        .cloned()
        .map(|config| {
            let report = run_author_pubsub_workload(config.clone());
            PubsubSweepResult { config, report }
        })
        .collect()
}

pub fn run_author_pubsub_htl_flood_baseline(
    config: PubsubWorkloadConfig,
    htl: usize,
) -> PubsubWorkloadReport {
    if config.node_count == 0 || config.author_count == 0 || config.publish_rounds == 0 {
        return PubsubWorkloadReport {
            seed: config.seed,
            node_count: config.node_count,
            author_count: config.author_count,
            publish_rounds: config.publish_rounds,
            spam_author_count: config.spam_author_count,
            ..Default::default()
        };
    }

    let mut rng = StdRng::seed_from_u64(config.seed);
    let mut sim = AuthorPubsubSim::new(config.pubsub.clone());
    let node_ids = (0..config.node_count)
        .map(|i| format!("node-{i:04}"))
        .collect::<Vec<_>>();

    for node_id in &node_ids {
        sim.add_node(node_id, NodeBehavior::Honest);
        let is_provider = rng.gen::<f64>() < config.reciprocal_provider_fraction.clamp(0.0, 1.0);
        sim.set_reciprocal_provider(node_id, is_provider);
    }

    connect_workload_topology(&mut sim, &node_ids, config.target_degree, &mut rng);

    let total_author_count = config.author_count.saturating_add(config.spam_author_count);
    let mut publisher_pool = node_ids.clone();
    publisher_pool.shuffle(&mut rng);
    let publisher_ids = (0..total_author_count)
        .map(|i| publisher_pool[i % publisher_pool.len()].clone())
        .collect::<Vec<_>>();
    let useful_publisher_ids = publisher_ids
        .iter()
        .take(config.author_count)
        .cloned()
        .collect::<Vec<_>>();
    let spam_publisher_ids = publisher_ids
        .iter()
        .skip(config.author_count)
        .cloned()
        .collect::<Vec<_>>();
    let protected_publishers = publisher_ids.iter().cloned().collect::<BTreeSet<_>>();

    for publisher in &publisher_ids {
        sim.set_reciprocal_provider(publisher, true);
        sim.set_node_behavior(publisher, NodeBehavior::Honest);
    }

    for node_id in &node_ids {
        if protected_publishers.contains(node_id) {
            continue;
        }
        if rng.gen::<f64>() < config.malicious_forwarder_fraction.clamp(0.0, 1.0) {
            sim.set_node_behavior(node_id, NodeBehavior::DropsPublications);
        }
    }

    let mut report = PubsubWorkloadReport {
        seed: config.seed,
        node_count: config.node_count,
        author_count: config.author_count,
        publish_rounds: config.publish_rounds,
        spam_author_count: config.spam_author_count,
        ..Default::default()
    };

    for (author_idx, publisher) in useful_publisher_ids.iter().enumerate() {
        let author = author_id(author_idx);
        sim.set_author_publisher(&author, publisher);
        let mut candidates = node_ids
            .iter()
            .filter(|node_id| *node_id != publisher)
            .cloned()
            .collect::<Vec<_>>();
        candidates.shuffle(&mut rng);
        if config.prefer_uncredited_subscribers {
            candidates.sort_by(|left, right| {
                sim.is_reciprocal_provider(left)
                    .cmp(&sim.is_reciprocal_provider(right))
                    .then_with(|| left.cmp(right))
            });
        }
        for subscriber in candidates.into_iter().take(
            config
                .subscriber_attempts_per_author
                .min(config.node_count - 1),
        ) {
            report.subscriber_attempts = report.subscriber_attempts.saturating_add(1);
            if sim.is_reciprocal_provider(&subscriber) {
                report.cooperative_attempts = report.cooperative_attempts.saturating_add(1);
            }
            add_explicit_subscriber(&mut sim, &author, &subscriber);
        }
    }

    for (author_idx, publisher) in spam_publisher_ids.iter().enumerate() {
        let author = spam_author_id(author_idx);
        sim.set_author_publisher(&author, publisher);
        let mut candidates = node_ids
            .iter()
            .filter(|node_id| *node_id != publisher)
            .cloned()
            .collect::<Vec<_>>();
        candidates.shuffle(&mut rng);
        for subscriber in candidates.into_iter().take(
            config
                .spam_subscriber_attempts_per_author
                .min(config.node_count - 1),
        ) {
            add_explicit_subscriber(&mut sim, &author, &subscriber);
        }
    }

    let mut delivery_latencies_ms = Vec::new();
    for _round in 0..config.publish_rounds {
        let spam_rounds = config.spam_publish_rounds_per_round;
        for _spam_round in 0..spam_rounds {
            for author_idx in 0..config.spam_author_count {
                let author = spam_author_id(author_idx);
                report.spam_publish_events = report.spam_publish_events.saturating_add(1);
                let delivered =
                    htl_flood_publish(&sim, &author, config.payload_bytes, htl, &mut report);
                record_spam_flood_delivery_round(&sim, &author, &delivered, &mut report);
            }
        }

        for author_idx in 0..config.author_count {
            let author = author_id(author_idx);
            let delivered =
                htl_flood_publish(&sim, &author, config.payload_bytes, htl, &mut report);
            record_flood_delivery_round(
                &sim,
                &author,
                &delivered,
                &mut report,
                &mut delivery_latencies_ms,
            );
        }
    }

    finalize_workload_report(&sim, &mut report, &delivery_latencies_ms);
    report
}

fn author_id(index: usize) -> String {
    format!("author-{index:04}")
}

fn spam_author_id(index: usize) -> String {
    format!("spam-author-{index:04}")
}

fn add_explicit_subscriber(sim: &mut AuthorPubsubSim, author: &str, subscriber: &str) {
    let Some(state) = sim.authors.get_mut(author) else {
        return;
    };
    state.explicit_subscribers.insert(subscriber.to_string());
}

fn htl_flood_publish(
    sim: &AuthorPubsubSim,
    author: &str,
    bytes: u64,
    htl: usize,
    report: &mut PubsubWorkloadReport,
) -> BTreeMap<String, u64> {
    let Some(publisher) = sim.author_publisher(author) else {
        return BTreeMap::new();
    };
    if !sim.is_active(publisher) {
        return BTreeMap::new();
    }

    let mut delivered_hops = BTreeMap::from([(publisher.to_string(), 0u64)]);
    let mut queue = VecDeque::from([(publisher.to_string(), 0usize, 0u64)]);
    while let Some((node_id, remaining_htl, hop_count)) = queue.pop_front() {
        if remaining_htl >= htl {
            continue;
        }
        let Some(node) = sim.nodes.get(&node_id) else {
            continue;
        };
        if node.behavior == NodeBehavior::DropsPublications {
            report.malicious_drops = report
                .malicious_drops
                .saturating_add(node.links.len() as u64);
            continue;
        }

        for neighbor in &node.links {
            if !sim.is_active(neighbor) {
                continue;
            }
            report.forwarded_bytes = report.forwarded_bytes.saturating_add(bytes);
            if delivered_hops.contains_key(neighbor) {
                report.duplicate_deliveries = report.duplicate_deliveries.saturating_add(1);
                report.duplicate_bytes = report.duplicate_bytes.saturating_add(bytes);
                continue;
            }
            let next_hop_count = hop_count.saturating_add(1);
            delivered_hops.insert(neighbor.clone(), next_hop_count);
            queue.push_back((
                neighbor.clone(),
                remaining_htl.saturating_add(1),
                next_hop_count,
            ));
        }
    }

    delivered_hops
}

fn connect_workload_topology(
    sim: &mut AuthorPubsubSim,
    node_ids: &[String],
    target_degree: usize,
    rng: &mut StdRng,
) {
    if node_ids.len() < 2 {
        return;
    }

    let ring_degree = target_degree.max(2).min(node_ids.len().saturating_sub(1));
    let ring_radius = (ring_degree / 2).max(1);
    let mut edges = BTreeSet::new();
    for i in 0..node_ids.len() {
        for offset in 1..=ring_radius {
            let j = (i + offset) % node_ids.len();
            let edge = ordered_edge(&node_ids[i], &node_ids[j]);
            if edges.insert(edge.clone()) {
                sim.link(&edge.0, &edge.1);
            }
        }
    }

    let max_edges = node_ids
        .len()
        .saturating_mul(node_ids.len().saturating_sub(1))
        / 2;
    let desired_edges = (node_ids.len().saturating_mul(target_degree.max(2)) / 2).min(max_edges);
    let max_attempts = desired_edges.saturating_mul(8).max(32);
    for _ in 0..max_attempts {
        if edges.len() >= desired_edges {
            break;
        }
        let a = rng.gen_range(0..node_ids.len());
        let b = rng.gen_range(0..node_ids.len());
        if a != b {
            let edge = ordered_edge(&node_ids[a], &node_ids[b]);
            if edges.insert(edge.clone()) {
                sim.link(&edge.0, &edge.1);
            }
        }
    }
}

fn seed_reciprocal_link_credit(sim: &mut AuthorPubsubSim, bytes: u64) {
    if bytes == 0 {
        return;
    }
    for (a, b) in unique_edges(sim) {
        if sim.is_reciprocal_provider(&a) {
            sim.record_bytes_received(&b, &a, bytes);
        }
        if sim.is_reciprocal_provider(&b) {
            sim.record_bytes_received(&a, &b, bytes);
        }
    }
}

fn unique_edges(sim: &AuthorPubsubSim) -> Vec<(String, String)> {
    let mut edges = BTreeSet::new();
    for (node_id, node) in &sim.nodes {
        for peer_id in &node.links {
            edges.insert(ordered_edge(node_id, peer_id));
        }
    }
    edges.into_iter().collect()
}

fn ordered_edge(a: &str, b: &str) -> (String, String) {
    if a < b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

fn apply_workload_churn(
    sim: &mut AuthorPubsubSim,
    node_ids: &[String],
    protected_publishers: &BTreeSet<String>,
    config: &PubsubWorkloadConfig,
    rng: &mut StdRng,
    report: &mut PubsubWorkloadReport,
) {
    let churn_rate = config.churn_rate.clamp(0.0, 1.0);
    for node_id in node_ids {
        if protected_publishers.contains(node_id) {
            continue;
        }
        if sim.is_active(node_id) {
            if rng.gen::<f64>() < churn_rate {
                sim.set_node_active(node_id, false);
                report.churn_leaves = report.churn_leaves.saturating_add(1);
            }
        } else if config.allow_rejoin && rng.gen::<f64>() < churn_rate {
            sim.set_node_active(node_id, true);
            report.churn_rejoins = report.churn_rejoins.saturating_add(1);
        }
    }
}

fn record_delivery_round(
    sim: &AuthorPubsubSim,
    author: &str,
    seq: u64,
    report: &mut PubsubWorkloadReport,
    delivery_latencies_ms: &mut Vec<u64>,
) {
    for subscriber in sim.subscribers(author) {
        if !sim.is_active(&subscriber) {
            continue;
        }
        report.delivery_opportunities = report.delivery_opportunities.saturating_add(1);
        if sim.is_reciprocal_provider(&subscriber) {
            report.cooperative_delivery_opportunities =
                report.cooperative_delivery_opportunities.saturating_add(1);
        }
        if sim.received(&subscriber, author, seq) {
            report.delivered_events = report.delivered_events.saturating_add(1);
            if let Some(hops) = sim.received_hops(&subscriber, author, seq) {
                delivery_latencies_ms.push(hops.saturating_mul(sim.config.latency_per_hop_ms));
            }
            if sim.is_reciprocal_provider(&subscriber) {
                report.cooperative_delivered_events =
                    report.cooperative_delivered_events.saturating_add(1);
            }
        }
    }
}

fn record_spam_delivery_round(
    sim: &AuthorPubsubSim,
    author: &str,
    seq: u64,
    report: &mut PubsubWorkloadReport,
) {
    for subscriber in sim.subscribers(author) {
        if !sim.is_active(&subscriber) {
            continue;
        }
        report.spam_delivery_opportunities = report.spam_delivery_opportunities.saturating_add(1);
        if sim.received(&subscriber, author, seq) {
            report.spam_delivered_events = report.spam_delivered_events.saturating_add(1);
        }
    }
}

fn record_flood_delivery_round(
    sim: &AuthorPubsubSim,
    author: &str,
    delivered_hops: &BTreeMap<String, u64>,
    report: &mut PubsubWorkloadReport,
    delivery_latencies_ms: &mut Vec<u64>,
) {
    for subscriber in sim.subscribers(author) {
        if !sim.is_active(&subscriber) {
            continue;
        }
        report.delivery_opportunities = report.delivery_opportunities.saturating_add(1);
        if sim.is_reciprocal_provider(&subscriber) {
            report.cooperative_delivery_opportunities =
                report.cooperative_delivery_opportunities.saturating_add(1);
        }
        if let Some(hops) = delivered_hops.get(&subscriber) {
            report.delivered_events = report.delivered_events.saturating_add(1);
            delivery_latencies_ms.push(hops.saturating_mul(sim.config.latency_per_hop_ms));
            if sim.is_reciprocal_provider(&subscriber) {
                report.cooperative_delivered_events =
                    report.cooperative_delivered_events.saturating_add(1);
            }
        }
    }
}

fn record_spam_flood_delivery_round(
    sim: &AuthorPubsubSim,
    author: &str,
    delivered_hops: &BTreeMap<String, u64>,
    report: &mut PubsubWorkloadReport,
) {
    for subscriber in sim.subscribers(author) {
        if !sim.is_active(&subscriber) {
            continue;
        }
        report.spam_delivery_opportunities = report.spam_delivery_opportunities.saturating_add(1);
        if delivered_hops.contains_key(&subscriber) {
            report.spam_delivered_events = report.spam_delivered_events.saturating_add(1);
        }
    }
}

fn finalize_workload_report(
    sim: &AuthorPubsubSim,
    report: &mut PubsubWorkloadReport,
    delivery_latencies_ms: &[u64],
) {
    report.active_nodes = sim.active_node_count();
    report.tree_edges = sim.tree_edge_count();

    for author in sim.author_ids() {
        let author_report = sim.report(&author);
        let useful_author = !author.starts_with("spam-author-");
        if useful_author {
            report.accepted_subscribers = report
                .accepted_subscribers
                .saturating_add(author_report.subscribers as u64);
            report.accepted_edges = report
                .accepted_edges
                .saturating_add(author_report.accepted_edges);
            report.rejected_subscriptions = report
                .rejected_subscriptions
                .saturating_add(author_report.rejected_subscriptions);
        }
        report.forwarded_bytes = report
            .forwarded_bytes
            .saturating_add(author_report.forwarded_bytes);
        report.duplicate_deliveries = report
            .duplicate_deliveries
            .saturating_add(author_report.duplicate_deliveries);
        report.duplicate_bytes = report
            .duplicate_bytes
            .saturating_add(author_report.duplicate_bytes);
        report.malicious_drops = report
            .malicious_drops
            .saturating_add(author_report.malicious_drops);
        report.deferred_sends = report
            .deferred_sends
            .saturating_add(author_report.deferred_sends);
        report.budget_deferred_sends = report
            .budget_deferred_sends
            .saturating_add(author_report.budget_deferred_sends);

        if useful_author {
            report.cooperative_accepted_subscribers =
                report.cooperative_accepted_subscribers.saturating_add(
                    sim.subscribers(&author)
                        .iter()
                        .filter(|subscriber| sim.is_reciprocal_provider(subscriber))
                        .count() as u64,
                );
        }
    }

    report.delivery_rate = ratio(report.delivered_events, report.delivery_opportunities);
    report.loss_rate = 1.0 - report.delivery_rate;
    report.cooperative_delivery_rate = ratio(
        report.cooperative_delivered_events,
        report.cooperative_delivery_opportunities,
    );
    report.bytes_per_delivered_event = if report.delivered_events == 0 {
        0.0
    } else {
        report.forwarded_bytes as f64 / report.delivered_events as f64
    };
    let mut sorted_latencies = delivery_latencies_ms.to_vec();
    sorted_latencies.sort_unstable();
    report.delivery_latency_p50_ms = percentile(&sorted_latencies, 0.50);
    report.delivery_latency_p95_ms = percentile(&sorted_latencies, 0.95);
    report.delivery_latency_max_ms = sorted_latencies.last().copied().unwrap_or_default();
    report.spam_delivery_rate = ratio(
        report.spam_delivered_events,
        report.spam_delivery_opportunities,
    );
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p.clamp(0.0, 1.0)).round() as usize;
    sorted[idx]
}
