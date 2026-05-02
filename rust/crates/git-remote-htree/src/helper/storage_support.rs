use std::collections::HashSet;
use std::path::PathBuf;

pub(super) fn get_hashtree_data_dir() -> PathBuf {
    hashtree_config::get_data_dir()
}

pub(super) fn queue_hash_if_new(
    queue: &mut Vec<([u8; 32], Option<[u8; 32]>)>,
    queued: &mut HashSet<[u8; 32]>,
    hash: [u8; 32],
    key: Option<[u8; 32]>,
) -> bool {
    if queued.insert(hash) {
        queue.push((hash, key));
        true
    } else {
        false
    }
}

pub(super) fn build_repo_viewer_url(path: &str, url_secret: Option<&[u8; 32]>) -> String {
    match url_secret {
        Some(secret) => format!("https://git.iris.to/#/{}?k={}", path, hex::encode(secret)),
        None => format!("https://git.iris.to/#/{}", path),
    }
}
