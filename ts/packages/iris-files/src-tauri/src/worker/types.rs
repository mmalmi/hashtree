use serde::{Deserialize, Serialize};

/// CID (Content Identifier) - hash + optional encryption key
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerCid {
    pub hash: String, // hex-encoded
    pub key: Option<String>, // hex-encoded encryption key
}

/// Directory entry for listDir response
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerDirEntry {
    pub name: String,
    pub hash: String,
    pub size: u64,
    pub link_type: u8, // 0=Blob, 1=File, 2=Dir
    pub key: Option<String>,
}

/// Worker request messages from frontend
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkerRequest {
    // Lifecycle
    Init { id: String },
    Ping { id: String },

    // Store operations
    Get { id: String, hash: String },
    Put { id: String, hash: String, data: String },
    Has { id: String, hash: String },
    Delete { id: String, hash: String },

    // Tree operations
    ReadFile { id: String, cid: WorkerCid },
    ReadFileRange {
        id: String,
        cid: WorkerCid,
        start: u64,
        end: Option<u64>,
    },
    WriteFile {
        id: String,
        #[serde(rename = "parentCid")]
        parent_cid: Option<WorkerCid>,
        path: String,
        data: String, // base64
    },
    DeleteFile {
        id: String,
        #[serde(rename = "parentCid")]
        parent_cid: WorkerCid,
        path: String,
    },
    ListDir { id: String, cid: WorkerCid },
    ResolveRoot {
        id: String,
        npub: String,
        path: Option<String>,
    },

    // Nostr operations (Phase 3)
    Subscribe {
        id: String,
        filters: Vec<serde_json::Value>,
    },
    Unsubscribe {
        id: String,
        #[serde(rename = "subId")]
        sub_id: String,
    },
    Publish { id: String, event: serde_json::Value },

    // Identity
    SetIdentity {
        id: String,
        pubkey: String,
        nsec: Option<String>,
    },

    // Relay management
    SetRelays {
        id: String,
        relays: Vec<String>,
    },
    GetRelays {
        id: String,
    },

    // Social graph (Phase 4)
    UpdateFollows {
        id: String,
        pubkey: String,
        follows: Vec<String>,
    },
    GetFollows {
        id: String,
        pubkey: String,
    },
    GetFollowers {
        id: String,
        pubkey: String,
    },
    GetWotDistance {
        id: String,
        target: String,
    },
    GetUsersWithinDistance {
        id: String,
        #[serde(rename = "maxDistance")]
        max_distance: usize,
    },

    // Blossom operations (Phase 6)
    BlossomUpload {
        id: String,
        data: String, // base64
    },
    BlossomDownload {
        id: String,
        hash: String,
    },
    BlossomExists {
        id: String,
        hash: String,
    },

    // Stats operations
    GetStorageStats {
        id: String,
    },
    GetSocialGraphSize {
        id: String,
    },

    // Storage management
    SetStorageMaxBytes {
        id: String,
        #[serde(rename = "maxBytes")]
        max_bytes: u64,
    },
    RunEviction {
        id: String,
    },

    // Relay statistics
    GetRelayStats {
        id: String,
    },

    // Blossom server configuration
    SetBlossomServers {
        id: String,
        #[serde(rename = "readServers")]
        read_servers: Vec<String>,
        #[serde(rename = "writeServers")]
        write_servers: Vec<String>,
    },
    GetBlossomServers {
        id: String,
    },

    // Tree push to Blossom
    PushToBlossom {
        id: String,
        cid: WorkerCid,
        #[serde(rename = "treeName")]
        tree_name: Option<String>,
    },

    // Republish tree event to Nostr
    RepublishTree {
        id: String,
        pubkey: String,
        #[serde(rename = "treeName")]
        tree_name: String,
    },

    // Batch republish all trees for a pubkey
    RepublishTrees {
        id: String,
        #[serde(rename = "pubkeyPrefix")]
        pubkey_prefix: Option<String>,
    },

    // Streaming file read
    ReadFileStream {
        id: String,
        cid: WorkerCid,
    },

    // WebRTC operations
    GetPeerStats {
        id: String,
    },
    SendHello {
        id: String,
        roots: Option<Vec<String>>,
    },
    SetWebRTCPools {
        id: String,
        #[serde(rename = "followsMax")]
        follows_max: usize,
        #[serde(rename = "followsSatisfied")]
        follows_satisfied: usize,
        #[serde(rename = "otherMax")]
        other_max: usize,
        #[serde(rename = "otherSatisfied")]
        other_satisfied: usize,
    },
}

/// Worker response messages to frontend
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkerResponse {
    // Lifecycle
    Ready { id: String },
    Pong { id: String },

    // Results
    Error { id: String, error: String },
    Result {
        id: String,
        data: Option<String>,
    }, // base64 data
    Bool { id: String, value: bool },
    Cid { id: String, cid: Option<WorkerCid> },
    DirListing {
        id: String,
        entries: Option<Vec<WorkerDirEntry>>,
    },
    Void { id: String },

    // Nostr events (Phase 3)
    Event {
        #[serde(rename = "subId")]
        sub_id: String,
        event: serde_json::Value,
    },
    Eose {
        #[serde(rename = "subId")]
        sub_id: String,
    },

    // Relay info
    Relays {
        id: String,
        relays: Vec<String>,
    },

    // Social graph (Phase 4)
    Follows {
        id: String,
        pubkeys: Vec<String>,
    },
    WotDistance {
        id: String,
        distance: Option<usize>,
    },
    UsersWithDistance {
        id: String,
        users: Vec<(String, usize)>,
    },

    // Stats
    StorageStats {
        id: String,
        items: u64,
        bytes: u64,
        #[serde(rename = "pinnedItems")]
        pinned_items: u64,
        #[serde(rename = "pinnedBytes")]
        pinned_bytes: u64,
        #[serde(rename = "maxBytes")]
        max_bytes: u64,
    },
    SocialGraphSize {
        id: String,
        size: usize,
    },

    // Eviction result
    EvictionResult {
        id: String,
        #[serde(rename = "bytesFreed")]
        bytes_freed: u64,
    },

    // Relay statistics
    RelayStats {
        id: String,
        relays: Vec<RelayStatEntry>,
    },

    // Blossom servers
    BlossomServers {
        id: String,
        #[serde(rename = "readServers")]
        read_servers: Vec<String>,
        #[serde(rename = "writeServers")]
        write_servers: Vec<String>,
    },

    // Push to Blossom result
    PushResult {
        id: String,
        pushed: u32,
        skipped: u32,
        failed: u32,
        errors: Option<Vec<String>>,
    },

    // Push progress
    PushProgress {
        #[serde(rename = "treeName")]
        tree_name: String,
        current: u32,
        total: u32,
    },

    // Streaming file chunk
    StreamChunk {
        id: String,
        data: Option<String>, // base64
        done: bool,
    },

    // Social graph version update
    SocialGraphVersion {
        version: u64,
    },

    // Batch republish result
    RepublishResult {
        id: String,
        count: u32,
        #[serde(rename = "encryptionErrors")]
        encryption_errors: Option<Vec<String>>,
    },

    // WebRTC peer stats
    PeerStats {
        id: String,
        peers: Vec<PeerStatEntry>,
    },
}

/// WebRTC peer statistics entry
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerStatEntry {
    pub peer_id: String,
    pub connected: bool,
    pub pool: String,
}

/// Relay connection statistics entry
#[derive(Debug, Clone, Serialize)]
pub struct RelayStatEntry {
    pub url: String,
    pub connected: bool,
    pub connecting: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_request_deserialize_init() {
        let json = r#"{"type":"init","id":"test-1"}"#;
        let req: WorkerRequest = serde_json::from_str(json).unwrap();
        match req {
            WorkerRequest::Init { id } => assert_eq!(id, "test-1"),
            _ => panic!("Expected Init"),
        }
    }

    #[test]
    fn test_worker_request_deserialize_get() {
        let json = r#"{"type":"get","id":"test-2","hash":"abcd1234"}"#;
        let req: WorkerRequest = serde_json::from_str(json).unwrap();
        match req {
            WorkerRequest::Get { id, hash } => {
                assert_eq!(id, "test-2");
                assert_eq!(hash, "abcd1234");
            }
            _ => panic!("Expected Get"),
        }
    }

    #[test]
    fn test_worker_request_deserialize_read_file() {
        let json = r#"{"type":"readFile","id":"test-3","cid":{"hash":"abc123","key":"def456"}}"#;
        let req: WorkerRequest = serde_json::from_str(json).unwrap();
        match req {
            WorkerRequest::ReadFile { id, cid } => {
                assert_eq!(id, "test-3");
                assert_eq!(cid.hash, "abc123");
                assert_eq!(cid.key, Some("def456".to_string()));
            }
            _ => panic!("Expected ReadFile"),
        }
    }

    #[test]
    fn test_worker_request_deserialize_write_file() {
        let json = r#"{"type":"writeFile","id":"test-4","parentCid":null,"path":"test.txt","data":"SGVsbG8="}"#;
        let req: WorkerRequest = serde_json::from_str(json).unwrap();
        match req {
            WorkerRequest::WriteFile {
                id,
                parent_cid,
                path,
                data,
            } => {
                assert_eq!(id, "test-4");
                assert!(parent_cid.is_none());
                assert_eq!(path, "test.txt");
                assert_eq!(data, "SGVsbG8=");
            }
            _ => panic!("Expected WriteFile"),
        }
    }

    #[test]
    fn test_worker_response_serialize_ready() {
        let resp = WorkerResponse::Ready {
            id: "test-1".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"ready""#));
        assert!(json.contains(r#""id":"test-1""#));
    }

    #[test]
    fn test_worker_response_serialize_cid() {
        let resp = WorkerResponse::Cid {
            id: "test-2".to_string(),
            cid: Some(WorkerCid {
                hash: "abc123".to_string(),
                key: Some("def456".to_string()),
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"cid""#));
        assert!(json.contains(r#""hash":"abc123""#));
        assert!(json.contains(r#""key":"def456""#));
    }

    #[test]
    fn test_worker_response_serialize_dir_listing() {
        let resp = WorkerResponse::DirListing {
            id: "test-3".to_string(),
            entries: Some(vec![WorkerDirEntry {
                name: "file.txt".to_string(),
                hash: "abc123".to_string(),
                size: 1024,
                link_type: 0,
                key: None,
            }]),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"dirListing""#));
        assert!(json.contains(r#""name":"file.txt""#));
        assert!(json.contains(r#""linkType":0"#));
    }
}
