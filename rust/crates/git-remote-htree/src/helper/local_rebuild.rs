//! Explicit, conservative recovery of a repository whose inherited packs are unavailable.

use super::{push::collect_complete_hashes, RemoteHelper};
use crate::git::object::ObjectId;
use crate::runtime::block_on_result;
use anyhow::{bail, Context, Result};
use hashtree_core::{Cid, HashTree, HashTreeConfig};
use std::collections::{BTreeMap, HashMap, HashSet};

impl RemoteHelper {
    pub(super) fn local_rebuild_requested() -> bool {
        std::env::var("HTREE_GIT_REBUILD_FROM_LOCAL").as_deref() == Ok("1")
    }

    pub(super) fn validate_local_rebuild_mode(&self) -> Result<()> {
        if self.config.blossom.force_upload {
            bail!("local rebuild cannot be combined with force_upload");
        }
        if self.push_specs.len() != 1
            || self.push_specs[0].force
            || self.push_specs[0].src.is_empty()
            || !self.push_specs[0].dst.starts_with("refs/")
        {
            bail!("local rebuild requires one non-deletion, non-force push");
        }
        Ok(())
    }

    pub(super) fn require_local_rebuild_refs(refs: &HashMap<String, String>) -> Result<()> {
        if !refs.keys().any(|name| name.starts_with("refs/"))
            || refs.get("HEAD").is_none_or(String::is_empty)
        {
            bail!("local rebuild requires readable existing refs and HEAD");
        }
        Ok(())
    }

    pub(super) fn build_local_rebuild(&self) -> Result<Cid> {
        let refs = self.storage.list_refs()?;
        Self::require_local_rebuild_refs(&refs)?;
        let tips: Vec<_> = refs
            .values()
            .filter(|value| !value.starts_with("ref: "))
            .map(String::as_str)
            .collect();
        Self::verify_git_object_closure(&tips)?;
        let tips: Vec<_> = tips.into_iter().map(str::to_string).collect();
        let objects = self.list_objects_for_shas(&tips, &[])?;
        eprintln!(
            "  Rebuilding {} object(s) across {} refs",
            objects.len(),
            refs.len()
        );
        let contents = self.read_git_objects_batch(&objects)?;
        // Prove exact contents before replacing any cached import state. Batch
        // reads keep this practical for repositories with thousands of objects.
        for (oid, (kind, content)) in objects.iter().zip(&contents) {
            if ObjectId::hash_object(*kind, content).to_hex() != *oid {
                bail!("local rebuild Git object id mismatch for {oid}");
            }
        }
        self.storage.clear()?;
        self.storage
            .set_pack_checkpoint_files(BTreeMap::new(), HashSet::new())?;
        for (kind, content) in contents {
            self.storage.write_raw_object(kind, &content)?;
        }
        for (name, value) in &refs {
            self.storage.import_ref(name, value)?;
        }
        let root =
            self.build_tree_with_progress("Rebuilding complete repository from local Git")?;
        let tree = HashTree::new(HashTreeConfig::new(self.storage.store().clone()));
        block_on_result(async {
            for (name, value) in &refs {
                let cid = tree
                    .resolve_path(&root, &format!(".git/{name}"))
                    .await?
                    .with_context(|| format!("local rebuild omitted ref {name}"))?;
                let bytes = tree
                    .get(&cid, None)
                    .await?
                    .context("local rebuild ref is unavailable")?;
                if String::from_utf8_lossy(&bytes).trim() != value {
                    bail!("local rebuild changed ref {name}");
                }
            }
            for oid in &objects {
                let path = format!(".git/objects/{}/{}", &oid[..2], &oid[2..]);
                tree.resolve_path(&root, &path)
                    .await?
                    .with_context(|| format!("local rebuild omitted Git object {oid}"))?;
            }
            collect_complete_hashes(&tree, &root, 4).await?;
            Ok(())
        })?;
        Ok(root)
    }
}
