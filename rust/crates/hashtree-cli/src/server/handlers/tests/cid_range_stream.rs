use super::super::{stream_file_range_cid_with_fetch, CID_RANGE_STREAM_CHUNK_SIZE};
use super::test_app_state;
use crate::storage::HashtreeStore;
use futures::StreamExt;
use hashtree_core::{HashTree, HashTreeConfig};
use std::sync::Arc;
use tempfile::TempDir;

/// Stream `[start, end_inclusive]` and return the body with its piece sizes.
async fn collect_range(
    state: &crate::server::auth::AppState,
    cid: &hashtree_core::Cid,
    start: u64,
    end_inclusive: u64,
) -> (Vec<u8>, Vec<usize>) {
    let mut body = Vec::new();
    let mut pieces = Vec::new();
    let mut stream = Box::pin(stream_file_range_cid_with_fetch(
        state.clone(),
        cid.clone(),
        start,
        end_inclusive,
    ));
    while let Some(piece) = stream.next().await {
        let piece = piece.expect("range piece");
        pieces.push(piece.len());
        body.extend_from_slice(&piece);
    }
    (body, pieces)
}

#[tokio::test]
async fn cid_range_stream_matches_file_bytes_across_leaf_boundaries() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    // Leaves a little larger than one response piece, so pieces straddle
    // leaf boundaries; two links per node forces interior nodes.
    let chunk_size = CID_RANGE_STREAM_CHUNK_SIZE as usize + 40_000;
    let data: Vec<u8> = (0..(chunk_size * 4 + 12_345))
        .map(|i| (i * 31 % 251) as u8)
        .collect();
    let last = data.len() as u64 - 1;

    for encrypted in [true, false] {
        let mut config = HashTreeConfig::new(store.store_arc())
            .with_chunk_size(chunk_size)
            .with_max_links(2);
        if !encrypted {
            config = config.public();
        }
        let (cid, _) = HashTree::new(config).put(&data).await.unwrap();
        assert_eq!(cid.key.is_some(), encrypted);

        let (body, pieces) = collect_range(&state, &cid, 0, last).await;
        assert_eq!(body, data, "encrypted={encrypted} full body");
        assert!(pieces
            .iter()
            .all(|len| *len > 0 && *len <= CID_RANGE_STREAM_CHUNK_SIZE as usize));
        assert_eq!(pieces[0], CID_RANGE_STREAM_CHUNK_SIZE as usize);

        let boundary = chunk_size as u64;
        for (start, end) in [
            (0, 0),
            (last, last),
            (boundary - 1, boundary),
            (boundary + 7, 3 * boundary + 99),
            (17, 2 * boundary - 1),
        ] {
            let (body, _) = collect_range(&state, &cid, start, end).await;
            assert_eq!(
                body,
                data[start as usize..=end as usize].to_vec(),
                "encrypted={encrypted} range {start}-{end}"
            );
        }
    }
}
