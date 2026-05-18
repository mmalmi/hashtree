use super::progress::emit_upload_progress;
use super::storage_support::{build_repo_viewer_url, get_hashtree_data_dir, queue_hash_if_new};
use super::{upload_progress, AncestorCheck, PushSpec, RemoteHelper, SERVER_COVERAGE_SAMPLE_SIZE};
use crate::git::refs::Ref;
use crate::nostr_client::{BlossomResult, PullRequestStateFilter};
use crate::runtime::new_multi_thread_runtime;
use anyhow::{bail, Context, Result};
use hashtree_core::{HashTree, Store};
use std::collections::HashSet;
use std::io::Write;
use std::process::Command;
use tracing::{debug, info, warn};

fn effective_upload_concurrency(server_count: usize, configured: usize) -> usize {
    let configured = configured.max(1);
    if server_count == 0 {
        1
    } else {
        configured
    }
}

pub(super) fn queue_links_for_diff_upload(
    queue: &mut Vec<([u8; 32], Option<[u8; 32]>)>,
    queued: &mut HashSet<[u8; 32]>,
    links: &[hashtree_core::Link],
    old_hashes: &HashSet<[u8; 32]>,
    prune_known_subtrees: bool,
    discovered_total: &std::sync::atomic::AtomicUsize,
) {
    use std::sync::atomic::Ordering;

    for link in links {
        if prune_known_subtrees && old_hashes.contains(&link.hash) {
            continue;
        }
        if queue_hash_if_new(queue, queued, link.hash, link.key) {
            discovered_total.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl RemoteHelper {
    fn upload_concurrency(&self, server_count: usize) -> usize {
        effective_upload_concurrency(server_count, self.config.blossom.upload_concurrency)
    }

    fn build_tree_with_progress(&self, label: &str) -> Result<hashtree_core::Cid> {
        if self.is_slow() {
            eprint!("  {}...", label);
            let _ = std::io::stderr().flush();
        }
        let result = self.storage.build_tree();
        if self.is_slow() {
            match &result {
                Ok(_) => eprintln!(" done"),
                Err(err) => eprintln!(" failed ({})", err),
            }
        }
        Ok(result?)
    }

    fn build_tree_with_base_progress<S: Store>(
        &self,
        label: &str,
        base_tree: Option<&HashTree<S>>,
        base_root: Option<&hashtree_core::Cid>,
        delta_base: Option<&str>,
    ) -> Result<hashtree_core::Cid> {
        if self.is_slow() {
            eprint!("  {}...", label);
            let _ = std::io::stderr().flush();
        }
        let result = self
            .storage
            .build_tree_with_base_objects(base_tree, base_root, delta_base);
        if self.is_slow() {
            match &result {
                Ok(_) => eprintln!(" done"),
                Err(err) => eprintln!(" failed ({})", err),
            }
        }
        Ok(result?)
    }

    /// Queue a push operation
    pub(super) fn queue_push(&mut self, arg: &str) -> Result<()> {
        // Format: [+]<src>:<dst>
        let force = arg.starts_with('+');
        let arg = if force { &arg[1..] } else { arg };

        let parts: Vec<&str> = arg.splitn(2, ':').collect();
        if parts.len() != 2 {
            bail!("Invalid push spec: {}", arg);
        }

        self.push_specs.push(PushSpec {
            src: parts[0].to_string(),
            dst: parts[1].to_string(),
            force,
        });
        Ok(())
    }

    /// Execute queued push operations
    pub(super) fn execute_push(&mut self) -> Result<Option<Vec<String>>> {
        self.start_op(); // Start timing for conditional verbose logging
        debug!(refs_count = self.push_specs.len(), "execute_push called");
        info!("Pushing {} refs", self.push_specs.len());

        // First, load existing refs and objects from remote to preserve other branches
        let has_force_push = self.push_specs.iter().any(|s| s.force);
        debug!(
            force = has_force_push,
            "About to call load_existing_remote_state"
        );

        if let Err(e) = self.load_existing_remote_state() {
            let err_str = e.to_string();
            let is_access_error = err_str.contains("link-visible")
                || err_str.contains("private")
                || err_str.contains("secret key");
            let is_likely_new_repo = err_str.contains("No root hash")
                || err_str.contains("not found")
                || err_str.contains("timeout");

            if is_access_error {
                debug!("Cannot access existing repo (visibility change): {}", e);
            } else if has_force_push {
                eprintln!("  Warning: Could not load existing remote state: {}", e);
                eprintln!("  Proceeding with force push (may overwrite other branches)");
            } else if is_likely_new_repo {
                debug!("Error loading remote state (likely new repo): {}", e);
                info!(
                    "Could not load existing remote state: {} (likely new repo)",
                    e
                );
            } else {
                eprintln!("  Warning: Could not load existing remote state: {}", e);
                eprintln!("  Other branches may be lost. Use 'git push --force' to override.");
                eprintln!("  Or check your network connection and try again.");
            }
        }

        let mut results = Vec::new();
        let mut pushed_refs: Vec<(String, String)> = Vec::new();
        let specs: Vec<_> = std::mem::take(&mut self.push_specs);

        for spec in specs {
            debug!(
                "Pushing {} -> {} (force={})",
                spec.src, spec.dst, spec.force
            );

            let sha = if spec.src.is_empty() {
                String::new()
            } else {
                self.resolve_ref(&spec.src)?
            };

            if sha.is_empty() {
                match self.storage.delete_ref(&spec.dst) {
                    Ok(_) => {
                        self.nostr.delete_ref(&self.repo_name, &spec.dst)?;
                        results.push(format!("ok {}", spec.dst));
                    }
                    Err(e) => results.push(format!("error {} {}", spec.dst, e)),
                }
            } else {
                if !spec.force {
                    if let Some(remote_sha) = self.remote_refs.get(&spec.dst) {
                        match self.check_ancestor(remote_sha, &sha) {
                            AncestorCheck::Ancestor => {}
                            AncestorCheck::NotAncestor => {
                                results.push(format!(
                                    "error {} non-fast-forward (use --force to override)",
                                    spec.dst
                                ));
                                eprintln!(
                                    "  Rejected: {} has commits you don't have. Pull first or use --force.",
                                    spec.dst
                                );
                                eprintln!("  remote: {}", remote_sha);
                                eprintln!("  local : {}", sha);
                                continue;
                            }
                            AncestorCheck::Unknown(reason) => {
                                results.push(format!(
                                    "error {} fast-forward-check-failed (use --force to override)",
                                    spec.dst
                                ));
                                eprintln!("  Rejected: {} fast-forward check failed.", spec.dst);
                                eprintln!("  Could not verify ancestry between:");
                                eprintln!("    remote: {}", remote_sha);
                                eprintln!("    local : {}", sha);
                                eprintln!("  merge-base error: {}", reason);
                                continue;
                            }
                        }
                    }
                }

                let remote_tip = self.remote_refs.get(&spec.dst).cloned();
                match self.push_objects(&sha, &spec.dst, spec.force, remote_tip.as_deref()) {
                    Ok(()) => {
                        results.push(format!("ok {}", spec.dst));
                        pushed_refs.push((spec.dst, sha));
                    }
                    Err(e) => results.push(format!("error {} {}", spec.dst, e)),
                }
            }
        }

        if self.nostr.can_sign() && !pushed_refs.is_empty() {
            self.detect_and_mark_merged_prs(&pushed_refs);
        }

        results.push(String::new());
        Ok(Some(results))
    }

    /// Load existing refs and objects from remote before pushing
    /// This preserves branches that aren't being pushed
    pub(super) fn load_existing_remote_state(&mut self) -> Result<()> {
        let data_dir = get_hashtree_data_dir();
        self.detail(&format!(
            "  Loading existing remote state... (data_dir: {:?})",
            data_dir
        ));

        let (refs, root_hash, _encryption_key) =
            self.nostr.fetch_refs_with_root(&self.repo_name)?;

        if refs.is_empty() {
            self.detail("  No existing refs found (new repository)");
            return Ok(());
        }

        self.detail(&format!("  Found {} existing refs", refs.len()));
        self.remote_refs.clear();
        for (ref_name, ref_value) in &refs {
            if ref_name.starts_with("refs/") && !ref_value.starts_with("ref: ") {
                self.remote_refs.insert(ref_name.clone(), ref_value.clone());
            }
        }

        for (ref_name, ref_value) in &refs {
            let is_being_pushed = self.push_specs.iter().any(|s| s.dst == *ref_name);
            if !is_being_pushed {
                self.storage.import_ref(ref_name, ref_value)?;
                debug!(
                    "Imported existing ref: {} -> {}",
                    ref_name,
                    &ref_value[..12.min(ref_value.len())]
                );
            }
        }

        let preserved_refs: Vec<(String, String)> = refs
            .iter()
            .filter(|(ref_name, ref_value)| {
                ref_name.starts_with("refs/")
                    && !ref_value.starts_with("ref: ")
                    && !self.push_specs.iter().any(|spec| spec.dst == **ref_name)
            })
            .map(|(ref_name, ref_value)| (ref_name.clone(), ref_value.clone()))
            .collect();

        if preserved_refs.is_empty() {
            self.detail("  No untouched direct refs to preserve");
            self.detail("  Remote state loaded");
            return Ok(());
        }

        if self.import_preserved_remote_objects_from_local_git(&preserved_refs)? {
            self.detail("  Reused preserved remote objects from local git");
        } else if let Some(root) = root_hash {
            self.detail(
                "  Falling back to remote object import for preserved refs not available locally",
            );
            let objects = self.fetch_all_git_objects(&root)?;
            self.detail(&format!("  Importing {} existing objects", objects.len()));

            for (oid, content) in objects {
                self.storage.import_compressed_object(&oid, content)?;
            }
        } else {
            bail!("No root hash found for repository - cannot preserve untouched refs");
        }

        self.detail("  Remote state loaded");
        Ok(())
    }
    pub(super) fn import_preserved_remote_objects_from_local_git(
        &self,
        preserved_refs: &[(String, String)],
    ) -> Result<bool> {
        let mut include_shas: Vec<String> =
            preserved_refs.iter().map(|(_, sha)| sha.clone()).collect();
        include_shas.sort();
        include_shas.dedup();

        if include_shas.is_empty() {
            return Ok(true);
        }

        let existing = self.git_batch_check_objects(include_shas.iter().map(|sha| sha.as_str()))?;
        if existing.len() != include_shas.len() {
            let missing: Vec<String> = include_shas
                .iter()
                .filter(|sha| !existing.contains(*sha))
                .cloned()
                .collect();
            self.detail(&format!(
                "  Local git is missing {} preserved remote tip(s): {}",
                missing.len(),
                missing
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            return Ok(false);
        }

        let exclude_shas = self.resolved_push_tip_shas();
        let objects = match self.list_objects_for_shas(&include_shas, &exclude_shas) {
            Ok(objects) => objects,
            Err(err) => {
                self.detail(&format!(
                    "  Could not enumerate preserved remote objects from local git: {}",
                    err
                ));
                return Ok(false);
            }
        };

        self.detail(&format!(
            "  Importing {} preserved object(s) from local git for {} untouched ref(s)",
            objects.len(),
            preserved_refs.len()
        ));

        let objects_with_content = match self.read_git_objects_batch(&objects) {
            Ok(objects_with_content) => objects_with_content,
            Err(err) => {
                self.detail(&format!(
                    "  Could not read preserved remote objects from local git: {}",
                    err
                ));
                return Ok(false);
            }
        };

        for (obj_type, content) in objects_with_content {
            self.storage.write_raw_object(obj_type, &content)?;
        }

        Ok(true)
    }

    pub(super) fn resolved_push_tip_shas(&self) -> Vec<String> {
        let mut shas = Vec::new();
        for spec in &self.push_specs {
            if spec.src.is_empty() {
                continue;
            }
            if let Ok(sha) = self.resolve_ref(&spec.src) {
                shas.push(sha);
            }
        }
        shas.sort();
        shas.dedup();
        shas
    }

    /// Resolve a ref to its sha
    pub(super) fn resolve_ref(&self, refspec: &str) -> Result<String> {
        let output = Command::new("git").args(["rev-parse", refspec]).output()?;

        if !output.status.success() {
            bail!("Failed to resolve ref: {}", refspec);
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Check if ancestor_sha is an ancestor of descendant_sha
    pub(super) fn check_ancestor(&self, ancestor_sha: &str, descendant_sha: &str) -> AncestorCheck {
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", ancestor_sha, descendant_sha])
            .output();

        match output {
            Ok(o) => Self::classify_merge_base_result(o.status.code(), &o.stderr),
            Err(e) => AncestorCheck::Unknown(format!("failed to run git merge-base: {}", e)),
        }
    }

    pub(super) fn classify_merge_base_result(
        status_code: Option<i32>,
        stderr: &[u8],
    ) -> AncestorCheck {
        match status_code {
            Some(0) => AncestorCheck::Ancestor,
            Some(1) => AncestorCheck::NotAncestor,
            Some(code) => {
                let stderr = String::from_utf8_lossy(stderr).trim().to_string();
                if stderr.is_empty() {
                    AncestorCheck::Unknown(format!("git merge-base exited with exit code {}", code))
                } else {
                    AncestorCheck::Unknown(format!(
                        "git merge-base exited with exit code {}: {}",
                        code, stderr
                    ))
                }
            }
            None => {
                let stderr = String::from_utf8_lossy(stderr).trim().to_string();
                if stderr.is_empty() {
                    AncestorCheck::Unknown(
                        "git merge-base terminated with no exit code".to_string(),
                    )
                } else {
                    AncestorCheck::Unknown(format!(
                        "git merge-base terminated with no exit code: {}",
                        stderr
                    ))
                }
            }
        }
    }

    /// Push all objects reachable from sha
    pub(super) fn push_objects(
        &mut self,
        sha: &str,
        dst_ref: &str,
        force_push: bool,
        remote_tip_sha: Option<&str>,
    ) -> Result<()> {
        if !force_push && remote_tip_sha == Some(sha) {
            debug!(
                "Skipping push for {} because remote tip already equals {}",
                dst_ref, sha
            );
            return Ok(());
        }

        eprint!("  Listing objects...");
        let _ = std::io::stderr().flush();
        let delta_base = (!force_push)
            .then(|| remote_tip_sha)
            .flatten()
            .map(str::to_string);
        let mut objects = self.list_objects_for_push(sha, delta_base.as_deref())?;
        if let Some(base) = delta_base.as_deref() {
            eprintln!(
                " {} objects (delta from {})",
                objects.len(),
                &base[..12.min(base.len())]
            );
        } else {
            eprintln!(" {} objects", objects.len());
        }

        info!("Pushing {} objects for {}", objects.len(), sha);

        let objects_with_content = self.read_git_objects_batch(&objects)?;
        eprintln!();

        eprint!("  Writing to local store...");
        let _ = std::io::stderr().flush();
        Self::write_objects_to_local_store(&self.storage, objects_with_content)?;
        eprintln!();

        let oid = crate::git::object::ObjectId::from_hex(sha)
            .ok_or_else(|| anyhow::anyhow!("Invalid object id: {}", sha))?;
        self.storage.write_ref(dst_ref, &Ref::Direct(oid))?;

        if dst_ref.starts_with("refs/heads/") {
            self.storage
                .write_ref("HEAD", &Ref::Symbolic(dst_ref.to_string()))?;
            debug!("Set HEAD -> {}", dst_ref);
        }

        if !self.nostr.can_sign() {
            anyhow::bail!(
                "Cannot push: no secret key for {}. You can only push to your own repos.",
                self.nostr.npub()
            );
        }

        let mut root_cid = match self.build_tree_with_progress("Building repo tree") {
            Ok(root_cid) => root_cid,
            Err(err) if delta_base.is_some() => {
                let base = delta_base.as_deref().unwrap_or_default();
                eprintln!(
                    "  Delta object set incomplete ({}); retrying against cached remote root",
                    err
                );
                if self.is_slow() {
                    eprintln!("  Reusing unchanged paths from cached remote root...");
                }
                debug!(
                    "Delta object set for {} via {} was incomplete: {}. Retrying build against cached remote root.",
                    dst_ref, base, err
                );

                let mut retried_with_cached_root = false;
                let mut cached_root_error: Option<String> = None;
                let cached_retry = if let Some(root_hash) =
                    self.nostr.get_cached_root_hash(&self.repo_name).cloned()
                {
                    let encryption_key = self
                        .nostr
                        .get_cached_encryption_key(&self.repo_name)
                        .copied();
                    match self.build_cached_fetch_tree() {
                        Ok((cached_tree, _)) => {
                            let root_bytes =
                                hex::decode(&root_hash).context("Invalid cached root hash hex")?;
                            let root_arr: [u8; 32] = root_bytes.try_into().map_err(|_| {
                                anyhow::anyhow!("Cached root hash must be 32 bytes")
                            })?;
                            let cached_root_cid = hashtree_core::Cid {
                                hash: root_arr,
                                key: encryption_key,
                            };
                            retried_with_cached_root = true;
                            Some(self.build_tree_with_base_progress(
                                "Retrying repo tree against cached remote root",
                                Some(&cached_tree),
                                Some(&cached_root_cid),
                                delta_base.as_deref(),
                            ))
                        }
                        Err(build_err) => {
                            cached_root_error =
                                Some(format!("build cached fetch tree: {}", build_err));
                            None
                        }
                    }
                } else {
                    cached_root_error = Some("no cached remote root".to_string());
                    None
                };

                match cached_retry
                    .unwrap_or_else(|| self.build_tree_with_progress("Retrying repo tree"))
                {
                    Ok(root_cid) => root_cid,
                    Err(retry_err) => {
                        if retried_with_cached_root {
                            eprintln!(
                                "  Cached-root retry still incomplete ({}); hydrating existing remote objects from cached root",
                                retry_err
                            );
                            debug!(
                                "Cached remote retry did not complete {} via {}: {}. Hydrating cached remote objects before full local import.",
                                dst_ref, base, retry_err
                            );
                        } else {
                            eprintln!(
                                "  Cached remote retry unavailable ({}); hydrating existing remote objects from cached root",
                                cached_root_error
                                    .clone()
                                    .unwrap_or_else(|| "unknown cached-root error".to_string())
                            );
                            debug!(
                                "Cached remote retry unavailable for {} via {}: {}. Hydrating cached remote objects before full local import.",
                                dst_ref,
                                base,
                                cached_root_error
                                    .clone()
                                    .unwrap_or_else(|| "unknown cached-root error".to_string())
                            );
                        }

                        if let Some(root_hash) =
                            self.nostr.get_cached_root_hash(&self.repo_name).cloned()
                        {
                            let existing_objects = self.fetch_all_git_objects(&root_hash)?;
                            eprintln!(
                                "  Importing {} cached remote object(s)",
                                existing_objects.len()
                            );
                            for (oid, content) in existing_objects {
                                self.storage.import_compressed_object(&oid, content)?;
                            }
                        }

                        match self.build_tree_with_progress(
                            "Retrying repo tree after cached-object hydration",
                        ) {
                            Ok(root_cid) => root_cid,
                            Err(post_hydration_err) => {
                                eprintln!(
                                    "  Cached-root hydration still incomplete ({}); falling back to full local import",
                                    post_hydration_err
                                );
                                debug!(
                                    "Cached remote hydration still incomplete for {} via {}: {}. Falling back to full local import.",
                                    dst_ref, base, post_hydration_err
                                );

                                eprint!("  Listing objects...");
                                let _ = std::io::stderr().flush();
                                objects = self.list_objects_to_push(sha, &[])?;
                                eprintln!(" {} objects", objects.len());

                                let objects_with_content = self.read_git_objects_batch(&objects)?;
                                eprintln!();

                                eprint!("  Writing to local store...");
                                let _ = std::io::stderr().flush();
                                Self::write_objects_to_local_store(
                                    &self.storage,
                                    objects_with_content,
                                )?;
                                eprintln!();

                                self.build_tree_with_progress(
                                    "Building repo tree from repaired local store",
                                )?
                            }
                        }
                    }
                }
            }
            Err(err) => return Err(err.into()),
        };
        if let Err(validation_err) = self.storage.validate_root_contains_direct_refs(&root_cid) {
            eprintln!(
                "  Built repo tree is missing ref object(s) ({}); rebuilding from full local import",
                validation_err
            );
            self.import_full_local_revision(sha)?;
            root_cid =
                self.build_tree_with_progress("Rebuilding repo tree after full local import")?;
            self.storage
                .validate_root_contains_direct_refs(&root_cid)
                .context("rebuilt repo tree is still missing ref objects")?;
        }
        let root_hash_hex = hex::encode(root_cid.hash);
        let chk_key = root_cid.key;
        let is_link_visible = self.url_secret.is_some();
        if self.is_slow() {
            eprintln!(
                " done (encrypted: {}, link_visible: {}, private: {})",
                chk_key.is_some(),
                is_link_visible,
                self.is_private
            );
        }

        let key_to_publish = if let (Some(chk), Some(secret)) = (chk_key, self.url_secret) {
            let mut masked = [0u8; 32];
            for i in 0..32 {
                masked[i] = chk[i] ^ secret[i];
            }
            Some(masked)
        } else {
            chk_key
        };

        let old_root_hash = self.nostr.get_cached_root_hash(&self.repo_name).cloned();
        let old_encryption_key = self
            .nostr
            .get_cached_encryption_key(&self.repo_name)
            .copied();
        let blossom_result = self.push_to_file_servers_with_diff(
            &root_hash_hex,
            chk_key.as_ref(),
            old_root_hash.as_deref(),
            old_encryption_key.as_ref(),
            !force_push,
        );

        let key_with_privacy = key_to_publish
            .as_ref()
            .map(|k| (k, is_link_visible, self.is_private));
        let (npub_url, relay_result) = self
            .nostr
            .publish_repo(&self.repo_name, &root_hash_hex, key_with_privacy)
            .map_err(|e| anyhow::anyhow!("Failed to publish repo metadata to relays: {}", e))?;

        let full_url = if let Some(secret) = self.url_secret {
            format!("{}#k={}", npub_url, hex::encode(secret))
        } else {
            npub_url.clone()
        };

        eprintln!("Published to: {}", full_url);
        if !relay_result.connected.is_empty() {
            eprintln!("  Relays: {}", relay_result.connected.join(", "));
        } else {
            eprintln!("  Relays: none");
        }
        if !relay_result.failed.is_empty() {
            eprintln!("  Relays failed: {}", relay_result.failed.join(", "));
        }
        if !blossom_result.succeeded.is_empty() {
            eprintln!("  Blossom: {}", blossom_result.succeeded.join(", "));
        }
        if !blossom_result.failed.is_empty() {
            eprintln!("  Blossom failed: {}", blossom_result.failed.join(", "));
        }

        eprintln!("  Config: ~/.hashtree/config.toml");

        if let Some(path) = npub_url.strip_prefix("htree://") {
            let viewer_url = build_repo_viewer_url(path, self.url_secret.as_ref());
            eprintln!("View at: {}", viewer_url);
        }

        match self.storage.evict_if_needed() {
            Ok(freed) if freed > 0 => {
                info!(
                    "Evicted {} bytes from shared git blob cache after push",
                    freed
                );
            }
            Ok(_) => {}
            Err(err) => {
                warn!("Failed to evict shared git blob cache after push: {}", err);
            }
        }

        Ok(())
    }

    fn list_objects_for_push(&self, sha: &str, delta_base: Option<&str>) -> Result<Vec<String>> {
        let exclude: Vec<String> = delta_base
            .map(|base| vec![base.to_string()])
            .unwrap_or_default();
        self.list_objects_to_push(sha, &exclude)
    }

    fn write_objects_to_local_store(
        storage: &crate::git::storage::GitStorage,
        objects_with_content: Vec<(crate::git::object::ObjectType, Vec<u8>)>,
    ) -> Result<()> {
        let total = objects_with_content.len();
        for (i, (obj_type, content)) in objects_with_content.into_iter().enumerate() {
            storage.write_raw_object(obj_type, &content)?;
            if (i + 1) % 1000 == 0 || i + 1 == total {
                eprint!("\r  Writing to local store: {}/{}", i + 1, total);
                let _ = std::io::stderr().flush();
            }
        }
        Ok(())
    }

    fn import_full_local_revision(&mut self, sha: &str) -> Result<()> {
        eprint!("  Listing objects...");
        let _ = std::io::stderr().flush();
        let objects = self.list_objects_to_push(sha, &[])?;
        eprintln!(" {} objects (full rebuild)", objects.len());

        let objects_with_content = self.read_git_objects_batch(&objects)?;
        eprintln!();

        eprint!("  Writing to local store...");
        let _ = std::io::stderr().flush();
        Self::write_objects_to_local_store(&self.storage, objects_with_content)?;
        eprintln!();

        Ok(())
    }

    /// Find merged-in parent SHAs from merge commits in a pushed range.
    pub(super) fn find_merged_parent_shas(&self, range: &str) -> Result<HashSet<String>> {
        let output = Command::new("git")
            .args(["rev-list", "--merges", "--parents", range])
            .output()
            .context("Failed to run git rev-list")?;

        if !output.status.success() {
            return Ok(HashSet::new());
        }

        let merged_parent_shas: HashSet<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .flat_map(|line| line.split_whitespace().skip(2).map(str::to_owned))
            .collect();

        Ok(merged_parent_shas)
    }

    /// Detect merged PRs in pushed refs and publish status events
    pub(super) fn detect_and_mark_merged_prs(&self, pushed_refs: &[(String, String)]) {
        let open_prs = match self
            .nostr
            .fetch_prs(&self.repo_name, PullRequestStateFilter::Open)
        {
            Ok(prs) => prs,
            Err(e) => {
                debug!("Failed to fetch open PRs: {}", e);
                return;
            }
        };

        if open_prs.is_empty() {
            return;
        }

        let merge_candidates = pushed_refs
            .iter()
            .filter_map(|(dst_ref, sha)| {
                dst_ref
                    .strip_prefix("refs/heads/")
                    .map(|branch_name| (dst_ref, branch_name, sha))
            })
            .filter_map(|(dst_ref, branch_name, sha)| {
                let Some(old_sha) = self.remote_refs.get(dst_ref) else {
                    debug!(
                        "Skipping PR auto-merge detection for {}: previous remote tip is unknown",
                        dst_ref
                    );
                    return None;
                };

                let range = format!("{}..{}", old_sha, sha);
                let merged_parent_shas = match self.find_merged_parent_shas(&range) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!("Failed to find merge commits for {}: {}", dst_ref, e);
                        return None;
                    }
                };

                if merged_parent_shas.is_empty() {
                    return None;
                }

                debug!(
                    "Found {} merged parent SHAs in push to {}",
                    merged_parent_shas.len(),
                    dst_ref
                );

                Some((branch_name, merged_parent_shas))
            });

        for (branch_name, merged_parent_shas) in merge_candidates {
            let matching_prs = open_prs
                .iter()
                .filter(|pr| pr.target_branch.as_deref().unwrap_or("master") == branch_name)
                .filter(|pr| {
                    pr.commit_tip
                        .as_ref()
                        .is_some_and(|commit_tip| merged_parent_shas.contains(commit_tip))
                });

            for pr in matching_prs {
                match self
                    .nostr
                    .publish_pr_merged_status(&pr.event_id, &pr.author_pubkey)
                {
                    Ok(()) => {
                        eprintln!(
                            "PR auto-merged: ({})...",
                            &pr.event_id[..12.min(pr.event_id.len())]
                        );
                    }
                    Err(e) => {
                        debug!("Failed to publish PR merged status: {}", e);
                    }
                }
            }
        }
    }

    /// Push content to file servers (blossom) with efficient diff-based upload
    pub(super) fn push_to_file_servers_with_diff(
        &self,
        root_hash: &str,
        encryption_key: Option<&[u8; 32]>,
        old_root_hash: Option<&str>,
        old_encryption_key: Option<&[u8; 32]>,
        trust_server_old_tree_coverage: bool,
    ) -> BlossomResult {
        use hashtree_core::crypto::decrypt_chk;
        use hashtree_core::try_decode_tree_node;

        let store = self.storage.store();
        let blossom = self.nostr.blossom();
        let configured: Vec<String> = blossom.write_servers().to_vec();

        let rt = match new_multi_thread_runtime() {
            Ok(rt) => rt,
            Err(e) => {
                warn!("Failed to create runtime for blossom upload: {}", e);
                return BlossomResult {
                    configured: configured.clone(),
                    succeeded: vec![],
                    failed: configured,
                };
            }
        };

        let root_bytes = match hex::decode(root_hash) {
            Ok(b) if b.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&b);
                arr
            }
            _ => {
                warn!("Invalid root hash: {}", root_hash);
                return BlossomResult {
                    configured: configured.clone(),
                    succeeded: vec![],
                    failed: configured,
                };
            }
        };

        let old_root_bytes: Option<[u8; 32]> = old_root_hash.and_then(|h| {
            hex::decode(h).ok().and_then(|b| {
                if b.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&b);
                    Some(arr)
                } else {
                    None
                }
            })
        });

        let verbose = self.is_slow();
        let force_upload = self.config.blossom.force_upload;
        let trust_server_old_tree_coverage = trust_server_old_tree_coverage && !force_upload;
        let success = rt.block_on(async {
            use hashtree_core::{collect_hashes, Cid, HashTree, HashTreeConfig};
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use std::sync::Arc;
            use tokio::sync::mpsc;

            let uploaded = Arc::new(AtomicUsize::new(0));
            let skipped_diff = Arc::new(AtomicUsize::new(0));
            let skipped_server = Arc::new(AtomicUsize::new(0));
            let failed = Arc::new(AtomicUsize::new(0));
            let completed = Arc::new(AtomicUsize::new(0));
            let discovered_total = Arc::new(AtomicUsize::new(1));
            let discovery_complete = Arc::new(AtomicBool::new(false));

            let old_hashes: HashSet<[u8; 32]> = if let Some(old_root) = old_root_bytes {
                if old_root == root_bytes {
                    if verbose {
                        eprintln!("  No changes detected (same root hash)");
                    }
                    return true;
                }

                if verbose {
                    eprint!("  Computing diff from previous tree...");
                    let _ = std::io::stderr().flush();
                }

                let tree = HashTree::new(HashTreeConfig::new(store.clone()));
                let old_cid = Cid {
                    hash: old_root,
                    key: old_encryption_key.copied(),
                };

                match collect_hashes(&tree, &old_cid, 32).await {
                    Ok(hashes) => {
                        if verbose {
                            eprintln!(" {} hashes in old tree", hashes.len());
                        }
                        hashes
                    }
                    Err(local_err) => {
                        match self.build_cached_fetch_tree() {
                            Ok((cached_tree, _)) => match collect_hashes(&cached_tree, &old_cid, 32).await {
                                Ok(hashes) => {
                                    if verbose {
                                        eprintln!(
                                            " {} hashes in old tree (via cached fetch tree after local miss: {})",
                                            hashes.len(),
                                            local_err
                                        );
                                    }
                                    hashes
                                }
                                Err(cached_err) => {
                                    if verbose {
                                        eprintln!(" failed locally: {}", local_err);
                                        eprintln!("  Cached old-tree walk failed too: {}", cached_err);
                                        eprintln!("  Falling back to full upload");
                                    }
                                    HashSet::new()
                                }
                            },
                            Err(build_err) => {
                                if verbose {
                                    eprintln!(" failed locally: {}", local_err);
                                    eprintln!("  Could not build cached fetch tree: {}", build_err);
                                    eprintln!("  Falling back to full upload");
                                }
                                HashSet::new()
                            }
                        }
                    }
                }
            } else {
                HashSet::new()
            };

            let has_old_tree = !old_hashes.is_empty();
            let all_servers: Vec<String> = blossom.write_servers().to_vec();
            let servers_needing_full: Arc<Vec<String>> = if force_upload {
                Arc::new(all_servers.clone())
            } else if has_old_tree && !all_servers.is_empty() {
                let old_root = old_root_bytes.unwrap();
                let mut sample_hashes = vec![hex::encode(old_root)];
                for hash in old_hashes
                    .iter()
                    .filter(|h| **h != old_root)
                    .take(SERVER_COVERAGE_SAMPLE_SIZE.saturating_sub(1))
                {
                    sample_hashes.push(hex::encode(hash));
                }
                let sample_refs: Vec<&str> = sample_hashes.iter().map(|s| s.as_str()).collect();
                let mut needs_full = Vec::new();
                for server in &all_servers {
                    if !blossom
                        .server_has_tree_samples(
                            server,
                            &sample_refs,
                            SERVER_COVERAGE_SAMPLE_SIZE,
                        )
                        .await
                    {
                        needs_full.push(server.clone());
                    }
                }
                if !needs_full.is_empty() && verbose {
                    let server_names: Vec<_> = needs_full
                        .iter()
                        .map(|s| {
                            s.trim_start_matches("https://")
                                .trim_start_matches("http://")
                                .split('/')
                                .next()
                                .unwrap_or(s)
                        })
                        .collect();
                    eprintln!(
                        "  Full upload needed: {} (missing old tree)",
                        server_names.join(", ")
                    );
                }
                Arc::new(needs_full)
            } else {
                Arc::new(Vec::new())
            };
            let prune_known_subtrees =
                has_old_tree && trust_server_old_tree_coverage && servers_needing_full.is_empty();

            const CHANNEL_SIZE: usize = 100;
            let upload_concurrency = self.upload_concurrency(all_servers.len());
            let (tx, rx) = mpsc::channel::<(Vec<u8>, bool, bool)>(CHANNEL_SIZE);

            let upload_handle = {
                let blossom = blossom.clone();
                let uploaded = Arc::clone(&uploaded);
                let skipped_server = Arc::clone(&skipped_server);
                let failed = Arc::clone(&failed);
                let completed = Arc::clone(&completed);
                let skipped_diff = Arc::clone(&skipped_diff);
                let discovered_total = Arc::clone(&discovered_total);
                let discovery_complete = Arc::clone(&discovery_complete);
                let servers_needing_full = Arc::clone(&servers_needing_full);

                tokio::spawn(async move {
                    use futures::stream::StreamExt;
                    use tokio_stream::wrappers::ReceiverStream;

                    let stream = ReceiverStream::new(rx);
                    stream
                        .map(|(data, from_old_tree, force_all_servers)| {
                            let blossom = blossom.clone();
                            let uploaded = Arc::clone(&uploaded);
                            let skipped_server = Arc::clone(&skipped_server);
                            let failed = Arc::clone(&failed);
                            let completed = Arc::clone(&completed);
                            let skipped_diff = Arc::clone(&skipped_diff);
                            let discovered_total = Arc::clone(&discovered_total);
                            let discovery_complete = Arc::clone(&discovery_complete);
                            let servers_needing_full = Arc::clone(&servers_needing_full);
                            async move {
                                let result = if force_all_servers {
                                    if blossom.write_servers().len() <= 1 {
                                        blossom.upload_if_missing(&data).await
                                    } else {
                                        blossom
                                            .upload_to_all_servers(&data)
                                            .await
                                            .map(|(h, c)| (h, c > 0))
                                    }
                                } else if from_old_tree && !servers_needing_full.is_empty() {
                                    if servers_needing_full.len() == 1 {
                                        blossom
                                            .clone()
                                            .with_write_servers(servers_needing_full.as_ref().clone())
                                            .upload_if_missing(&data)
                                            .await
                                    } else {
                                        blossom
                                            .upload_to_selected_servers(
                                                &data,
                                                servers_needing_full.as_ref().as_slice(),
                                            )
                                            .await
                                            .map(|(h, c)| (h, c > 0))
                                    }
                                } else {
                                    blossom.upload_if_missing(&data).await
                                };
                                match result {
                                    Ok((_, true)) => {
                                        uploaded.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Ok((_, false)) => {
                                        skipped_server.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(e) => {
                                        failed.fetch_add(1, Ordering::Relaxed);
                                        eprintln!("\n  Upload failed ({} bytes): {}", data.len(), e);
                                    }
                                }
                                let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                                if count == 1 || count.is_multiple_of(10) {
                                    let discovered = discovered_total.load(Ordering::Relaxed);
                                    let total = discovery_complete
                                        .load(Ordering::Relaxed)
                                        .then_some(discovered);
                                    emit_upload_progress(upload_progress(
                                        count,
                                        discovered,
                                        total,
                                        uploaded.load(Ordering::Relaxed),
                                        skipped_diff.load(Ordering::Relaxed),
                                        skipped_server.load(Ordering::Relaxed),
                                        failed.load(Ordering::Relaxed),
                                        has_old_tree,
                                    ));
                                }
                            }
                        })
                        .buffer_unordered(upload_concurrency)
                        .for_each(|_| async {})
                        .await;
                })
            };

            let mut visited: HashSet<[u8; 32]> = HashSet::new();
            let mut queued: HashSet<[u8; 32]> = HashSet::new();
            let mut queue: Vec<([u8; 32], Option<[u8; 32]>)> = Vec::new();
            let _ = queue_hash_if_new(&mut queue, &mut queued, root_bytes, encryption_key.copied());

            eprint!(
                "{}",
                upload_progress(
                    0,
                    discovered_total.load(Ordering::Relaxed),
                    None,
                    0,
                    0,
                    0,
                    0,
                    has_old_tree
                )
                .format()
            );
            let _ = std::io::stderr().flush();

            while let Some((hash, key)) = queue.pop() {
                if visited.contains(&hash) {
                    continue;
                }
                visited.insert(hash);
                let discovered = discovered_total.load(Ordering::Relaxed);
                let from_old_tree = old_hashes.contains(&hash);

                let mut force_all_servers_for_hash = false;
                if from_old_tree {
                    if trust_server_old_tree_coverage {
                        if servers_needing_full.is_empty() {
                            skipped_diff.fetch_add(1, Ordering::Relaxed);
                            let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                            if count == 1 || count.is_multiple_of(10) {
                                emit_upload_progress(upload_progress(
                                    count,
                                    discovered,
                                    None,
                                    uploaded.load(Ordering::Relaxed),
                                    skipped_diff.load(Ordering::Relaxed),
                                    skipped_server.load(Ordering::Relaxed),
                                    failed.load(Ordering::Relaxed),
                                    has_old_tree,
                                ));
                            }
                            continue;
                        }
                    } else {
                        let hash_hex = hex::encode(hash);
                        let mut missing_on_any_server = false;
                        for server in blossom.write_servers() {
                            if !blossom.exists_on_server(&hash_hex, server).await {
                                missing_on_any_server = true;
                                break;
                            }
                        }
                        if !missing_on_any_server {
                            skipped_diff.fetch_add(1, Ordering::Relaxed);
                            let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                            if count == 1 || count.is_multiple_of(10) {
                                emit_upload_progress(upload_progress(
                                    count,
                                    discovered,
                                    None,
                                    uploaded.load(Ordering::Relaxed),
                                    skipped_diff.load(Ordering::Relaxed),
                                    skipped_server.load(Ordering::Relaxed),
                                    failed.load(Ordering::Relaxed),
                                    has_old_tree,
                                ));
                            }
                            continue;
                        }
                        force_all_servers_for_hash = true;
                    }
                }

                let data = match store.get_sync(&hash) {
                    Ok(Some(data)) => data,
                    Ok(None) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                        eprintln!("\n  Missing from local store: {}", hex::encode(hash));
                        if count == 1 || count.is_multiple_of(10) {
                            emit_upload_progress(upload_progress(
                                count,
                                discovered,
                                None,
                                uploaded.load(Ordering::Relaxed),
                                skipped_diff.load(Ordering::Relaxed),
                                skipped_server.load(Ordering::Relaxed),
                                failed.load(Ordering::Relaxed),
                                has_old_tree,
                            ));
                        }
                        continue;
                    }
                    Err(e) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                        eprintln!("\n  Store read error for {}: {}", hex::encode(hash), e);
                        if count == 1 || count.is_multiple_of(10) {
                            emit_upload_progress(upload_progress(
                                count,
                                discovered,
                                None,
                                uploaded.load(Ordering::Relaxed),
                                skipped_diff.load(Ordering::Relaxed),
                                skipped_server.load(Ordering::Relaxed),
                                failed.load(Ordering::Relaxed),
                                has_old_tree,
                            ));
                        }
                        continue;
                    }
                };

                let plaintext = if let Some(k) = key {
                    match decrypt_chk(&data, &k) {
                        Ok(p) => p,
                        Err(_) => data.clone(),
                    }
                } else {
                    data.clone()
                };

                if let Some(node) = try_decode_tree_node(&plaintext) {
                    queue_links_for_diff_upload(
                        &mut queue,
                        &mut queued,
                        &node.links,
                        &old_hashes,
                        prune_known_subtrees,
                        &discovered_total,
                    );
                }

                if tx
                    .send((data, from_old_tree, force_all_servers_for_hash))
                    .await
                    .is_err()
                {
                    break;
                }
                let discovered = discovered_total.load(Ordering::Relaxed);
                if discovered.is_multiple_of(100) {
                    emit_upload_progress(upload_progress(
                        completed.load(Ordering::Relaxed),
                        discovered,
                        None,
                        uploaded.load(Ordering::Relaxed),
                        skipped_diff.load(Ordering::Relaxed),
                        skipped_server.load(Ordering::Relaxed),
                        failed.load(Ordering::Relaxed),
                        has_old_tree,
                    ));
                }
            }

            discovery_complete.store(true, Ordering::Relaxed);

            let final_total_seen = discovered_total.load(Ordering::Relaxed);
            emit_upload_progress(upload_progress(
                completed.load(Ordering::Relaxed),
                final_total_seen,
                Some(final_total_seen),
                uploaded.load(Ordering::Relaxed),
                skipped_diff.load(Ordering::Relaxed),
                skipped_server.load(Ordering::Relaxed),
                failed.load(Ordering::Relaxed),
                has_old_tree,
            ));

            drop(tx);
            let _ = upload_handle.await;

            let final_uploaded = uploaded.load(Ordering::Relaxed);
            let final_skipped_diff = skipped_diff.load(Ordering::Relaxed);
            let final_skipped_server = skipped_server.load(Ordering::Relaxed);
            let final_failed = failed.load(Ordering::Relaxed);
            let final_completed = completed.load(Ordering::Relaxed);

            emit_upload_progress(upload_progress(
                final_completed,
                final_total_seen,
                Some(final_total_seen),
                final_uploaded,
                final_skipped_diff,
                final_skipped_server,
                final_failed,
                has_old_tree,
            ));
            eprintln!();

            info!(
                "Blossom upload complete: {} uploaded, {} unchanged (diff), {} already on server, {} failed",
                final_uploaded, final_skipped_diff, final_skipped_server, final_failed
            );

            final_uploaded > 0 || final_skipped_server > 0 || final_skipped_diff > 0
        });

        if success {
            BlossomResult {
                configured: configured.clone(),
                succeeded: configured,
                failed: vec![],
            }
        } else {
            BlossomResult {
                configured: configured.clone(),
                succeeded: vec![],
                failed: configured,
            }
        }
    }

    /// Collect all hashes reachable from a root hash by walking the merkle tree
    #[allow(dead_code)]
    pub(super) fn collect_tree_hashes(&self, root_hash: &str) -> Result<Vec<[u8; 32]>> {
        use hashtree_core::try_decode_tree_node;

        let store = self.storage.store();
        let mut hashes = Vec::new();
        let mut visited: HashSet<[u8; 32]> = HashSet::new();

        let root_bytes = hex::decode(root_hash).context("Invalid root hash hex")?;
        if root_bytes.len() != 32 {
            bail!("Root hash must be 32 bytes");
        }
        let mut root: [u8; 32] = [0u8; 32];
        root.copy_from_slice(&root_bytes);

        let mut queue = vec![root];

        while let Some(hash) = queue.pop() {
            if visited.contains(&hash) {
                continue;
            }
            visited.insert(hash);
            hashes.push(hash);

            if let Ok(Some(data)) = store.get_sync(&hash) {
                if let Some(node) = try_decode_tree_node(&data) {
                    for link in node.links {
                        if !visited.contains(&link.hash) {
                            queue.push(link.hash);
                        }
                    }
                }
            }
        }

        debug!(
            "Collected {} hashes from tree {}",
            hashes.len(),
            &root_hash[..12]
        );
        Ok(hashes)
    }
}

#[cfg(test)]
mod tests {
    use super::effective_upload_concurrency;

    #[test]
    fn upload_concurrency_uses_configured_parallelism_for_single_server() {
        assert_eq!(effective_upload_concurrency(1, 10), 10);
    }

    #[test]
    fn upload_concurrency_clamps_zero_to_one() {
        assert_eq!(effective_upload_concurrency(1, 0), 1);
        assert_eq!(effective_upload_concurrency(0, 10), 1);
    }
}
