//! Deterministic topology probes using production routers, stores and wire codecs.
//! Only the carrier is simulated; byte counters exclude FIPS/TCP/underlay overhead.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use hashtree_core::{
    decode_blob_request, encode_blob_reply_header, encode_blob_request, sha256, BlobReply,
    BlobRequest, BlobRoute, BlobRouteContext, MemoryStore, Store, StoreBlobRoute, StoreError,
};
use hashtree_network::{BlobRouteEntry, BlobRouter, BlobRouterConfig, MeshForwardingRoute};

#[derive(Default)]
struct Traffic {
    htls: Mutex<Vec<u8>>,
    bytes: AtomicUsize,
}

struct Link {
    remote: Weak<BlobRouter>,
    traffic: Arc<Traffic>,
    corrupt: bool,
}

#[async_trait]
impl BlobRoute for Link {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        self.carry(request, None).await
    }

    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        self.carry(request, Some(context)).await
    }
}

impl Link {
    async fn carry(
        &self,
        request: BlobRequest,
        context: Option<BlobRouteContext>,
    ) -> Result<BlobReply, StoreError> {
        let wire = encode_blob_request(&request);
        self.traffic.bytes.fetch_add(wire.len(), Ordering::SeqCst);
        self.traffic.htls.lock().unwrap().push(request.htl);
        let request = decode_blob_request(&wire).unwrap();
        let remote = self
            .remote
            .upgrade()
            .ok_or_else(|| StoreError::Other("peer disconnected".into()))?;
        let mut reply = match remote.get_request(request, None, context).await? {
            Some(data) => BlobReply::Data(data),
            None => BlobReply::NoResult,
        };
        let payload = match &mut reply {
            BlobReply::Data(data) => {
                if self.corrupt && !data.is_empty() {
                    data[0] ^= 1;
                }
                data.len()
            }
            BlobReply::NoResult => 0,
        };
        self.traffic.bytes.fetch_add(
            encode_blob_reply_header(&reply).unwrap().len() + payload,
            Ordering::SeqCst,
        );
        Ok(reply)
    }
}

struct Node {
    store: Arc<MemoryStore>,
    router: Arc<BlobRouter>,
}

impl Node {
    fn new() -> Self {
        let store = Arc::new(MemoryStore::new());
        let router = Arc::new(
            BlobRouter::new(
                vec![local(&store)],
                Some(store.clone()),
                BlobRouterConfig::default(),
            )
            .unwrap(),
        );
        Self { store, router }
    }

    async fn peers(&self, peers: &[(&Node, bool)], traffic: &Arc<Traffic>) {
        let routes = peers
            .iter()
            .enumerate()
            .map(|(i, (node, corrupt))| {
                BlobRouteEntry::new(
                    format!("peer-{i}"),
                    Arc::new(Link {
                        remote: Arc::downgrade(&node.router),
                        traffic: traffic.clone(),
                        corrupt: *corrupt,
                    }),
                )
            })
            .collect();
        let peers = Arc::new(BlobRouter::new(routes, None, BlobRouterConfig::default()).unwrap());
        self.router
            .set_routes(vec![
                local(&self.store),
                BlobRouteEntry::new("mesh", Arc::new(MeshForwardingRoute::new(peers))),
            ])
            .await
            .unwrap();
    }
}

fn local(store: &Arc<MemoryStore>) -> BlobRouteEntry {
    BlobRouteEntry::new("local", Arc::new(StoreBlobRoute::new(store.clone())))
}

#[tokio::test]
async fn chains_preserve_hop_limits_and_verified_caches_through_provider_departure() {
    let data = vec![19; 4096];
    let hash = sha256(&data);
    for hops in [1, 2, 4, 8, 10] {
        let nodes: Vec<_> = (0..=hops).map(|_| Node::new()).collect();
        let traffic = Arc::new(Traffic::default());
        nodes[hops].store.put(hash, data.clone()).await.unwrap();
        for i in 0..hops {
            nodes[i].peers(&[(&nodes[i + 1], false)], &traffic).await;
        }
        let short = BlobRequest {
            hash,
            htl: hops as u8 - 1,
        };
        assert_eq!(
            nodes[0]
                .router
                .get_request(short, None, None)
                .await
                .unwrap(),
            None
        );
        assert_eq!(traffic.htls.lock().unwrap().len(), hops - 1);
        let before = traffic.bytes.load(Ordering::SeqCst);
        traffic.htls.lock().unwrap().clear();
        let request = BlobRequest {
            hash,
            htl: hops as u8,
        };
        assert_eq!(
            nodes[0]
                .router
                .get_request(request, None, None)
                .await
                .unwrap(),
            Some(data.clone())
        );
        assert_eq!(
            *traffic.htls.lock().unwrap(),
            (0..hops as u8).rev().collect::<Vec<_>>()
        );
        let cold_bytes = traffic.bytes.load(Ordering::SeqCst) - before;
        assert_eq!(cold_bytes, hops * (4096 + 36 + 7));
        for node in &nodes {
            assert_eq!(node.store.get(&hash).await.unwrap(), Some(data.clone()));
        }
        let before = traffic.bytes.load(Ordering::SeqCst);
        assert_eq!(
            nodes[0]
                .router
                .get_request(request, None, None)
                .await
                .unwrap(),
            Some(data.clone())
        );
        let warm_bytes = traffic.bytes.load(Ordering::SeqCst) - before;
        assert!(warm_bytes <= cold_bytes);
        // A cached terminal route still serves data when forwarding is impossible.
        nodes[hops].store.delete(&hash).await.unwrap();
        let before = traffic.bytes.load(Ordering::SeqCst);
        assert_eq!(
            nodes[0]
                .router
                .get_request(BlobRequest { hash, htl: 0 }, None, None)
                .await
                .unwrap(),
            Some(data.clone())
        );
        assert_eq!(traffic.bytes.load(Ordering::SeqCst), before);
        println!("chain hops={hops}: delivery=1/1 cold_blob_wire_bytes={cold_bytes} warm_blob_wire_bytes={warm_bytes} cached_htl0_bytes=0");
    }
}

#[tokio::test]
async fn cyclic_diamond_rejects_corruption_and_recovers_from_provider_churn() {
    let nodes: Vec<_> = (0..4).map(|_| Node::new()).collect();
    let traffic = Arc::new(Traffic::default());
    let data = vec![27; 4096];
    let hash = sha256(&data);
    nodes[1].store.put(hash, data.clone()).await.unwrap();
    nodes[3].store.put(hash, data.clone()).await.unwrap();
    nodes[0]
        .peers(&[(&nodes[1], true), (&nodes[2], false)], &traffic)
        .await;
    nodes[2]
        .peers(&[(&nodes[0], false), (&nodes[3], false)], &traffic)
        .await;
    assert_eq!(
        nodes[0]
            .router
            .get_request(BlobRequest { hash, htl: 10 }, None, None)
            .await
            .unwrap(),
        Some(data.clone())
    );
    assert_eq!(traffic.htls.lock().unwrap().len(), 4);
    assert_eq!(traffic.bytes.load(Ordering::SeqCst), 3 * (4096 + 43) + 43);
    assert_eq!(nodes[0].store.get(&hash).await.unwrap(), Some(data.clone()));
    assert_eq!(nodes[2].store.get(&hash).await.unwrap(), Some(data.clone()));
    nodes[3].store.delete(&hash).await.unwrap();
    nodes[2]
        .router
        .set_routes(vec![local(&nodes[2].store)])
        .await
        .unwrap();
    nodes[0].store.delete(&hash).await.unwrap();
    nodes[0].peers(&[(&nodes[2], false)], &traffic).await;
    let before = traffic.bytes.load(Ordering::SeqCst);
    assert_eq!(
        nodes[0]
            .router
            .get_request(BlobRequest { hash, htl: 1 }, None, None)
            .await
            .unwrap(),
        Some(data)
    );
    assert_eq!(traffic.bytes.load(Ordering::SeqCst) - before, 4096 + 43);
    println!("cyclic_diamond: delivery=1/1 link_attempts=4 cold_blob_wire_bytes=12460 churn_recovery_bytes=4139");
}
