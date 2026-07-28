use super::move_catalog::{move_cleanup_state_key, move_state_key};
use super::temperature::AccessRecord;
use super::{
    decode_manifest, encode_manifest, map_heed, member_hash_key, unix_timestamp_now,
    LocationRecord, PoolManifest, PoolMemberId, PoolStore, MANIFEST_KEY,
};
use hashtree_core::store::StoreError;
use hashtree_core::types::Hash;

impl PoolStore {
    pub(super) fn read_manifest(&self) -> Result<PoolManifest, StoreError> {
        Ok(self.read_manifest_with_identity()?.0)
    }

    pub(super) fn read_manifest_with_identity(
        &self,
    ) -> Result<(PoolManifest, hashtree_core::types::Hash), StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let bytes = self
            .manifest_db
            .get(&rtxn, MANIFEST_KEY)
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool manifest is missing".into()))?;
        Ok((decode_manifest(bytes)?, hashtree_core::sha256(bytes)))
    }

    pub(super) fn manifest_from_txn(
        &self,
        txn: &heed::RoTxn<'_>,
    ) -> Result<PoolManifest, StoreError> {
        let bytes = self
            .manifest_db
            .get(txn, MANIFEST_KEY)
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool manifest is missing".into()))?;
        decode_manifest(bytes)
    }

    pub(super) fn put_manifest_txn(
        &self,
        txn: &mut heed::RwTxn<'_>,
        manifest: &PoolManifest,
    ) -> Result<(), StoreError> {
        let bytes = encode_manifest(manifest)?;
        self.manifest_db
            .put(txn, MANIFEST_KEY, bytes.as_slice())
            .map_err(map_heed)
    }

    pub(super) fn read_location(&self, hash: &Hash) -> Result<Option<LocationRecord>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        self.locations
            .get(&rtxn, hash)
            .map_err(map_heed)?
            .map(LocationRecord::decode)
            .transpose()
    }

    pub(super) fn reserve_if_absent(
        &self,
        hash: Hash,
        pending: LocationRecord,
    ) -> Result<LocationRecord, StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        if let Some(existing) = self.locations.get(&wtxn, &hash).map_err(map_heed)? {
            return LocationRecord::decode(existing);
        }
        self.set_location_txn(&mut wtxn, hash, Some(pending))?;
        wtxn.commit().map_err(map_heed)?;
        Ok(pending)
    }

    pub(super) fn finalize_pending(
        &self,
        hash: Hash,
        pending: LocationRecord,
    ) -> Result<(), StoreError> {
        if let LocationRecord::Pending { member, size } = pending {
            let mut wtxn = self.env.write_txn().map_err(map_heed)?;
            let current = self
                .locations
                .get(&wtxn, &hash)
                .map_err(map_heed)?
                .map(LocationRecord::decode)
                .transpose()?;
            if current == Some(pending) {
                self.set_location_txn(
                    &mut wtxn,
                    hash,
                    Some(LocationRecord::Stored { member, size }),
                )?;
            }
            wtxn.commit().map_err(map_heed)?;
        }
        Ok(())
    }

    pub(super) fn set_location_txn(
        &self,
        txn: &mut heed::RwTxn<'_>,
        hash: Hash,
        location: Option<LocationRecord>,
    ) -> Result<(), StoreError> {
        let previous = self
            .locations
            .get(txn, &hash)
            .map_err(map_heed)?
            .map(LocationRecord::decode)
            .transpose()?;
        let had_previous = previous.is_some();
        if let Some(previous) = previous {
            let (members, len) = previous.members();
            for member in members.into_iter().take(len) {
                self.by_member
                    .delete(txn, &member_hash_key(member, hash))
                    .map_err(map_heed)?;
            }
        }
        if previous != location {
            if let Some(LocationRecord::Moving { source, .. }) = self
                .temperature_state
                .get(txn, &move_cleanup_state_key(hash))
                .map_err(map_heed)?
                .map(LocationRecord::decode)
                .transpose()?
            {
                self.by_member
                    .delete(txn, &member_hash_key(source, hash))
                    .map_err(map_heed)?;
            }
            self.temperature_state
                .delete(txn, &move_state_key(hash))
                .map_err(map_heed)?;
            self.temperature_state
                .delete(txn, &move_cleanup_state_key(hash))
                .map_err(map_heed)?;
        }
        match location {
            Some(location) => {
                let encoded = location.encode();
                self.locations.put(txn, &hash, &encoded).map_err(map_heed)?;
                let (members, len) = location.members();
                for member in members.into_iter().take(len) {
                    self.by_member
                        .put(txn, &member_hash_key(member, hash), &())
                        .map_err(map_heed)?;
                }
                if !had_previous {
                    let access = AccessRecord::new(unix_timestamp_now()).encode();
                    self.last_accessed
                        .put(txn, &hash, &access)
                        .map_err(map_heed)?;
                }
            }
            None => {
                self.locations.delete(txn, &hash).map_err(map_heed)?;
                self.last_accessed.delete(txn, &hash).map_err(map_heed)?;
            }
        }
        Ok(())
    }

    pub(super) fn count_member_locations(&self, id: PoolMemberId) -> Result<u64, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        self.count_member_locations_txn(&rtxn, id)
    }

    /// Check one exact member prefix without enumerating its location index.
    ///
    /// Callers that only need an emptiness proof (not a count) use this while
    /// holding the catalog write transaction, so a concurrent process cannot
    /// add a location between the proof and a manifest update.
    pub(super) fn member_has_locations_txn(
        &self,
        txn: &heed::RoTxn<'_>,
        id: PoolMemberId,
    ) -> Result<bool, StoreError> {
        let mut entries = self
            .by_member
            .prefix_iter(txn, id.as_bytes())
            .map_err(map_heed)?;
        let Some(entry) = entries.next() else {
            return Ok(false);
        };
        let (key, _) = entry.map_err(map_heed)?;
        if key.len() != 48 || !key.starts_with(id.as_bytes()) {
            return Err(StoreError::Other("invalid pool member index key".into()));
        }
        Ok(true)
    }

    pub(super) fn count_member_locations_txn(
        &self,
        txn: &heed::RoTxn<'_>,
        id: PoolMemberId,
    ) -> Result<u64, StoreError> {
        let mut count = 0u64;
        for item in self
            .by_member
            .prefix_iter(txn, id.as_bytes())
            .map_err(map_heed)?
        {
            item.map_err(map_heed)?;
            count = count.saturating_add(1);
        }
        Ok(count)
    }

    pub(super) fn member_hashes(
        &self,
        id: PoolMemberId,
        limit: usize,
    ) -> Result<Vec<Hash>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let mut hashes = Vec::with_capacity(limit);
        for item in self
            .by_member
            .prefix_iter(&rtxn, id.as_bytes())
            .map_err(map_heed)?
        {
            let (key, _) = item.map_err(map_heed)?;
            if key.len() != 48 {
                return Err(StoreError::Other("invalid pool member index key".into()));
            }
            let hash: Hash = key[16..]
                .try_into()
                .map_err(|_| StoreError::Other("invalid pool member hash key".into()))?;
            hashes.push(hash);
            if hashes.len() >= limit {
                break;
            }
        }
        Ok(hashes)
    }
}
