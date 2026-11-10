use super::{add_directory_to_tree, HashtreeStore, UploadOptions};
use hashtree_core::{Cid, HashTree, HashTreeConfig, LinkType, MemoryStore};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Walk a stored directory and return `path -> file contents`, with `None`
/// for directories.
async fn snapshot(
    tree: &HashTree<MemoryStore>,
    cid: &Cid,
    prefix: String,
    out: &mut BTreeMap<String, Option<Vec<u8>>>,
) {
    for entry in tree.list_directory(cid).await.unwrap() {
        let path = format!("{prefix}{}", entry.name);
        let child = Cid {
            hash: entry.hash,
            key: entry.key,
        };
        if entry.link_type == LinkType::Dir {
            out.insert(path.clone(), None);
            Box::pin(snapshot(tree, &child, format!("{path}/"), out)).await;
        } else {
            out.insert(path, tree.get(&child, None).await.unwrap());
        }
    }
}

#[tokio::test]
async fn directory_add_keeps_nested_empty_and_symlinked_entries() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("site");
    write(&root, "index.html", "<html>ok</html>");
    write(&root, "a/b/c/deep.txt", "deep");
    write(&root, "a/b/sibling.txt", "sibling");
    write(&root, "a/top.txt", "top");
    write(&root, "z/only-dirs/leaf/file.txt", "leaf");
    std::fs::create_dir_all(root.join("empty/inner")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("a/top.txt"), root.join("link.txt")).unwrap();

    for encrypted in [false, true] {
        let mut config = HashTreeConfig::new(Arc::new(MemoryStore::new()));
        if !encrypted {
            config = config.public();
        }
        let tree = HashTree::new(config);
        let cid = add_directory_to_tree(&tree, &root, true, None)
            .await
            .unwrap();

        let mut seen = BTreeMap::new();
        snapshot(&tree, &cid, String::new(), &mut seen).await;

        let file = |text: &str| Some(text.as_bytes().to_vec());
        let mut expected: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::from([
            ("index.html".to_string(), file("<html>ok</html>")),
            ("a".to_string(), None),
            ("a/top.txt".to_string(), file("top")),
            ("a/b".to_string(), None),
            ("a/b/sibling.txt".to_string(), file("sibling")),
            ("a/b/c".to_string(), None),
            ("a/b/c/deep.txt".to_string(), file("deep")),
            ("z".to_string(), None),
            ("z/only-dirs".to_string(), None),
            ("z/only-dirs/leaf".to_string(), None),
            ("z/only-dirs/leaf/file.txt".to_string(), file("leaf")),
            ("empty".to_string(), None),
            ("empty/inner".to_string(), None),
        ]);
        if cfg!(unix) {
            expected.insert("link.txt".to_string(), file("top"));
        }
        assert_eq!(seen, expected, "encrypted={encrypted}");
    }
}

#[tokio::test]
async fn empty_directory_adds_as_an_empty_root() {
    let temp = TempDir::new().unwrap();
    let tree = HashTree::new(HashTreeConfig::new(Arc::new(MemoryStore::new())).public());
    let cid = add_directory_to_tree(&tree, temp.path(), true, None)
        .await
        .unwrap();
    assert!(tree.list_directory(&cid).await.unwrap().is_empty());
}

#[test]
fn options_and_named_uploads_agree() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let store = HashtreeStore::new(temp.path().join("store"))?;
    let source = temp.path().join("src");
    write(&source, "one.txt", "one");
    write(&source, "dir/two.txt", "two");
    let file = source.join("one.txt");

    assert_eq!(
        store.upload_file(&file)?,
        store.upload_file_with(&file, UploadOptions::public())?
    );
    assert_eq!(
        store.upload_file_encrypted(&file)?,
        store.upload_file_with(&file, UploadOptions::encrypted())?
    );
    assert_eq!(
        store.upload_dir(&source)?,
        store.upload_dir_with(&source, UploadOptions::public())?
    );
    let encrypted = store.upload_dir_with(&source, UploadOptions::encrypted())?;
    assert_eq!(store.upload_dir_encrypted(&source)?, encrypted);
    assert!(encrypted.contains(':'), "encrypted CIDs carry their key");
    assert!(!store.upload_dir(&source)?.contains(':'));
    Ok(())
}

#[test]
fn directory_add_matches_per_file_cids_for_one_and_many_files() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let store = HashtreeStore::new(temp.path().join("store"))?;

    // One file takes the in-line path. With many files, the large ones take
    // the threaded path and the small ones stay sequential.
    for file_count in [1usize, 40] {
        let source = temp.path().join(format!("src-{file_count}"));
        for index in 0..file_count {
            let repeats = if index % 2 == 0 { 4_000 } else { index + 1 };
            write(
                &source,
                &format!("d{}/file-{index}.txt", index % 3),
                &format!("contents of file {index}").repeat(repeats),
            );
        }

        let root = Cid::parse(&store.upload_dir_with(&source, UploadOptions::encrypted())?)?;
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        for index in 0..file_count {
            let relative = format!("d{}/file-{index}.txt", index % 3);
            let in_tree = futures::executor::block_on(tree.resolve(&root, &relative))?
                .expect("file is in the directory");
            let alone = Cid::parse(&store.upload_file_encrypted(source.join(&relative))?)?;
            assert_eq!(in_tree, alone, "{relative}");
        }
        // Adding the same directory again is deterministic.
        assert_eq!(
            store.upload_dir_with(&source, UploadOptions::encrypted())?,
            root.to_string()
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn directory_add_skips_dangling_symlinks() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let store = HashtreeStore::new(temp.path().join("store"))?;
    let source = temp.path().join("src");
    for index in 0..8 {
        write(&source, &format!("file-{index}.txt"), "ok");
    }
    let without_link = store.upload_dir_with(&source, UploadOptions::public())?;

    // A dangling link is neither a file nor a directory once followed.
    std::os::unix::fs::symlink(source.join("missing"), source.join("dangling"))?;
    assert_eq!(
        store.upload_dir_with(&source, UploadOptions::public())?,
        without_link
    );
    Ok(())
}
