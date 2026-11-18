use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use fips_core::Identity;
use hashtree_core::{
    decode_blob_request, encode_blob_reply_header, encode_blob_request, BlobReply, BlobRequest,
    BlobRoute, BlobRouteContext, MemoryStore, StoreBlobRoute, StoreError,
};
use hashtree_network::{BlobRouteEntry, BlobRouter, BlobRouterConfig, MeshForwardingRoute};
use tokio::time::{sleep, Duration};

use super::super::{FipsBlobRoute, PeerIdentity};
use super::PAYLOAD_BYTES;
use crate::recent_requesters::RecentRequesters;
use crate::tcp_blob::{verify_hash, INBOUND_BLOB_PEER};
use crate::TcpBlobTransportError;

pub(super) const HOP_LIMIT: u8 = 5;
pub(super) const SEARCH_DEADLINE: Duration = Duration::from_millis(700);
const PROVIDER_ATTEMPTS: usize = 4;

#[derive(Default)]
pub(super) struct Traffic {
    pub attempts: AtomicUsize,
    pub bytes: AtomicUsize,
    pub false_misses: AtomicUsize,
    pub corrupt_rejected: AtomicUsize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Behavior {
    Honest,
    FalseMiss,
    Corrupt,
}

pub(super) struct Node {
    recent_requesters: Arc<RecentRequesters>,
    pub store: Arc<MemoryStore>,
    pub router: Arc<BlobRouter>,
    pub online: Arc<AtomicBool>,
    behavior: Behavior,
}

impl Node {
    pub fn serves_blobs(&self) -> bool {
        self.behavior == Behavior::Honest && self.online.load(Relaxed)
    }
}

#[derive(Clone)]
struct Link {
    recent_requesters: Arc<RecentRequesters>,
    source: PeerIdentity,
    node: Weak<BlobRouter>,
    online: Arc<AtomicBool>,
    traffic: Arc<Traffic>,
    delay: Duration,
    stalled: bool,
    behavior: Behavior,
}

impl Link {
    async fn fetch(self, request: BlobRequest) -> Result<BlobReply, TcpBlobTransportError> {
        let wire = encode_blob_request(&request);
        self.traffic.attempts.fetch_add(1, Relaxed);
        self.traffic.bytes.fetch_add(wire.len(), Relaxed);
        if self.stalled {
            return std::future::pending().await;
        }
        sleep(self.delay).await;
        let router = self
            .node
            .upgrade()
            .filter(|_| self.online.load(Relaxed))
            .ok_or_else(|| StoreError::Other("simulated peer departed".into()))?;
        let request = decode_blob_request(&wire).unwrap();
        self.recent_requesters.record(request.hash, self.source);
        let reply = match self.behavior {
            Behavior::FalseMiss => BlobReply::NoResult,
            Behavior::Corrupt => BlobReply::Data(vec![255; PAYLOAD_BYTES]),
            Behavior::Honest => match INBOUND_BLOB_PEER
                .scope(
                    (request, self.source),
                    router.get_request(request, Some(&["store".into()]), None),
                )
                .await?
            {
                Some(data) => BlobReply::Data(data),
                None => BlobReply::NoResult,
            },
        };
        let bytes = encode_blob_reply_header(&reply).unwrap().len()
            + match &reply {
                BlobReply::Data(data) => data.len(),
                BlobReply::NoResult => 0,
            };
        self.traffic.bytes.fetch_add(bytes, Relaxed);
        sleep(self.delay).await;
        if self.behavior == Behavior::FalseMiss {
            self.traffic.false_misses.fetch_add(1, Relaxed);
        }
        if matches!(&reply, BlobReply::Data(data) if !verify_hash(data, &request.hash)) {
            self.traffic.corrupt_rejected.fetch_add(1, Relaxed);
            return Err(TcpBlobTransportError::HashMismatch);
        }
        Ok(reply)
    }
}

struct SimulatedProviderRoute {
    recent_requesters: Arc<RecentRequesters>,
    policy: FipsBlobRoute<MemoryStore>,
    links: HashMap<[u8; 16], Link>,
}

impl SimulatedProviderRoute {
    async fn fetch(
        &self,
        request: BlobRequest,
        context: Option<BlobRouteContext>,
    ) -> Result<BlobReply, StoreError> {
        let budget = context.map_or(PROVIDER_ATTEMPTS, |context| context.attempt_budget);
        let window = self.policy.provider_window(request.hash, budget)?;
        window
            .race(request, context, &self.recent_requesters, |peer| {
                let link = self.links[peer.node_addr().as_bytes()].clone();
                let integrity = self.policy.integrity.clone();
                async move { integrity.check_reply(peer, link.fetch(request).await) }
            })
            .await
    }
}

#[async_trait]
impl BlobRoute for SimulatedProviderRoute {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        self.fetch(request, None).await
    }

    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        self.fetch(request, Some(context)).await
    }
}

#[derive(Clone, Copy)]
struct Edge {
    to: usize,
    delay: Duration,
    stalled: bool,
}

pub(super) struct Network {
    pub nodes: Vec<Node>,
    pub traffic: Arc<Traffic>,
    edges: Vec<Vec<Edge>>,
}

impl Network {
    pub async fn new(
        identities: &[PeerIdentity],
        seed: u64,
        disturbances: bool,
        adversarial: bool,
    ) -> Self {
        let traffic = Arc::new(Traffic::default());
        let size = identities.len();
        let nodes = (0..size)
            .map(|index| {
                let store = Arc::new(MemoryStore::new());
                let router = BlobRouter::new(
                    vec![],
                    Some(store.clone()),
                    BlobRouterConfig {
                        request_timeout: SEARCH_DEADLINE,
                        ..BlobRouterConfig::default()
                    },
                )
                .unwrap();
                let behavior = match (adversarial, mix(seed ^ 0xa700 ^ index as u64) % 10) {
                    (true, 0) => Behavior::FalseMiss,
                    (true, 1) => Behavior::Corrupt,
                    _ => Behavior::Honest,
                };
                Node {
                    recent_requesters: Arc::default(),
                    store,
                    router: Arc::new(router),
                    online: Arc::new(AtomicBool::new(true)),
                    behavior,
                }
            })
            .collect::<Vec<_>>();
        let edges = topology(size, seed, disturbances);
        for ((source, node), edges) in identities.iter().zip(&nodes).zip(&edges) {
            let peers = edges.iter().map(|edge| identities[edge.to]).collect();
            let links = edges
                .iter()
                .map(|edge| {
                    let remote = &nodes[edge.to];
                    (
                        *identities[edge.to].node_addr().as_bytes(),
                        Link {
                            recent_requesters: remote.recent_requesters.clone(),
                            source: *source,
                            node: Arc::downgrade(&remote.router),
                            online: remote.online.clone(),
                            traffic: traffic.clone(),
                            delay: edge.delay,
                            stalled: edge.stalled,
                            behavior: remote.behavior,
                        },
                    )
                })
                .collect();
            let policy = FipsBlobRoute::new(None, Weak::new(), peers, PROVIDER_ATTEMPTS).unwrap();
            let mesh = MeshForwardingRoute::new(Arc::new(SimulatedProviderRoute {
                recent_requesters: node.recent_requesters.clone(),
                policy,
                links,
            }));
            node.router
                .set_routes(vec![
                    BlobRouteEntry::new("store", Arc::new(StoreBlobRoute::new(node.store.clone()))),
                    BlobRouteEntry::new("mesh", Arc::new(mesh)),
                ])
                .await
                .unwrap();
        }
        Self {
            nodes,
            traffic,
            edges,
        }
    }

    pub fn reachable(&self, start: usize, holders: &[usize]) -> bool {
        let mut queue = VecDeque::from([(start, 0)]);
        let mut seen = vec![false; self.nodes.len()];
        seen[start] = true;
        while let Some((node, hops)) = queue.pop_front() {
            if holders.contains(&node) {
                return true;
            }
            if hops == HOP_LIMIT {
                continue;
            }
            for edge in &self.edges[node] {
                if !edge.stalled && !seen[edge.to] && self.nodes[edge.to].serves_blobs() {
                    seen[edge.to] = true;
                    queue.push_back((edge.to, hops + 1));
                }
            }
        }
        false
    }
}

pub(super) fn identity(node: usize) -> PeerIdentity {
    let mut fixture_key = [0; 32];
    fixture_key[24..].copy_from_slice(&((node + 1) as u64).to_be_bytes());
    let identity = Identity::from_secret_bytes(&fixture_key).unwrap();
    PeerIdentity::from_pubkey(identity.pubkey())
}

fn topology(size: usize, seed: u64, disturbances: bool) -> Vec<Vec<Edge>> {
    let mut peers = vec![Vec::new(); size];
    for node in 0..size {
        let shortcut = mix(seed ^ node as u64) as usize % size;
        for other in [(node + 1) % size, (node + 2) % size, shortcut] {
            if node != other {
                peers[node].push(other);
                peers[other].push(node);
            }
        }
    }
    peers
        .into_iter()
        .enumerate()
        .map(|(node, mut peers)| {
            peers.sort_unstable();
            peers.dedup();
            let link = |other| mix((seed << 32) ^ ((node as u64) << 16) ^ other as u64);
            peers.sort_by_key(|&other| link(other));
            peers
                .into_iter()
                .map(|to| {
                    let sample = link(to);
                    Edge {
                        to,
                        delay: Duration::from_millis(6 + sample % 19)
                            + Duration::from_millis(if disturbances && sample.is_multiple_of(11) {
                                120
                            } else {
                                0
                            }),
                        stalled: disturbances && sample.is_multiple_of(23),
                    }
                })
                .collect()
        })
        .collect()
}

pub(super) fn mix(value: u64) -> u64 {
    let value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    let value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}
