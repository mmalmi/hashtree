//! Bounded recovery when a delta merge needs objects absent from the import cache.

use super::RemoteHelper;
use crate::git::object::ObjectId;
use anyhow::{bail, Context, Result};

impl RemoteHelper {
    pub(super) fn hydrate_cached_root_repair(&self, sha: &str) -> Result<()> {
        // A tag push may leave HEAD on a different branch. Repair every retained
        // ref, not just the pushed revision, without changing refs or pack state.
        let refs = self.storage.list_refs()?;
        let mut tips: Vec<_> = refs
            .values()
            .filter(|value| !value.starts_with("ref: "))
            .map(String::as_str)
            .chain(std::iter::once(sha))
            .collect();
        tips.sort_unstable();
        tips.dedup();
        Self::verify_git_object_closure(&tips).context(
            "cached-root repair needs complete, valid local history for every retained ref",
        )?;
        let output = Self::strict_git_command()
            .args([
                "rev-list",
                "--objects",
                "--no-object-names",
                "--missing=error",
            ])
            .args(&tips)
            .output()
            .context("enumerate cached-root repair objects")?;
        if !output.status.success() {
            bail!(
                "cannot enumerate cached-root repair objects: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let mut objects: Vec<_> = String::from_utf8(output.stdout)?
            .lines()
            .map(str::to_string)
            .collect();
        objects.sort_unstable();
        objects.dedup();
        let missing = self.storage.missing_imported_objects(&objects)?;
        eprintln!(
            "  Hydrating {} local Git object(s) for cached-root repair",
            missing.len()
        );
        // Bound each uncompressed read. The import cache retains compressed
        // objects; never accumulate the entire history's contents in a Vec.
        for batch in missing.chunks(100) {
            let contents = self.read_git_objects_batch(batch)?;
            for (oid, (kind, content)) in batch.iter().zip(&contents) {
                if ObjectId::hash_object(*kind, content).to_hex() != *oid {
                    bail!("cached-root repair Git object id mismatch for {oid}");
                }
            }
            for (kind, content) in contents {
                self.storage.write_raw_object(kind, &content)?;
            }
        }
        Ok(())
    }
}
