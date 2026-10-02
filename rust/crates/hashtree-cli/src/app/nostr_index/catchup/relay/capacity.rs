use super::*;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Key {
    relay: String,
    kinds: Vec<u16>,
    since: u64,
    until: u64,
    limit: usize,
}

impl RelaySource {
    pub(super) async fn capacity(&mut self, relay: &str, query: &CatchupQuery) -> Result<usize> {
        let key = Key {
            relay: relay.to_owned(),
            kinds: query.kinds.clone(),
            since: query.since,
            until: query.until,
            limit: query.limit,
        };
        if let Some(capacity) = self.capacities.get(&key) {
            return Ok(*capacity);
        }
        // One author-free query for this relay/filter window, using exactly the
        // same signature, filter, EOSE and resource validation as author pages.
        // Under a uniform relay cap, this observed size is a lower bound on its
        // capacity; it is not evidence of global history coverage. Keep only
        // the cardinality: these unrelated events never enter the index.
        let capacity = self.query_page(relay, query, false).await?.len();
        self.capacities.insert(key, capacity);
        Ok(capacity)
    }
}
