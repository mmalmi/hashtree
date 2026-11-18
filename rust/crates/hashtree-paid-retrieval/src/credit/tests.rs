use super::*;
use crate::PaidBlobTransport;
use async_trait::async_trait;
use hashtree_core::{sha256, BlobReply, BlobRequest, BlobRoute, MemoryStore, Store};
use peer_payments::{Account, Book, Buyer, Limits, SpendingLimits};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

const PRICE: u64 = 2;

struct Pair {
    _dir: TempDir,
    buyer: CreditAccount,
    seller: CreditAccount,
    source: Arc<MemoryStore>,
    route: CreditBlobRoute,
    link: Arc<TestLink>,
}

struct TestLink {
    seller: CreditBlobProvider,
    payer: String,
    bytes: AtomicUsize,
    frames: AtomicUsize,
    fault: AtomicUsize,
}

#[async_trait]
impl PaidBlobTransport for TestLink {
    fn authenticates_peer(&self) -> bool {
        true
    }
    async fn exchange(&self, req: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        let mut reply = self.seller.handle(&self.payer, &req).await;
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes
            .fetch_add(req.len() + reply.len(), Ordering::Relaxed);
        match (
            self.fault.load(Ordering::Relaxed),
            decode::<Request>(REQUEST, &req)?,
        ) {
            (1, Request::Fetch { .. }) if reply.ends_with(DATA) => {
                reply[0] ^= 1;
            }
            (2, Request::Receipt { .. }) => return Err(failed("simulated lost acknowledgement")),
            _ => {}
        }
        Ok(reply)
    }
}

fn account(dir: &TempDir, name: &str, key: Keypair, accepted: String) -> CreditAccount {
    let owner = key.x_only_public_key().0.to_string();
    let book = Account::new(
        Book::new(&owner, Limits::default(), [accepted]).unwrap(),
        Buyer::new(&owner, SpendingLimits::default()).unwrap(),
    )
    .unwrap();
    let journal = Arc::new(Mutex::new(
        AccountStore::create(dir.path().join(name), book).unwrap(),
    ));
    CreditAccount::new(journal, key).unwrap()
}

fn pair(max: u64) -> Pair {
    let dir = tempfile::tempdir().unwrap();
    let key =
        |n| Keypair::from_seckey_slice(&secp256k1::Secp256k1::signing_only(), &[n; 32]).unwrap();
    let buyer = account(
        &dir,
        "buyer",
        key(1),
        key(2).x_only_public_key().0.to_string(),
    );
    let seller = account(&dir, "seller", key(2), buyer.owner.clone());
    let source = Arc::new(MemoryStore::new());
    let provider = CreditBlobProvider::new(source.clone(), seller.clone(), PRICE).unwrap();
    let link = Arc::new(TestLink {
        seller: provider,
        payer: buyer.owner.clone(),
        bytes: AtomicUsize::new(0),
        frames: AtomicUsize::new(0),
        fault: AtomicUsize::new(0),
    });
    let route = CreditBlobRoute::new(
        buyer.clone(),
        seller.owner.clone(),
        link.clone(),
        max,
        Arc::new(MemoryStore::new()),
    )
    .unwrap();
    Pair {
        _dir: dir,
        buyer,
        seller,
        source,
        link,
        route,
    }
}

fn request(hash: Hash) -> BlobRequest {
    BlobRequest { hash, htl: 0 }
}

#[tokio::test]
async fn verified_retrieval_is_bounded_across_duplicate_reads_and_acknowledgement_loss() {
    let p = pair(PRICE);
    let bytes = b"hash-bound".to_vec();
    let hash = sha256(&bytes);
    p.source.put(hash, bytes.clone()).await.unwrap();
    p.link.fault.store(2, Ordering::Relaxed);
    assert!(p.route.route(request(hash)).await.is_err());
    assert_eq!(
        p.seller
            .journal
            .lock()
            .unwrap()
            .book()
            .seller()
            .exposure()
            .unwrap()
            .outstanding_sat,
        PRICE
    );
    p.link.fault.store(0, Ordering::Relaxed);
    p.route.reconcile_pending().await.unwrap();
    let sent = p.link.frames.load(Ordering::Relaxed);
    p.route.reconcile_pending().await.unwrap();
    assert_eq!(p.link.frames.load(Ordering::Relaxed), sent);
    assert_eq!(
        p.buyer
            .journal
            .lock()
            .unwrap()
            .book()
            .buyer()
            .exposure()
            .unwrap(),
        (0, PRICE)
    );
    assert!(p.route.reconcile(hash).await.unwrap());
    let frames = p.link.frames.load(Ordering::Relaxed);
    assert_eq!(
        p.route.route(request(hash)).await.unwrap(),
        BlobReply::Data(bytes)
    );
    assert_eq!(p.link.frames.load(Ordering::Relaxed), frames);
}

#[tokio::test]
async fn substitution_cannot_earn_credit_and_replayed_requests_do_not_get_a_new_allowance() {
    let p = pair(PRICE);
    let bytes = b"blob".to_vec();
    let hash = sha256(&bytes);
    p.source.put(hash, bytes).await.unwrap();
    p.link.fault.store(1, Ordering::Relaxed);
    assert!(p.route.route(request(hash)).await.is_err());
    p.link.fault.store(0, Ordering::Relaxed);
    assert!(p.route.route(request(hash)).await.is_err());
    assert!(!p.route.reconcile(hash).await.unwrap());
    let e = p
        .seller
        .journal
        .lock()
        .unwrap()
        .book()
        .seller()
        .exposure()
        .unwrap();
    assert_eq!((e.outstanding_sat, e.reserved_sat), (0, PRICE));
}

#[tokio::test]
async fn wrong_peer_no_inventory_and_excessive_prices_no_obligation() {
    let p = pair(1);
    let hash = sha256(b"test");
    p.source.put(hash, b"test".to_vec()).await.unwrap();
    let quote = encode(
        REQUEST,
        &Request::Quote {
            hash,
            created_at: now().unwrap(),
        },
    )
    .unwrap();
    let rejected = p.link.seller.handle(&p.seller.owner, &quote).await;
    assert!(matches!(
        decode::<Reply>(CONTROL, &rejected).unwrap(),
        Reply::Refused
    ));
    assert!(p.route.route(request(hash)).await.is_err());
    assert_eq!(
        p.buyer
            .journal
            .lock()
            .unwrap()
            .book()
            .buyer()
            .exposure()
            .unwrap(),
        (0, 0)
    );
    assert_eq!(
        p.seller
            .journal
            .lock()
            .unwrap()
            .book()
            .seller()
            .exposure()
            .unwrap()
            .total_sat,
        0
    );
}

#[tokio::test]
async fn measure_purchases_and_cache_hits() {
    use std::time::Instant;
    let p = pair(PRICE);
    let mut blobs = Vec::new();
    for n in 0..48 {
        let bytes = vec![n; 256 * 1024];
        let hash = sha256(&bytes);
        p.source.put(hash, bytes).await.unwrap();
        blobs.push(hash);
    }
    let start = Instant::now();
    for hash in &blobs {
        assert!(matches!(
            p.route.route(request(*hash)).await.unwrap(),
            BlobReply::Data(_)
        ));
    }
    let paid = start.elapsed();
    let traffic = p.link.bytes.load(Ordering::Relaxed);
    let start = Instant::now();
    for hash in &blobs {
        assert!(matches!(
            p.route.route(request(*hash)).await.unwrap(),
            BlobReply::Data(_)
        ));
    }
    let cached = start.elapsed();
    assert_eq!(p.link.frames.load(Ordering::Relaxed), 3 * blobs.len());
    assert_eq!(p.link.bytes.load(Ordering::Relaxed), traffic);
    assert_eq!(
        p.seller
            .journal
            .lock()
            .unwrap()
            .book()
            .seller()
            .exposure()
            .unwrap()
            .outstanding_sat,
        blobs.len() as u64 * PRICE
    );
    let journal_bytes = std::fs::read_dir(p._dir.path())
        .unwrap()
        .map(|f| f.unwrap().metadata().unwrap().len())
        .sum::<u64>();
    println!("{} purchases, {} bytes, {} frames, journal_bytes={journal_bytes}, fresh={paid:?}, cached={cached:?}",blobs.len(),traffic,p.link.frames.load(Ordering::Relaxed));
}
