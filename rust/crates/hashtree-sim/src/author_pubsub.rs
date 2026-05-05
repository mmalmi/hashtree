//! Deterministic simulation for author-key pubsub trees.
//!
//! This is intentionally separate from production mesh forwarding. It models the
//! small protocol shape we want to evaluate first: leased author interests,
//! bounded fanout, redundant parents, and local admission based on social trust
//! plus reciprocal bandwidth credit.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionPolicy {
    Open,
    SocialReciprocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeBehavior {
    Honest,
    DropsPublications,
}

#[derive(Debug, Clone)]
pub struct PubsubConfig {
    pub max_children_per_author: usize,
    pub max_parents_per_author: usize,
    pub admission_policy: AdmissionPolicy,
    pub anonymous_free_credit_bytes: u64,
    pub social_credit_bytes: u64,
    pub reciprocal_credit_multiplier: f64,
    pub subscription_cost_bytes: u64,
}

impl Default for PubsubConfig {
    fn default() -> Self {
        Self {
            max_children_per_author: 8,
            max_parents_per_author: 1,
            admission_policy: AdmissionPolicy::SocialReciprocal,
            anonymous_free_credit_bytes: 4 * 1024,
            social_credit_bytes: 64 * 1024,
            reciprocal_credit_multiplier: 1.0,
            subscription_cost_bytes: 1024,
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
    pub malicious_drops: u64,
    pub credit_drops: u64,
}

#[derive(Debug, Clone)]
struct Node {
    behavior: NodeBehavior,
    links: BTreeSet<String>,
    social_trust: BTreeMap<String, f64>,
    received: BTreeSet<(String, u64)>,
}

impl Node {
    fn new(behavior: NodeBehavior) -> Self {
        Self {
            behavior,
            links: BTreeSet::new(),
            social_trust: BTreeMap::new(),
            received: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct WireStats {
    bytes_sent: u64,
    bytes_received: u64,
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

    fn child_count(&self, parent: &str) -> usize {
        self.children.get(parent).map(BTreeSet::len).unwrap_or(0)
    }

    fn has_edge(&self, parent: &str, child: &str) -> bool {
        self.children
            .get(parent)
            .is_some_and(|children| children.contains(child))
    }

    fn parent_count(&self, child: &str) -> usize {
        self.parents.get(child).map(BTreeSet::len).unwrap_or(0)
    }

    fn tree_nodes(&self) -> BTreeSet<String> {
        let mut nodes = BTreeSet::from([self.publisher.clone()]);
        for (parent, children) in &self.children {
            nodes.insert(parent.clone());
            nodes.extend(children.iter().cloned());
        }
        nodes
    }
}

#[derive(Debug, Default)]
pub struct AuthorPubsubSim {
    config: PubsubConfig,
    nodes: BTreeMap<String, Node>,
    authors: BTreeMap<String, AuthorState>,
    wire: BTreeMap<(String, String), WireStats>,
}

impl AuthorPubsubSim {
    pub fn new(config: PubsubConfig) -> Self {
        Self {
            config,
            nodes: BTreeMap::new(),
            authors: BTreeMap::new(),
            wire: BTreeMap::new(),
        }
    }

    pub fn add_node(&mut self, node_id: impl Into<String>, behavior: NodeBehavior) {
        self.nodes.insert(node_id.into(), Node::new(behavior));
    }

    pub fn link(&mut self, a: &str, b: &str) {
        if let Some(node) = self.nodes.get_mut(a) {
            node.links.insert(b.to_string());
        }
        if let Some(node) = self.nodes.get_mut(b) {
            node.links.insert(a.to_string());
        }
    }

    pub fn set_social_trust(&mut self, from: &str, to: &str, trust: f64) {
        if let Some(node) = self.nodes.get_mut(from) {
            node.social_trust
                .insert(to.to_string(), trust.clamp(0.0, 1.0));
        }
    }

    pub fn record_bytes_received(&mut self, node_id: &str, from_peer: &str, bytes: u64) {
        let entry = self
            .wire
            .entry((node_id.to_string(), from_peer.to_string()))
            .or_default();
        entry.bytes_received = entry.bytes_received.saturating_add(bytes);
    }

    pub fn set_author_publisher(&mut self, author: &str, publisher: &str) {
        self.authors
            .insert(author.to_string(), AuthorState::new(publisher.to_string()));
    }

    pub fn subscribe(&mut self, node_id: &str, author: &str) -> bool {
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

    pub fn publish(&mut self, author: &str, seq: u64, bytes: u64) {
        let Some(publisher) = self
            .authors
            .get(author)
            .map(|state| state.publisher.clone())
        else {
            return;
        };

        let mut seen = BTreeSet::from([publisher.clone()]);
        let mut queue = VecDeque::from([publisher]);

        if let Some(state) = self.authors.get_mut(author) {
            state.last_seq = Some(seq);
        }

        while let Some(parent) = queue.pop_front() {
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

            for child in children {
                if !self.can_send_bytes(&parent, &child, bytes) {
                    if let Some(state) = self.authors.get_mut(author) {
                        state.stats.credit_drops = state.stats.credit_drops.saturating_add(1);
                    }
                    continue;
                }

                self.record_wire_sent(&parent, &child, bytes);
                if let Some(state) = self.authors.get_mut(author) {
                    state.stats.forwarded_bytes = state.stats.forwarded_bytes.saturating_add(bytes);
                }

                let first_delivery = self
                    .nodes
                    .get_mut(&child)
                    .map(|node| node.received.insert((author.to_string(), seq)))
                    .unwrap_or(false);

                if first_delivery && seen.insert(child.clone()) {
                    queue.push_back(child);
                }
            }
        }
    }

    pub fn received(&self, node_id: &str, author: &str, seq: u64) -> bool {
        self.nodes
            .get(node_id)
            .is_some_and(|node| node.received.contains(&(author.to_string(), seq)))
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
        if !self.nodes.contains_key(target) {
            return None;
        }
        if state.parent_count(target) >= self.config.max_parents_per_author {
            return None;
        }

        let mut queue = VecDeque::new();
        for start in state.tree_nodes() {
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
            let Some(parent_node) = self.nodes.get(&parent) else {
                continue;
            };
            for child in &parent_node.links {
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
        if state.child_count(parent) >= self.config.max_children_per_author {
            return false;
        }
        if state.parent_count(child) >= self.config.max_parents_per_author {
            return false;
        }
        self.has_credit(parent, child, self.config.subscription_cost_bytes)
    }

    fn can_send_bytes(&self, parent: &str, child: &str, bytes: u64) -> bool {
        self.has_credit(parent, child, bytes)
    }

    fn has_credit(&self, parent: &str, child: &str, cost_bytes: u64) -> bool {
        if self.config.admission_policy == AdmissionPolicy::Open {
            return true;
        }
        self.peer_credit(parent, child) >= cost_bytes as f64
    }

    fn peer_credit(&self, parent: &str, child: &str) -> f64 {
        let wire = self
            .wire
            .get(&(parent.to_string(), child.to_string()))
            .cloned()
            .unwrap_or_default();
        let social_credit =
            self.social_score(parent, child) * self.config.social_credit_bytes as f64;
        let reciprocal_credit =
            wire.bytes_received as f64 * self.config.reciprocal_credit_multiplier;
        self.config.anonymous_free_credit_bytes as f64 + social_credit + reciprocal_credit
            - wire.bytes_sent as f64
    }

    fn social_score(&self, from: &str, to: &str) -> f64 {
        let direct = self
            .nodes
            .get(from)
            .and_then(|node| node.social_trust.get(to).copied())
            .unwrap_or(0.0);
        if direct > 0.0 {
            return direct.clamp(0.0, 1.0);
        }

        let mut best: f64 = 0.0;
        let Some(from_node) = self.nodes.get(from) else {
            return 0.0;
        };
        for (middle, from_to_middle) in &from_node.social_trust {
            let Some(middle_to_target) = self
                .nodes
                .get(middle)
                .and_then(|node| node.social_trust.get(to).copied())
            else {
                continue;
            };
            best = best.max(0.5 * from_to_middle.min(middle_to_target));
        }

        let inbound = self
            .nodes
            .get(to)
            .and_then(|node| node.social_trust.get(from).copied())
            .unwrap_or(0.0);
        best.max(0.25 * inbound).clamp(0.0, 1.0)
    }

    fn record_wire_sent(&mut self, from: &str, to: &str, bytes: u64) {
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
    }
}
