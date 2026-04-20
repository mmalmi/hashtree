use anyhow::{anyhow, Context, Result};
use cashu_service::{CashuMintBalance, CashuPaymentClient, CashuReceivedPayment, CashuSentPayment};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex, RwLock};

use crate::protocol::{DataChunk, DataQuoteRequest, DataQuoteResponse};
use crate::PeerSelector;

pub const CASHU_MINT_METADATA_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CashuMintMetadataRecord {
    pub successful_receipts: u64,
    pub failed_receipts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedCashuMintMetadata {
    version: u32,
    mints: HashMap<String, CashuMintMetadataRecord>,
}

pub fn cashu_mint_metadata_path(data_dir: &Path) -> PathBuf {
    data_dir.join("cashu").join("mint-metadata.json")
}

pub struct CashuMintMetadataStore {
    path: Option<PathBuf>,
    state: RwLock<HashMap<String, CashuMintMetadataRecord>>,
}

impl CashuMintMetadataStore {
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Self {
            path: None,
            state: RwLock::new(HashMap::new()),
        })
    }

    pub fn load(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let path = path.into();
        let state = if path.exists() {
            let content = fs::read_to_string(&path).with_context(|| {
                format!("Failed to read Cashu mint metadata from {}", path.display())
            })?;
            let snapshot: PersistedCashuMintMetadata =
                serde_json::from_str(&content).context("Failed to parse Cashu mint metadata")?;
            snapshot.mints
        } else {
            HashMap::new()
        };

        Ok(Arc::new(Self {
            path: Some(path),
            state: RwLock::new(state),
        }))
    }

    pub async fn get(&self, mint_url: &str) -> CashuMintMetadataRecord {
        self.state
            .read()
            .await
            .get(mint_url)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn record_receipt_success(&self, mint_url: &str) -> Result<()> {
        let mut state = self.state.write().await;
        state
            .entry(mint_url.to_string())
            .or_default()
            .successful_receipts += 1;
        self.persist_locked(&state)
    }

    pub async fn record_receipt_failure(&self, mint_url: &str) -> Result<()> {
        let mut state = self.state.write().await;
        state
            .entry(mint_url.to_string())
            .or_default()
            .failed_receipts += 1;
        self.persist_locked(&state)
    }

    pub async fn is_blocked(&self, mint_url: &str, threshold: u64) -> bool {
        if threshold == 0 {
            return false;
        }
        let record = self.get(mint_url).await;
        record.failed_receipts >= threshold && record.failed_receipts > record.successful_receipts
    }

    fn persist_locked(&self, state: &HashMap<String, CashuMintMetadataRecord>) -> Result<()> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create Cashu mint metadata directory {}",
                    parent.display()
                )
            })?;
        }

        let snapshot = PersistedCashuMintMetadata {
            version: CASHU_MINT_METADATA_VERSION,
            mints: state.clone(),
        };
        let content = serde_json::to_string_pretty(&snapshot)
            .context("Failed to encode Cashu mint metadata")?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, content).with_context(|| {
            format!(
                "Failed to write temporary Cashu mint metadata {}",
                tmp_path.display()
            )
        })?;
        fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "Failed to move Cashu mint metadata into place {}",
                path.display()
            )
        })?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashuRoutingConfig {
    pub accepted_mints: Vec<String>,
    pub default_mint: Option<String>,
    pub quote_payment_offer_sat: u64,
    pub quote_ttl_ms: u32,
    pub settlement_timeout_ms: u64,
    pub mint_failure_block_threshold: u64,
    pub peer_suggested_mint_base_cap_sat: u64,
    pub peer_suggested_mint_success_step_sat: u64,
    pub peer_suggested_mint_receipt_step_sat: u64,
    pub peer_suggested_mint_max_cap_sat: u64,
    pub payment_default_block_threshold: u64,
    pub chunk_target_bytes: usize,
}

impl Default for CashuRoutingConfig {
    fn default() -> Self {
        Self {
            accepted_mints: Vec::new(),
            default_mint: None,
            quote_payment_offer_sat: 3,
            quote_ttl_ms: 1_500,
            settlement_timeout_ms: 5_000,
            mint_failure_block_threshold: 2,
            peer_suggested_mint_base_cap_sat: 3,
            peer_suggested_mint_success_step_sat: 1,
            peer_suggested_mint_receipt_step_sat: 2,
            peer_suggested_mint_max_cap_sat: 21,
            payment_default_block_threshold: 0,
            chunk_target_bytes: 32 * 1024,
        }
    }
}

struct PendingQuoteRequest {
    response_tx: oneshot::Sender<Option<NegotiatedQuote>>,
    preferred_mint_url: Option<String>,
    offered_payment_sat: u64,
}

struct IssuedQuote {
    payment_sat: u64,
    mint_url: Option<String>,
    expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedSettlement {
    pub chunk_index: u32,
    pub payment_sat: u64,
    pub mint_url: Option<String>,
    pub final_chunk: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedQuote {
    pub peer_id: String,
    pub quote_id: u64,
    pub payment_sat: u64,
    pub mint_url: Option<String>,
}

struct OutgoingTransfer {
    chunks: Vec<Vec<u8>>,
    chunk_payments: Vec<u64>,
    mint_url: Option<String>,
    next_chunk_index: usize,
}

pub struct CashuQuoteState {
    routing: CashuRoutingConfig,
    peer_selector: Arc<RwLock<PeerSelector>>,
    payment_client: Option<Arc<dyn CashuPaymentClient>>,
    mint_metadata: Arc<CashuMintMetadataStore>,
    pending_quotes: Mutex<HashMap<String, PendingQuoteRequest>>,
    issued_quotes: Mutex<HashMap<(String, String, u64), IssuedQuote>>,
    pending_settlements: Mutex<HashMap<(String, String, u64), ExpectedSettlement>>,
    outgoing_transfers: Mutex<HashMap<(String, String, u64), OutgoingTransfer>>,
    next_quote_id: AtomicU64,
}

impl CashuQuoteState {
    pub fn new(
        routing: CashuRoutingConfig,
        peer_selector: Arc<RwLock<PeerSelector>>,
        payment_client: Option<Arc<dyn CashuPaymentClient>>,
    ) -> Self {
        Self::new_with_mint_metadata(
            routing,
            peer_selector,
            payment_client,
            CashuMintMetadataStore::in_memory(),
        )
    }

    pub fn new_with_mint_metadata(
        routing: CashuRoutingConfig,
        peer_selector: Arc<RwLock<PeerSelector>>,
        payment_client: Option<Arc<dyn CashuPaymentClient>>,
        mint_metadata: Arc<CashuMintMetadataStore>,
    ) -> Self {
        Self {
            routing,
            peer_selector,
            payment_client,
            mint_metadata,
            pending_quotes: Mutex::new(HashMap::new()),
            issued_quotes: Mutex::new(HashMap::new()),
            pending_settlements: Mutex::new(HashMap::new()),
            outgoing_transfers: Mutex::new(HashMap::new()),
            next_quote_id: AtomicU64::new(1),
        }
    }

    pub fn payment_client_available(&self) -> bool {
        self.payment_client.is_some()
    }

    pub async fn requester_quote_terms(&self) -> Option<(String, u64, u32)> {
        if !self.payment_client_available()
            || self.routing.quote_payment_offer_sat == 0
            || self.routing.quote_ttl_ms == 0
        {
            return None;
        }

        let mint_url = self
            .requested_quote_mint(self.routing.quote_payment_offer_sat)
            .await?;
        Some((
            mint_url,
            self.routing.quote_payment_offer_sat,
            self.routing.quote_ttl_ms,
        ))
    }

    pub fn settlement_timeout(&self) -> Duration {
        Duration::from_millis(self.routing.settlement_timeout_ms.max(1))
    }

    pub async fn requested_quote_mint(&self, amount_sat: u64) -> Option<String> {
        for mint_url in self.trusted_mint_candidates() {
            if self
                .is_mint_allowed_for_requester(&mint_url, amount_sat)
                .await
            {
                return Some(mint_url);
            }
        }
        None
    }

    pub async fn choose_quote_mint(&self, requested_mint: Option<&str>) -> Option<String> {
        if let Some(requested_mint) = requested_mint {
            if self.accepts_quote_mint(Some(requested_mint))
                && !self.is_mint_blocked(requested_mint).await
            {
                return Some(requested_mint.to_string());
            }
        }
        if let Some(default_mint) = self.routing.default_mint.as_ref() {
            if !self.is_mint_blocked(default_mint).await {
                return Some(default_mint.clone());
            }
        }
        for mint_url in &self.routing.accepted_mints {
            if !self.is_mint_blocked(mint_url).await {
                return Some(mint_url.clone());
            }
        }
        if self.routing.accepted_mints.is_empty() {
            if let Some(requested_mint) = requested_mint {
                if !self.is_mint_blocked(requested_mint).await {
                    return Some(requested_mint.to_string());
                }
            }
        }
        None
    }

    pub async fn register_pending_quote(
        &self,
        hash_hex: String,
        preferred_mint_url: Option<String>,
        offered_payment_sat: u64,
    ) -> oneshot::Receiver<Option<NegotiatedQuote>> {
        let (tx, rx) = oneshot::channel();
        self.pending_quotes.lock().await.insert(
            hash_hex,
            PendingQuoteRequest {
                response_tx: tx,
                preferred_mint_url,
                offered_payment_sat,
            },
        );
        rx
    }

    pub async fn clear_pending_quote(&self, hash_hex: &str) {
        let _ = self.pending_quotes.lock().await.remove(hash_hex);
    }

    pub async fn should_accept_quote_response(
        &self,
        from_peer: &str,
        preferred_mint_url: Option<&str>,
        offered_payment_sat: u64,
        res: &DataQuoteResponse,
    ) -> bool {
        let Some(payment_sat) = res.p else {
            return false;
        };
        if payment_sat > offered_payment_sat {
            return false;
        }

        let response_mint = res.m.as_deref();
        let Some(response_mint) = response_mint else {
            return false;
        };
        if self.is_mint_blocked(response_mint).await
            || !self
                .has_sufficient_balance(response_mint, payment_sat)
                .await
        {
            return false;
        }
        if preferred_mint_url == Some(response_mint) {
            return true;
        }
        if self.trusts_quote_mint(Some(response_mint)) {
            return true;
        }

        payment_sat <= self.peer_suggested_mint_cap_sat(from_peer).await
    }

    pub async fn handle_quote_response(&self, from_peer: &str, res: DataQuoteResponse) -> bool {
        if !res.a || !self.payment_client_available() {
            return false;
        }

        let (Some(quote_id), Some(payment_sat)) = (res.q, res.p) else {
            return false;
        };
        let hash_hex = hex::encode(&res.h);
        let (preferred_mint_url, offered_payment_sat) = {
            let pending_quotes = self.pending_quotes.lock().await;
            let Some(pending) = pending_quotes.get(&hash_hex) else {
                return false;
            };
            (
                pending.preferred_mint_url.clone(),
                pending.offered_payment_sat,
            )
        };

        if !self
            .should_accept_quote_response(
                from_peer,
                preferred_mint_url.as_deref(),
                offered_payment_sat,
                &res,
            )
            .await
        {
            return false;
        }

        let Some(pending) = self.pending_quotes.lock().await.remove(&hash_hex) else {
            return false;
        };
        let _ = pending.response_tx.send(Some(NegotiatedQuote {
            peer_id: from_peer.to_string(),
            quote_id,
            payment_sat,
            mint_url: res.m,
        }));
        true
    }

    pub async fn build_quote_response(
        &self,
        from_peer: &str,
        req: &DataQuoteRequest,
        can_serve: bool,
    ) -> DataQuoteResponse {
        if !can_serve || !self.payment_client_available() {
            return DataQuoteResponse {
                h: req.h.clone(),
                a: false,
                q: None,
                p: None,
                t: None,
                m: None,
            };
        }

        let Some(chosen_mint) = self.choose_quote_mint(req.m.as_deref()).await else {
            return DataQuoteResponse {
                h: req.h.clone(),
                a: false,
                q: None,
                p: None,
                t: None,
                m: None,
            };
        };
        let quote_id = self.next_quote_id.fetch_add(1, Ordering::Relaxed);
        let hash_hex = hex::encode(&req.h);
        self.issued_quotes.lock().await.insert(
            (from_peer.to_string(), hash_hex, quote_id),
            IssuedQuote {
                payment_sat: req.p,
                mint_url: Some(chosen_mint.clone()),
                expires_at: Instant::now() + Duration::from_millis(req.t as u64),
            },
        );

        DataQuoteResponse {
            h: req.h.clone(),
            a: true,
            q: Some(quote_id),
            p: Some(req.p),
            t: Some(req.t),
            m: Some(chosen_mint),
        }
    }

    pub async fn take_valid_quote(
        &self,
        from_peer: &str,
        hash: &[u8],
        quote_id: u64,
    ) -> Option<ExpectedSettlement> {
        let hash_hex = hex::encode(hash);
        let key = (from_peer.to_string(), hash_hex, quote_id);
        let issued = self.issued_quotes.lock().await.remove(&key)?;
        (issued.expires_at >= Instant::now()).then_some(ExpectedSettlement {
            chunk_index: 0,
            payment_sat: issued.payment_sat,
            mint_url: issued.mint_url,
            final_chunk: true,
        })
    }

    pub async fn register_expected_payment(
        self: &Arc<Self>,
        from_peer: String,
        hash_hex: String,
        quote_id: u64,
        settlement: ExpectedSettlement,
    ) {
        let key = (from_peer.clone(), hash_hex.clone(), quote_id);
        self.pending_settlements
            .lock()
            .await
            .insert(key.clone(), settlement);

        let state = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(state.settlement_timeout()).await;
            let expired = state
                .pending_settlements
                .lock()
                .await
                .remove(&key)
                .is_some();
            if expired {
                let _ = state.outgoing_transfers.lock().await.remove(&key);
                state
                    .peer_selector
                    .write()
                    .await
                    .record_cashu_payment_default(&from_peer);
            }
        });
    }

    pub async fn claim_expected_payment(
        &self,
        from_peer: &str,
        hash: &[u8],
        quote_id: u64,
        chunk_index: u32,
        announced_payment_sat: u64,
        announced_mint: Option<&str>,
    ) -> Result<ExpectedSettlement> {
        let hash_hex = hex::encode(hash);
        let key = (from_peer.to_string(), hash_hex, quote_id);
        let settlement = self
            .pending_settlements
            .lock()
            .await
            .remove(&key)
            .ok_or_else(|| anyhow!("No pending settlement"))?;

        if settlement.chunk_index != chunk_index {
            return Err(anyhow!("Payment chunk did not match the expected chunk"));
        }
        if announced_payment_sat < settlement.payment_sat {
            return Err(anyhow!("Quoted payment amount was not met"));
        }
        if settlement.mint_url.as_deref() != announced_mint {
            return Err(anyhow!("Payment mint does not match quoted mint"));
        }

        Ok(settlement)
    }

    pub async fn prepare_quoted_transfer(
        &self,
        from_peer: &str,
        hash: &[u8],
        quote_id: u64,
        settlement: &ExpectedSettlement,
        data: Vec<u8>,
    ) -> Option<(DataChunk, ExpectedSettlement)> {
        let hash_hex = hex::encode(hash);
        let transfer_key = (from_peer.to_string(), hash_hex, quote_id);
        let outgoing = OutgoingTransfer::new(
            data,
            self.routing.chunk_target_bytes.max(1),
            settlement.payment_sat,
            settlement.mint_url.clone(),
        );
        let (first_chunk, first_expected, maybe_store) =
            outgoing.take_first_chunk(hash, quote_id)?;
        if let Some(transfer) = maybe_store {
            self.outgoing_transfers
                .lock()
                .await
                .insert(transfer_key, transfer);
        }
        Some((first_chunk, first_expected))
    }

    pub async fn next_outgoing_chunk(
        &self,
        from_peer: &str,
        hash: &[u8],
        quote_id: u64,
    ) -> Option<(DataChunk, ExpectedSettlement)> {
        let hash_hex = hex::encode(hash);
        let key = (from_peer.to_string(), hash_hex, quote_id);
        let mut transfers = self.outgoing_transfers.lock().await;
        let transfer = transfers.get_mut(&key)?;
        let next = transfer.next_chunk(hash, quote_id)?;
        if transfer.is_complete() {
            let _ = transfers.remove(&key);
        }
        Some(next)
    }

    pub async fn create_payment_token(
        &self,
        mint_url: &str,
        amount_sat: u64,
    ) -> Result<CashuSentPayment> {
        let client = self
            .payment_client
            .as_ref()
            .ok_or_else(|| anyhow!("Cashu settlement helper unavailable"))?;
        client.send_payment(mint_url, amount_sat).await
    }

    pub async fn receive_payment_token(&self, encoded_token: &str) -> Result<CashuReceivedPayment> {
        let client = self
            .payment_client
            .as_ref()
            .ok_or_else(|| anyhow!("Cashu settlement helper unavailable"))?;
        client.receive_payment(encoded_token).await
    }

    pub async fn revoke_payment_token(&self, mint_url: &str, operation_id: &str) -> Result<()> {
        let client = self
            .payment_client
            .as_ref()
            .ok_or_else(|| anyhow!("Cashu settlement helper unavailable"))?;
        client.revoke_payment(mint_url, operation_id).await
    }

    pub async fn record_paid_peer(&self, peer_id: &str, amount_sat: u64) {
        self.peer_selector
            .write()
            .await
            .record_cashu_payment(peer_id, amount_sat);
    }

    pub async fn record_receipt_from_peer(
        &self,
        peer_id: &str,
        mint_url: &str,
        amount_sat: u64,
    ) -> Result<()> {
        self.peer_selector
            .write()
            .await
            .record_cashu_receipt(peer_id, amount_sat);
        self.mint_metadata.record_receipt_success(mint_url).await
    }

    pub async fn record_payment_default_from_peer(&self, peer_id: &str) {
        self.peer_selector
            .write()
            .await
            .record_cashu_payment_default(peer_id);
    }

    pub async fn record_mint_receive_failure(&self, mint_url: &str) -> Result<()> {
        self.mint_metadata.record_receipt_failure(mint_url).await
    }

    pub async fn should_refuse_requests_from_peer(&self, peer_id: &str) -> bool {
        let threshold = self.routing.payment_default_block_threshold;
        if threshold == 0 {
            return false;
        }
        self.peer_selector
            .read()
            .await
            .is_peer_blocked_for_payment_defaults(peer_id, threshold)
    }

    fn accepts_quote_mint(&self, mint_url: Option<&str>) -> bool {
        if self.routing.accepted_mints.is_empty() {
            return true;
        }

        let Some(mint_url) = mint_url else {
            return false;
        };
        self.routing
            .accepted_mints
            .iter()
            .any(|mint| mint == mint_url)
    }

    fn trusts_quote_mint(&self, mint_url: Option<&str>) -> bool {
        let Some(mint_url) = mint_url else {
            return self.routing.default_mint.is_none() && self.routing.accepted_mints.is_empty();
        };
        self.routing.default_mint.as_deref() == Some(mint_url)
            || self
                .routing
                .accepted_mints
                .iter()
                .any(|mint| mint == mint_url)
    }

    async fn peer_suggested_mint_cap_sat(&self, peer_id: &str) -> u64 {
        let base = self.routing.peer_suggested_mint_base_cap_sat;
        if base == 0 {
            return 0;
        }

        let selector = self.peer_selector.read().await;
        let Some(stats) = selector.get_stats(peer_id) else {
            let max_cap = self.routing.peer_suggested_mint_max_cap_sat;
            return if max_cap > 0 { base.min(max_cap) } else { base };
        };

        if stats.cashu_payment_defaults > 0
            && stats.cashu_payment_defaults >= stats.cashu_payment_receipts
        {
            return 0;
        }

        let success_bonus = stats
            .successes
            .saturating_mul(self.routing.peer_suggested_mint_success_step_sat);
        let receipt_bonus = stats
            .cashu_payment_receipts
            .saturating_mul(self.routing.peer_suggested_mint_receipt_step_sat);
        let mut cap = base
            .saturating_add(success_bonus)
            .saturating_add(receipt_bonus);
        let max_cap = self.routing.peer_suggested_mint_max_cap_sat;
        if max_cap > 0 {
            cap = cap.min(max_cap);
        }
        cap
    }

    async fn is_mint_allowed_for_requester(&self, mint_url: &str, amount_sat: u64) -> bool {
        !self.is_mint_blocked(mint_url).await
            && self.has_sufficient_balance(mint_url, amount_sat).await
    }

    async fn has_sufficient_balance(&self, mint_url: &str, amount_sat: u64) -> bool {
        if amount_sat == 0 {
            return true;
        }
        self.mint_balance_sat(mint_url).await >= amount_sat
    }

    async fn mint_balance_sat(&self, mint_url: &str) -> u64 {
        let Some(client) = self.payment_client.as_ref() else {
            return 0;
        };
        match client.mint_balance(mint_url).await {
            Ok(CashuMintBalance {
                unit, balance_sat, ..
            }) if unit == "sat" => balance_sat,
            _ => 0,
        }
    }

    async fn is_mint_blocked(&self, mint_url: &str) -> bool {
        self.mint_metadata
            .is_blocked(mint_url, self.routing.mint_failure_block_threshold)
            .await
    }

    fn trusted_mint_candidates(&self) -> Vec<String> {
        let mut candidates = Vec::new();
        if let Some(default_mint) = self.routing.default_mint.as_ref() {
            candidates.push(default_mint.clone());
        }
        for mint_url in &self.routing.accepted_mints {
            if !candidates.iter().any(|existing| existing == mint_url) {
                candidates.push(mint_url.clone());
            }
        }
        candidates
    }
}

impl OutgoingTransfer {
    fn new(
        data: Vec<u8>,
        chunk_target_bytes: usize,
        total_payment_sat: u64,
        mint_url: Option<String>,
    ) -> Self {
        let (chunks, chunk_payments) =
            build_chunk_plan(data, chunk_target_bytes, total_payment_sat);
        Self {
            chunks,
            chunk_payments,
            mint_url,
            next_chunk_index: 0,
        }
    }

    fn take_first_chunk(
        mut self,
        hash: &[u8],
        quote_id: u64,
    ) -> Option<(DataChunk, ExpectedSettlement, Option<Self>)> {
        let next = self.next_chunk(hash, quote_id)?;
        let store = (!self.is_complete()).then_some(self);
        Some((next.0, next.1, store))
    }

    fn next_chunk(
        &mut self,
        hash: &[u8],
        quote_id: u64,
    ) -> Option<(DataChunk, ExpectedSettlement)> {
        let chunk_index = self.next_chunk_index;
        let data = self.chunks.get(chunk_index)?.clone();
        let payment_sat = *self.chunk_payments.get(chunk_index)?;
        let total_chunks = self.chunks.len() as u32;
        self.next_chunk_index += 1;
        Some((
            DataChunk {
                h: hash.to_vec(),
                q: quote_id,
                c: chunk_index as u32,
                n: total_chunks,
                p: payment_sat,
                d: data,
            },
            ExpectedSettlement {
                chunk_index: chunk_index as u32,
                payment_sat,
                mint_url: self.mint_url.clone(),
                final_chunk: chunk_index + 1 == self.chunks.len(),
            },
        ))
    }

    fn is_complete(&self) -> bool {
        self.next_chunk_index >= self.chunks.len()
    }
}

fn build_chunk_plan(
    data: Vec<u8>,
    chunk_target_bytes: usize,
    total_payment_sat: u64,
) -> (Vec<Vec<u8>>, Vec<u64>) {
    let total_len = data.len();
    let max_chunks_by_size = if total_len == 0 {
        1
    } else {
        total_len.div_ceil(chunk_target_bytes.max(1))
    };
    let chunk_count = if total_payment_sat == 0 {
        1
    } else {
        max_chunks_by_size
            .min(total_payment_sat.min(usize::MAX as u64) as usize)
            .max(1)
    };

    let mut chunks = Vec::with_capacity(chunk_count);
    if total_len == 0 {
        chunks.push(Vec::new());
    } else {
        let base = total_len / chunk_count;
        let extra = total_len % chunk_count;
        let mut offset = 0usize;
        for chunk_idx in 0..chunk_count {
            let size = base + usize::from(chunk_idx < extra);
            let end = offset + size;
            chunks.push(data[offset..end].to_vec());
            offset = end;
        }
    }

    let mut chunk_payments = Vec::with_capacity(chunk_count);
    let base_payment = total_payment_sat / chunk_count as u64;
    let extra_payment = total_payment_sat % chunk_count as u64;
    for chunk_idx in 0..chunk_count {
        chunk_payments.push(base_payment + u64::from(chunk_idx < extra_payment as usize));
    }

    (chunks, chunk_payments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SelectionStrategy;
    use async_trait::async_trait;
    use cashu_service::{
        CashuMintBalance, CashuPaymentClient, CashuReceivedPayment, CashuSentPayment,
    };
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    #[derive(Debug)]
    struct NoopPaymentClient {
        balances: StdMutex<HashMap<String, u64>>,
    }

    impl Default for NoopPaymentClient {
        fn default() -> Self {
            let mut balances = HashMap::new();
            balances.insert("https://mint.example".to_string(), 21);
            balances.insert("https://mint-a.example".to_string(), 21);
            balances.insert("https://mint-b.example".to_string(), 21);
            Self {
                balances: StdMutex::new(balances),
            }
        }
    }

    #[async_trait]
    impl CashuPaymentClient for NoopPaymentClient {
        async fn send_payment(&self, mint_url: &str, amount_sat: u64) -> Result<CashuSentPayment> {
            Ok(CashuSentPayment {
                mint_url: mint_url.to_string(),
                unit: "sat".to_string(),
                amount_sat,
                send_fee_sat: 0,
                operation_id: "op-1".to_string(),
                token: "cashuBtoken".to_string(),
            })
        }

        async fn receive_payment(&self, _encoded_token: &str) -> Result<CashuReceivedPayment> {
            Ok(CashuReceivedPayment {
                mint_url: "https://mint.example".to_string(),
                unit: "sat".to_string(),
                amount_sat: 3,
            })
        }

        async fn revoke_payment(&self, _mint_url: &str, _operation_id: &str) -> Result<()> {
            Ok(())
        }

        async fn mint_balance(&self, mint_url: &str) -> Result<CashuMintBalance> {
            let balance_sat = self
                .balances
                .lock()
                .unwrap()
                .get(mint_url)
                .copied()
                .unwrap_or_default();
            Ok(CashuMintBalance {
                mint_url: mint_url.to_string(),
                unit: "sat".to_string(),
                balance_sat,
            })
        }
    }

    fn make_state(routing: CashuRoutingConfig, with_client: bool) -> Arc<CashuQuoteState> {
        Arc::new(CashuQuoteState::new(
            routing,
            Arc::new(RwLock::new(PeerSelector::with_strategy(
                SelectionStrategy::Weighted,
            ))),
            with_client
                .then_some(Arc::new(NoopPaymentClient::default()) as Arc<dyn CashuPaymentClient>),
        ))
    }

    fn quote_response(mint_url: Option<&str>, payment_sat: u64) -> DataQuoteResponse {
        DataQuoteResponse {
            h: vec![0x11; 32],
            a: true,
            q: Some(7),
            p: Some(payment_sat),
            t: Some(500),
            m: mint_url.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn test_requester_quote_terms_require_payment_client_and_mint_policy() {
        let disabled = make_state(CashuRoutingConfig::default(), false);
        assert_eq!(disabled.requester_quote_terms().await, None);

        let no_client = make_state(
            CashuRoutingConfig {
                default_mint: Some("https://mint-a.example".to_string()),
                ..Default::default()
            },
            false,
        );
        assert_eq!(no_client.requester_quote_terms().await, None);

        let enabled = make_state(
            CashuRoutingConfig {
                default_mint: Some("https://mint-a.example".to_string()),
                ..Default::default()
            },
            true,
        );
        assert_eq!(
            enabled.requester_quote_terms().await,
            Some(("https://mint-a.example".to_string(), 3, 1_500))
        );
    }

    #[tokio::test]
    async fn test_requester_quote_terms_require_funded_trusted_mint() {
        let state = make_state(
            CashuRoutingConfig {
                default_mint: Some("https://mint-empty.example".to_string()),
                quote_payment_offer_sat: 3,
                ..Default::default()
            },
            true,
        );
        assert_eq!(
            state.requester_quote_terms().await,
            None,
            "unfunded trusted mint should disable paid quotes"
        );
    }

    #[tokio::test]
    async fn test_should_accept_quote_response_allows_bounded_peer_suggested_mint() {
        let state = make_state(
            CashuRoutingConfig {
                accepted_mints: vec!["https://mint-a.example".to_string()],
                default_mint: Some("https://mint-a.example".to_string()),
                peer_suggested_mint_base_cap_sat: 3,
                peer_suggested_mint_max_cap_sat: 3,
                ..Default::default()
            },
            true,
        );

        let accepted = state
            .should_accept_quote_response(
                "peer-a",
                Some("https://mint-a.example"),
                3,
                &quote_response(Some("https://mint-b.example"), 3),
            )
            .await;
        assert!(accepted);
    }

    #[tokio::test]
    async fn test_should_accept_quote_response_rejects_peer_suggested_mint_after_defaults() {
        let state = make_state(
            CashuRoutingConfig {
                accepted_mints: vec!["https://mint-a.example".to_string()],
                default_mint: Some("https://mint-a.example".to_string()),
                peer_suggested_mint_base_cap_sat: 3,
                peer_suggested_mint_max_cap_sat: 3,
                ..Default::default()
            },
            true,
        );
        state
            .peer_selector
            .write()
            .await
            .record_cashu_payment_default("peer-a");

        let accepted = state
            .should_accept_quote_response(
                "peer-a",
                Some("https://mint-a.example"),
                3,
                &quote_response(Some("https://mint-b.example"), 3),
            )
            .await;
        assert!(!accepted);
    }

    #[tokio::test]
    async fn test_should_accept_quote_response_rejects_when_mint_balance_is_insufficient() {
        let state = make_state(
            CashuRoutingConfig {
                accepted_mints: vec!["https://mint-a.example".to_string()],
                default_mint: Some("https://mint-a.example".to_string()),
                ..Default::default()
            },
            true,
        );

        let accepted = state
            .should_accept_quote_response(
                "peer-a",
                Some("https://mint-empty.example"),
                5,
                &quote_response(Some("https://mint-empty.example"), 5),
            )
            .await;
        assert!(!accepted);
    }

    #[tokio::test]
    async fn test_handle_quote_response_resolves_pending_quote() {
        let state = make_state(
            CashuRoutingConfig {
                accepted_mints: vec!["https://mint-a.example".to_string()],
                default_mint: Some("https://mint-a.example".to_string()),
                peer_suggested_mint_base_cap_sat: 3,
                peer_suggested_mint_max_cap_sat: 3,
                ..Default::default()
            },
            true,
        );

        let hash_hex = hex::encode([0x11; 32]);
        let mut rx = state
            .register_pending_quote(hash_hex, Some("https://mint-a.example".to_string()), 3)
            .await;

        let handled = state
            .handle_quote_response("peer-a", quote_response(Some("https://mint-b.example"), 3))
            .await;
        assert!(handled);

        let quote = rx
            .try_recv()
            .expect("expected negotiated quote")
            .expect("expected quote payload");
        assert_eq!(quote.peer_id, "peer-a");
        assert_eq!(quote.quote_id, 7);
        assert_eq!(quote.payment_sat, 3);
        assert_eq!(quote.mint_url.as_deref(), Some("https://mint-b.example"));
    }

    #[tokio::test]
    async fn test_build_quote_response_registers_quote_for_validation() {
        let state = make_state(
            CashuRoutingConfig {
                accepted_mints: vec!["https://mint-a.example".to_string()],
                default_mint: Some("https://mint-a.example".to_string()),
                ..Default::default()
            },
            true,
        );

        let res = state
            .build_quote_response(
                "peer-a",
                &DataQuoteRequest {
                    h: vec![0x22; 32],
                    p: 3,
                    t: 500,
                    m: Some("https://mint-a.example".to_string()),
                },
                true,
            )
            .await;
        assert!(res.a);

        let expected = state
            .take_valid_quote("peer-a", &[0x22; 32], res.q.unwrap())
            .await
            .expect("quote should validate");
        assert_eq!(expected.payment_sat, 3);
        assert_eq!(expected.mint_url.as_deref(), Some("https://mint-a.example"));
    }

    #[tokio::test]
    async fn test_payment_timeout_records_default() {
        let state = make_state(
            CashuRoutingConfig {
                default_mint: Some("https://mint-a.example".to_string()),
                settlement_timeout_ms: 10,
                ..Default::default()
            },
            true,
        );
        state
            .register_expected_payment(
                "peer-a".to_string(),
                hex::encode([0x33; 32]),
                7,
                ExpectedSettlement {
                    chunk_index: 0,
                    payment_sat: 3,
                    mint_url: Some("https://mint-a.example".to_string()),
                    final_chunk: true,
                },
            )
            .await;

        tokio::time::sleep(Duration::from_millis(25)).await;
        let selector = state.peer_selector.read().await;
        let stats = selector.get_stats("peer-a").expect("peer stats");
        assert_eq!(stats.cashu_payment_defaults, 1);
    }

    #[tokio::test]
    async fn test_mint_metadata_store_persists_and_blocks_failed_mint() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = cashu_mint_metadata_path(temp_dir.path());
        let store = CashuMintMetadataStore::load(&path).unwrap();
        store
            .record_receipt_failure("https://mint-bad.example")
            .await
            .unwrap();
        store
            .record_receipt_failure("https://mint-bad.example")
            .await
            .unwrap();

        let restored = CashuMintMetadataStore::load(&path).unwrap();
        let record = restored.get("https://mint-bad.example").await;
        assert_eq!(record.failed_receipts, 2);
        assert!(restored.is_blocked("https://mint-bad.example", 2).await);
    }

    #[test]
    fn test_build_chunk_plan_splits_payment_into_small_chunks() {
        let (chunks, payments) = build_chunk_plan(vec![1_u8; 10], 4, 3);
        assert_eq!(chunks.len(), 3);
        assert_eq!(payments, vec![1, 1, 1]);
        assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), 10);
    }
}
