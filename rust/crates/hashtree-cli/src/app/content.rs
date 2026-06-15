use anyhow::Result;

pub(crate) async fn add_directory_with_progress<S: hashtree_core::store::Store>(
    tree: &hashtree_core::HashTree<S>,
    dir: &std::path::Path,
    respect_gitignore: bool,
    progress: Option<&hashtree_cli::AddProgress>,
) -> Result<hashtree_core::Cid> {
    use futures::io::AllowStdIo;
    use hashtree_cli::ignore_rules::build_content_walker;
    use hashtree_core::{DirEntry, LinkType};
    use std::collections::HashMap;

    // Collect files by their parent directory path
    let mut dir_contents: HashMap<String, Vec<(String, hashtree_core::Cid)>> = HashMap::new();

    // Use ignore crate for gitignore-aware walking
    let walker = build_content_walker(dir, respect_gitignore);

    for result in walker {
        let entry = result?;
        let path = entry.path();

        // Skip the root directory itself
        if path == dir {
            continue;
        }

        let relative = path.strip_prefix(dir).unwrap_or(path);

        if path.is_file() {
            let file = std::fs::File::open(path)
                .map_err(|e| anyhow::anyhow!("Failed to open file {}: {}", path.display(), e))?;
            if let Some(progress) = progress {
                if let Ok(metadata) = file.metadata() {
                    progress.record_file_discovered(metadata.len());
                }
            }
            let (cid, _size) = tree
                .put_stream_with_progress(AllowStdIo::new(file), |bytes| {
                    if let Some(progress) = progress {
                        progress.record_bytes(bytes);
                    }
                })
                .await
                .map_err(|e| anyhow::anyhow!("Failed to add file {}: {}", path.display(), e))?;
            if let Some(progress) = progress {
                progress.record_file_finished();
            }

            let parent = relative
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let name = relative
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            dir_contents.entry(parent).or_default().push((name, cid));
        } else if path.is_dir() {
            // Ensure directory entry exists
            let dir_path = relative.to_string_lossy().to_string();
            dir_contents.entry(dir_path).or_default();
        }
    }

    // Build directory tree bottom-up
    let mut dirs: Vec<String> = dir_contents.keys().cloned().collect();
    dirs.sort_by(|a, b| {
        let depth_a = a.matches('/').count() + if a.is_empty() { 0 } else { 1 };
        let depth_b = b.matches('/').count() + if b.is_empty() { 0 } else { 1 };
        depth_b.cmp(&depth_a) // Deepest first
    });

    let mut dir_cids: HashMap<String, hashtree_core::Cid> = HashMap::new();

    for dir_path in dirs {
        let files = dir_contents.get(&dir_path).cloned().unwrap_or_default();

        let mut entries: Vec<DirEntry> = files
            .into_iter()
            .map(|(name, cid)| DirEntry::from_cid(name, &cid))
            .collect();

        // Add subdirectory entries
        for (subdir_path, cid) in &dir_cids {
            let parent = std::path::Path::new(subdir_path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            if parent == dir_path {
                let name = std::path::Path::new(subdir_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                entries.push(DirEntry::from_cid(name, cid).with_link_type(LinkType::Dir));
            }
        }

        let cid = tree
            .put_directory(entries)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create directory node: {}", e))?;

        dir_cids.insert(dir_path, cid);
    }

    // Return root directory cid
    dir_cids
        .get("")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("No root directory"))
}

#[cfg(test)]
mod tests {
    use super::add_directory_with_progress;
    use hashtree_core::{
        decode_tree_node, decrypt_chk, store::Store, HashTree, HashTreeConfig, LinkType,
        MemoryStore,
    };
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn add_directory_marks_nested_directories_as_dir_links() {
        let temp_dir = TempDir::new().unwrap();
        let site_dir = temp_dir.path().join("site");
        let assets_dir = site_dir.join("assets");
        std::fs::create_dir_all(&assets_dir).unwrap();
        std::fs::write(site_dir.join("index.html"), "<html>ok</html>").unwrap();
        std::fs::write(assets_dir.join("main.js"), "console.log('ok');").unwrap();

        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        let root = add_directory_with_progress(&tree, &site_dir, true, None)
            .await
            .unwrap();

        let root_bytes = Store::get(store.as_ref(), &root.hash)
            .await
            .unwrap()
            .unwrap();
        let root_plaintext = match root.key {
            Some(key) => decrypt_chk(&root_bytes, &key).unwrap(),
            None => root_bytes,
        };
        let root_node = decode_tree_node(&root_plaintext).unwrap();

        let assets_link = root_node
            .links
            .iter()
            .find(|link| link.name.as_deref() == Some("assets"))
            .expect("assets link");
        assert_eq!(assets_link.link_type, LinkType::Dir);
    }
}
