/// Store a directory tree from disk; shared with `HashtreeStore` uploads.
pub(crate) use hashtree_cli::storage::add_directory_to_tree as add_directory_with_progress;

#[cfg(test)]
mod tests {
    use super::add_directory_with_progress;
    use hashtree_core::{
        decode_tree_node, decrypt_object, store::Store, HashTree, HashTreeConfig, LinkType,
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
            Some(key) => decrypt_object(&root_bytes, &key).unwrap(),
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
