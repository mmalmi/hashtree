use hashtree_core::encryption::{decrypt_object, encrypt_object, EncryptionMode, ObjectKey};
use hashtree_core::{crypto, sha256};

#[test]
fn legacy_and_versioned_chk_preserve_ciphertext() {
    let (legacy, old_key) = crypto::encrypt_chk(b"hello").unwrap();
    let (versioned, key) = encrypt_object(b"hello", EncryptionMode::ChkV1).unwrap();
    assert_eq!(legacy, versioned);
    assert_eq!(key.as_bytes()[0], 1);
    assert_eq!(&key.as_bytes()[1..], &old_key);
    assert_eq!(
        decrypt_object(&legacy, &ObjectKey::from(old_key)).unwrap(),
        b"hello"
    );
    assert_eq!(decrypt_object(&versioned, &key).unwrap(), b"hello");
}

#[test]
fn random_suite_matches_published_vector_and_randomizes_new_writes() {
    let key = ObjectKey::from_bytes(&[2; 33]).unwrap();
    let bytes =
        hex::decode("000102030405060708090a0b216d8e96be582844fc3a4e2a23eecfbe1a29cd5b6b").unwrap();
    assert_eq!(decrypt_object(&bytes, &key).unwrap(), b"hello");
    assert_eq!(
        hex::encode(sha256(&bytes)),
        "819b3a52aa2550b762399299892a6b613b7071c57d95787a1915310afd50fc7b"
    );
    let (a, ka) = encrypt_object(b"hello", EncryptionMode::RndV1).unwrap();
    let (b, kb) = encrypt_object(b"hello", EncryptionMode::RndV1).unwrap();
    assert_ne!(a, b);
    assert_ne!(ka, kb);
    assert_eq!(decrypt_object(&a, &ka).unwrap(), b"hello");
    let mut bad = a;
    bad[12] ^= 1;
    assert!(decrypt_object(&bad, &ka).is_err());
}

#[test]
fn invalid_keys_never_fall_back_to_plaintext_or_chk() {
    for len in [0, 1, 31, 34, 64] {
        assert!(ObjectKey::from_bytes(&vec![1; len]).is_err());
    }
    for suite in [0, 3, 255] {
        let mut key = [1; 33];
        key[0] = suite;
        assert!(ObjectKey::from_bytes(&key).is_err());
    }
}

#[tokio::test]
async fn all_suites_and_legacy_data_work_through_default_tree_readers() {
    use futures::TryStreamExt;
    use hashtree_core::{
        nhash_decode, nhash_encode_full, DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore,
        NHashData,
    };
    use std::sync::Arc;
    let store = Arc::new(MemoryStore::new());
    let reader = HashTree::new(HashTreeConfig::new(store.clone()).with_max_links(2));
    let data = b"A file spanning several chunks.";
    let mut entries = Vec::new();
    for (i, mode) in [
        EncryptionMode::LegacyChk,
        EncryptionMode::ChkV1,
        EncryptionMode::RndV1,
    ]
    .into_iter()
    .enumerate()
    {
        let writer = HashTree::new(
            HashTreeConfig::new(store.clone())
                .with_chunk_size(7)
                .with_max_links(2),
        )
        .with_encryption_mode(mode);
        let (cid, size) = writer.put(data).await.unwrap();
        assert_eq!(cid.key.unwrap().mode(), mode);
        assert_eq!(reader.get(&cid, None).await.unwrap().unwrap(), data);
        assert_eq!(
            reader
                .read_file_range_cid(&cid, 5, Some(23))
                .await
                .unwrap()
                .unwrap(),
            data[5..23]
        );
        let chunks: Vec<Vec<u8>> = reader.get_stream(&cid).try_collect().await.unwrap();
        assert_eq!(chunks.concat(), data);
        let reference = NHashData {
            hash: cid.hash,
            decrypt_key: cid.key,
        };
        assert_eq!(
            nhash_decode(&nhash_encode_full(&reference).unwrap()).unwrap(),
            reference
        );
        let (streamed, _) = writer
            .put_stream(futures::io::Cursor::new(data))
            .await
            .unwrap();
        assert_eq!(streamed.key.unwrap().mode(), mode);
        assert_eq!(reader.get(&streamed, None).await.unwrap().unwrap(), data);
        entries.push(
            DirEntry::from_cid(i.to_string(), &cid)
                .with_size(size)
                .with_link_type(LinkType::File),
        );
    }
    let public = HashTree::new(HashTreeConfig::new(store.clone()).public());
    let (plain, size) = public.put(data).await.unwrap();
    entries.push(DirEntry::from_cid("plain", &plain).with_size(size));
    let root = reader.put_directory(entries.clone()).await.unwrap();
    assert_eq!(root.key.unwrap().mode(), EncryptionMode::ChkV1);
    for entry in reader.list_directory(&root).await.unwrap() {
        assert_eq!(
            reader
                .get(
                    &hashtree_core::Cid {
                        hash: entry.hash,
                        key: entry.key
                    },
                    None
                )
                .await
                .unwrap()
                .unwrap(),
            data
        );
    }
    let random =
        HashTree::new(HashTreeConfig::new(store)).with_encryption_mode(EncryptionMode::RndV1);
    let root = random.put_directory(entries).await.unwrap();
    let edited = reader
        .set_entry(&root, &[], "added", &plain, size, LinkType::Blob)
        .await
        .unwrap();
    assert_eq!(edited.key.unwrap().mode(), EncryptionMode::RndV1);
}

#[test]
fn versioned_keys_do_not_enable_legacy_plaintext_fallback() {
    use hashtree_core::{decode_tree_node_by_cid, encode_and_hash, xor_key_bytes, Cid, TreeNode};
    let (bytes, hash) = encode_and_hash(&TreeNode::dir(vec![])).unwrap();
    for mode in [EncryptionMode::ChkV1, EncryptionMode::RndV1] {
        let (_, key) = encrypt_object(b"hello", mode).unwrap();
        assert!(decode_tree_node_by_cid(&Cid::encrypted(hash, key), bytes.clone()).is_err());
        let secret = [7; 33];
        let masked = xor_key_bytes(key.as_bytes(), &secret).unwrap();
        assert_eq!(xor_key_bytes(&masked, &secret).unwrap(), key.as_bytes());
        assert!(xor_key_bytes(key.as_bytes(), &[7; 32]).is_err());
    }
}

#[tokio::test]
async fn mixed_chunks_are_independently_readable_under_plain_and_encrypted_roots() {
    use futures::TryStreamExt;
    use hashtree_core::{encode_and_hash, HashTree, HashTreeConfig, Link, MemoryStore, TreeNode};
    use std::sync::Arc;
    let store = Arc::new(MemoryStore::new());
    let reader = HashTree::new(HashTreeConfig::new(store.clone()));
    let plain = HashTree::new(HashTreeConfig::new(store.clone()).public());
    let raw_reader = hashtree_core::reader::TreeReader::new(store.clone());
    let mut links = Vec::new();
    for (i, mode) in [
        None,
        Some(EncryptionMode::LegacyChk),
        Some(EncryptionMode::ChkV1),
        Some(EncryptionMode::RndV1),
    ]
    .into_iter()
    .enumerate()
    {
        let part = [i as u8; 2];
        let (cid, size) = match mode {
            Some(mode) => HashTree::new(HashTreeConfig::new(store.clone()))
                .with_encryption_mode(mode)
                .put(&part)
                .await
                .unwrap(),
            None => plain.put(&part).await.unwrap(),
        };
        let mut link = Link::new(cid.hash).with_size(size);
        link.key = cid.key;
        links.push(link);
    }
    let (manifest, _) = encode_and_hash(&TreeNode::file(links)).unwrap();
    let expected = [0, 0, 1, 1, 2, 2, 3, 3];
    for writer in [&reader, &plain] {
        let (cid, _) = writer.put(&manifest).await.unwrap();
        assert_eq!(reader.get(&cid, None).await.unwrap().unwrap(), expected);
        assert_eq!(raw_reader.get(&cid).await.unwrap().unwrap(), expected);
        assert_eq!(
            reader
                .read_file_range_cid(&cid, 1, Some(7))
                .await
                .unwrap()
                .unwrap(),
            expected[1..7]
        );
        let chunks: Vec<Vec<u8>> = reader.get_stream(&cid).try_collect().await.unwrap();
        assert_eq!(chunks.concat(), expected);
    }
}
