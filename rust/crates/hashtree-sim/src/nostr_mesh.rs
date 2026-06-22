//! Nostr mesh simulation for p2p request/response flows.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};

use hashtree_network::{
    decrement_htl_with_policy, is_hashtree_root_kind, should_forward_htl, validate_mesh_frame,
    MeshNostrFrame, PeerHTLConfig, SignalingMessage, HASHTREE_KIND, MESH_EVENT_POLICY,
    MESH_MAX_HTL, NOSTR_KIND_HASHTREE,
};
use nostr::{
    Alphabet, Event, EventBuilder, EventId, Filter, Keys, Kind, PublicKey, SingleLetterTag, Tag,
    TagKind,
};

#[derive(Debug, Default, Clone)]
pub struct MeshStats {
    pub forwarded_reqs: usize,
    pub forwarded_events: usize,
    pub forwarded_replies: usize,
    pub forwarded_mesh_frames: usize,
    pub dropped_mesh_duplicates: usize,
    pub dropped_mesh_invalid: usize,
}

#[derive(Debug)]
pub struct NostrMesh {
    nodes: HashMap<String, Node>,
    links: HashMap<String, HashSet<String>>,
    htl_configs: HashMap<(String, String), PeerHTLConfig>,
    queues: HashMap<String, VecDeque<Envelope>>,
    stats: MeshStats,
}

#[derive(Debug)]
struct Node {
    keys: Keys,
    store: HashMap<EventId, Event>,
    seen_events: HashSet<EventId>,
    seen_mesh_frame_ids: HashSet<String>,
    seen_mesh_event_ids: HashSet<EventId>,
    seen_reqs: HashSet<(String, String)>,
    seen_replies: HashSet<(String, String, EventId)>,
    received: Vec<Event>,
    signaling: Vec<SignalingMessage>,
}

#[derive(Debug, Clone)]
enum Envelope {
    Publish {
        origin: String,
        sender: String,
        event: Event,
        ttl: u8,
    },
    Req {
        origin: String,
        sender: String,
        sub_id: String,
        filters: Vec<Filter>,
        ttl: u8,
    },
    Reply {
        origin: String,
        sender: String,
        sub_id: String,
        event: Event,
        ttl: u8,
    },
    MeshFrame {
        origin: String,
        sender: String,
        frame: MeshNostrFrame,
    },
}

impl NostrMesh {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            links: HashMap::new(),
            htl_configs: HashMap::new(),
            queues: HashMap::new(),
            stats: MeshStats::default(),
        }
    }
}

impl Default for NostrMesh {
    fn default() -> Self {
        Self::new()
    }
}

impl NostrMesh {
    pub fn add_node(&mut self, node_id: &str) -> PublicKey {
        let keys = Keys::generate();
        let node = Node {
            keys: keys.clone(),
            store: HashMap::new(),
            seen_events: HashSet::new(),
            seen_mesh_frame_ids: HashSet::new(),
            seen_mesh_event_ids: HashSet::new(),
            seen_reqs: HashSet::new(),
            seen_replies: HashSet::new(),
            received: Vec::new(),
            signaling: Vec::new(),
        };
        self.nodes.insert(node_id.to_string(), node);
        self.links.entry(node_id.to_string()).or_default();
        self.queues.entry(node_id.to_string()).or_default();
        keys.public_key()
    }

    pub fn pubkey(&self, node_id: &str) -> Option<PublicKey> {
        self.nodes.get(node_id).map(|node| node.keys.public_key())
    }

    pub fn link(&mut self, a: &str, b: &str) {
        self.links
            .entry(a.to_string())
            .or_default()
            .insert(b.to_string());
        self.links
            .entry(b.to_string())
            .or_default()
            .insert(a.to_string());
        self.htl_configs.insert(
            (a.to_string(), b.to_string()),
            Self::deterministic_htl_config(a, b),
        );
        self.htl_configs.insert(
            (b.to_string(), a.to_string()),
            Self::deterministic_htl_config(b, a),
        );
    }

    pub fn publish_hashtree_root(
        &mut self,
        node_id: &str,
        tree_name: &str,
        hash_hex: &str,
        ttl: u8,
    ) -> anyhow::Result<()> {
        let node = self
            .nodes
            .get(node_id)
            .ok_or_else(|| anyhow::anyhow!("unknown node"))?;
        let tags = vec![
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree".to_string()],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec![hash_hex.to_string()]),
        ];
        let event = EventBuilder::new(Kind::Custom(HASHTREE_KIND), "")
            .tags(tags)
            .sign_with_keys(&node.keys)?;
        self.publish_event(node_id, event, ttl);
        Ok(())
    }

    pub fn send_signaling(
        &mut self,
        node_id: &str,
        target_pubkey: &PublicKey,
        msg: &SignalingMessage,
        ttl: u8,
    ) -> anyhow::Result<()> {
        let frame_id = format!(
            "{}:{}",
            node_id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        self.send_signaling_with_frame_id(node_id, target_pubkey, msg, &frame_id, ttl)
    }

    pub fn send_signaling_with_frame_id(
        &mut self,
        node_id: &str,
        target_pubkey: &PublicKey,
        msg: &SignalingMessage,
        frame_id: &str,
        ttl: u8,
    ) -> anyhow::Result<()> {
        let node = self
            .nodes
            .get(node_id)
            .ok_or_else(|| anyhow::anyhow!("unknown node"))?;
        let content = serde_json::to_string(msg)?;
        let tags = vec![Tag::public_key(*target_pubkey)];
        let event = EventBuilder::new(Kind::Custom(NOSTR_KIND_HASHTREE), content)
            .tags(tags)
            .sign_with_keys(&node.keys)?;
        let frame =
            MeshNostrFrame::new_event_with_id(event, node_id, frame_id, ttl.clamp(1, MESH_MAX_HTL));
        self.publish_mesh_frame(node_id, frame);
        Ok(())
    }

    pub fn publish_event(&mut self, node_id: &str, event: Event, ttl: u8) {
        self.store_event(node_id, &event);
        self.enqueue_neighbors(
            node_id,
            Envelope::Publish {
                origin: node_id.to_string(),
                sender: node_id.to_string(),
                event,
                ttl,
            },
        );
    }

    fn publish_mesh_frame(&mut self, node_id: &str, frame: MeshNostrFrame) {
        // Track origin frame/event as already seen to mirror production dedupe.
        self.mark_mesh_seen(node_id, &frame);
        self.store_event(node_id, frame.event());
        self.enqueue_neighbors(
            node_id,
            Envelope::MeshFrame {
                origin: node_id.to_string(),
                sender: node_id.to_string(),
                frame,
            },
        );
    }

    pub fn request(&mut self, node_id: &str, sub_id: &str, filters: Vec<Filter>, ttl: u8) {
        let envelope = Envelope::Req {
            origin: node_id.to_string(),
            sender: node_id.to_string(),
            sub_id: sub_id.to_string(),
            filters,
            ttl,
        };
        self.enqueue_neighbors(node_id, envelope);
    }

    pub fn drain(&mut self, max_steps: usize) -> usize {
        let mut steps = 0;
        loop {
            let mut progressed = false;
            let node_ids: Vec<String> = self.queues.keys().cloned().collect();
            for node_id in node_ids {
                if steps >= max_steps {
                    return steps;
                }
                let queue = self.queues.entry(node_id.clone()).or_default();
                let Some(envelope) = queue.pop_front() else {
                    continue;
                };
                progressed = true;
                steps += 1;
                self.handle_envelope(&node_id, envelope);
            }
            if !progressed {
                break;
            }
        }
        steps
    }

    pub fn received_events(&self, node_id: &str) -> Vec<Event> {
        self.nodes
            .get(node_id)
            .map(|node| node.received.clone())
            .unwrap_or_default()
    }

    pub fn received_signaling(&self, node_id: &str) -> Vec<SignalingMessage> {
        self.nodes
            .get(node_id)
            .map(|node| node.signaling.clone())
            .unwrap_or_default()
    }

    pub fn resolve_hashtree_hash(&self, node_id: &str, tree_name: &str) -> Option<String> {
        let node = self.nodes.get(node_id)?;
        for event in &node.received {
            if !is_hashtree_root_kind(event.kind) {
                continue;
            }
            if !has_tag_value(event, "d", tree_name) {
                continue;
            }
            if let Some(hash) = get_tag_value(event, "hash") {
                return Some(hash);
            }
        }
        None
    }

    pub fn stats(&self) -> &MeshStats {
        &self.stats
    }

    fn handle_envelope(&mut self, node_id: &str, envelope: Envelope) {
        match envelope {
            Envelope::Publish {
                origin,
                sender,
                event,
                ttl,
            } => self.handle_publish(node_id, &origin, &sender, event, ttl),
            Envelope::Req {
                origin,
                sender,
                sub_id,
                filters,
                ttl,
            } => self.handle_req(node_id, &origin, &sender, &sub_id, filters, ttl),
            Envelope::Reply {
                origin,
                sender,
                sub_id,
                event,
                ttl,
            } => self.handle_reply(node_id, &origin, &sender, &sub_id, event, ttl),
            Envelope::MeshFrame {
                origin,
                sender,
                frame,
            } => self.handle_mesh_frame(node_id, &origin, &sender, frame),
        }
    }

    fn mark_mesh_seen(&mut self, node_id: &str, frame: &MeshNostrFrame) -> bool {
        let Some(node) = self.nodes.get_mut(node_id) else {
            return false;
        };

        if !node.seen_mesh_frame_ids.insert(frame.frame_id.clone()) {
            return false;
        }
        if !node.seen_mesh_event_ids.insert(frame.event().id) {
            return false;
        }
        true
    }

    fn handle_publish(&mut self, node_id: &str, origin: &str, sender: &str, event: Event, ttl: u8) {
        let is_new = self.store_event(node_id, &event);
        if is_new && ttl > 0 {
            self.stats.forwarded_events += 1;
            self.enqueue_forward(
                node_id,
                sender,
                Envelope::Publish {
                    origin: origin.to_string(),
                    sender: node_id.to_string(),
                    event,
                    ttl: ttl - 1,
                },
            );
        }
    }

    fn handle_req(
        &mut self,
        node_id: &str,
        origin: &str,
        sender: &str,
        sub_id: &str,
        filters: Vec<Filter>,
        ttl: u8,
    ) {
        let req_key = (origin.to_string(), sub_id.to_string());
        let events = {
            let Some(node) = self.nodes.get_mut(node_id) else {
                return;
            };
            if !node.seen_reqs.insert(req_key.clone()) {
                return;
            }
            node.store.values().cloned().collect::<Vec<_>>()
        };

        for event in events {
            if filters
                .iter()
                .any(|filter| filter.match_event(&event, Default::default()))
            {
                let reply = Envelope::Reply {
                    origin: origin.to_string(),
                    sender: node_id.to_string(),
                    sub_id: sub_id.to_string(),
                    event,
                    ttl,
                };
                self.enqueue_forward(node_id, sender, reply);
            }
        }

        if ttl > 0 {
            self.stats.forwarded_reqs += 1;
            self.enqueue_forward(
                node_id,
                sender,
                Envelope::Req {
                    origin: origin.to_string(),
                    sender: node_id.to_string(),
                    sub_id: sub_id.to_string(),
                    filters,
                    ttl: ttl - 1,
                },
            );
        }
    }

    fn handle_reply(
        &mut self,
        node_id: &str,
        origin: &str,
        sender: &str,
        sub_id: &str,
        event: Event,
        ttl: u8,
    ) {
        let reply_key = (origin.to_string(), sub_id.to_string(), event.id);
        {
            let Some(node) = self.nodes.get_mut(node_id) else {
                return;
            };
            if !node.seen_replies.insert(reply_key.clone()) {
                return;
            }
        }

        if node_id == origin {
            self.store_event(node_id, &event);
            return;
        }

        if ttl > 0 {
            self.stats.forwarded_replies += 1;
            self.enqueue_forward(
                node_id,
                sender,
                Envelope::Reply {
                    origin: origin.to_string(),
                    sender: node_id.to_string(),
                    sub_id: sub_id.to_string(),
                    event,
                    ttl: ttl - 1,
                },
            );
        }
    }

    fn handle_mesh_frame(
        &mut self,
        node_id: &str,
        origin: &str,
        sender: &str,
        frame: MeshNostrFrame,
    ) {
        if validate_mesh_frame(&frame).is_err() {
            self.stats.dropped_mesh_invalid += 1;
            return;
        }

        if !self.mark_mesh_seen(node_id, &frame) {
            self.stats.dropped_mesh_duplicates += 1;
            return;
        }

        self.store_event(node_id, frame.event());
        let Some(neighbors) = self.links.get(node_id).cloned() else {
            return;
        };
        for neighbor in neighbors {
            if neighbor == sender {
                continue;
            }

            let cfg = self
                .htl_configs
                .get(&(node_id.to_string(), neighbor.clone()))
                .copied()
                .unwrap_or_else(|| Self::deterministic_htl_config(node_id, &neighbor));
            let next_htl = decrement_htl_with_policy(frame.htl, &MESH_EVENT_POLICY, &cfg);
            if !should_forward_htl(next_htl) {
                continue;
            }

            let mut outbound = frame.clone();
            outbound.htl = next_htl;
            self.stats.forwarded_mesh_frames += 1;
            self.queues
                .entry(neighbor)
                .or_default()
                .push_back(Envelope::MeshFrame {
                    origin: origin.to_string(),
                    sender: node_id.to_string(),
                    frame: outbound,
                });
        }
    }

    fn deterministic_htl_config(from: &str, to: &str) -> PeerHTLConfig {
        fn sample(from: &str, to: &str, salt: u8) -> f64 {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            from.hash(&mut hasher);
            to.hash(&mut hasher);
            salt.hash(&mut hasher);
            let value = hasher.finish();
            (value as f64) / (u64::MAX as f64)
        }

        PeerHTLConfig::from_samples(sample(from, to, 1), sample(from, to, 2))
    }

    fn store_event(&mut self, node_id: &str, event: &Event) -> bool {
        if let Some(node) = self.nodes.get_mut(node_id) {
            if node.seen_events.insert(event.id) {
                node.store.insert(event.id, event.clone());
                node.received.push(event.clone());
                if event.kind == Kind::Custom(NOSTR_KIND_HASHTREE) {
                    if let Some(msg) = decode_signaling_for(node, event) {
                        node.signaling.push(msg);
                    }
                }
                return true;
            }
        }
        false
    }

    fn enqueue_neighbors(&mut self, node_id: &str, envelope: Envelope) {
        let Some(neighbors) = self.links.get(node_id) else {
            return;
        };
        for neighbor in neighbors {
            self.queues
                .entry(neighbor.clone())
                .or_default()
                .push_back(envelope.clone());
        }
    }

    fn enqueue_forward(&mut self, node_id: &str, sender: &str, envelope: Envelope) {
        let Some(neighbors) = self.links.get(node_id) else {
            return;
        };
        for neighbor in neighbors {
            if neighbor == sender {
                continue;
            }
            self.queues
                .entry(neighbor.clone())
                .or_default()
                .push_back(envelope.clone());
        }
    }
}

fn get_tag_value(event: &Event, name: &str) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        let tag_vec = tag.as_slice();
        if tag_vec.len() >= 2 && tag_vec[0].as_str() == name {
            Some(tag_vec[1].clone())
        } else {
            None
        }
    })
}

fn has_tag_value(event: &Event, name: &str, value: &str) -> bool {
    event.tags.iter().any(|tag| {
        let tag_vec = tag.as_slice();
        tag_vec.len() >= 2 && tag_vec[0].as_str() == name && tag_vec[1].as_str() == value
    })
}

fn decode_signaling_for(node: &Node, event: &Event) -> Option<SignalingMessage> {
    if event.kind != Kind::Custom(NOSTR_KIND_HASHTREE) {
        return None;
    }
    let target = node.keys.public_key().to_hex();
    if !has_tag_value(event, "p", &target) {
        return None;
    }
    serde_json::from_str::<SignalingMessage>(&event.content).ok()
}
