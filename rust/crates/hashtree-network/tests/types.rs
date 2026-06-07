//! Tests for mesh transport types

use hashtree_network::{
    bytes_to_hash, create_fragment_response, create_request, create_response, encode_request,
    encode_response, is_fragmented, parse_message, should_forward, should_forward_htl,
    validate_mesh_frame, DataMessage, MeshNostrFrame, MeshStats, MeshStoreConfig, PeerHTLConfig,
    PeerId, PeerState, SelectionStrategy, SignalingMessage, TimedSeenSet, BLOB_REQUEST_POLICY,
    MAX_HTL, MESH_DEFAULT_HTL, MESH_EVENT_POLICY, MESH_MAX_HTL, MESH_PROTOCOL,
    MESH_PROTOCOL_VERSION, MSG_TYPE_REQUEST, MSG_TYPE_RESPONSE, NOSTR_KIND_HASHTREE,
};
use nostr_sdk::nostr::{EventBuilder, Keys, Kind};

#[test]
fn test_peer_id_creation() {
    let peer_id = PeerId::new("abc123".to_string());
    assert_eq!(peer_id.pubkey, "abc123");
}

#[test]
fn test_peer_id_to_string() {
    let peer_id = PeerId::new("abc123".to_string());
    assert_eq!(peer_id.to_peer_string(), "abc123");
    assert_eq!(peer_id.to_string(), "abc123");
}

#[test]
fn test_peer_id_from_string() {
    let peer_id = PeerId::from_string("abc123").unwrap();
    assert_eq!(peer_id.pubkey, "abc123");
}

#[test]
fn test_peer_id_from_string_invalid() {
    let peer_id = PeerId::from_string("invalid").unwrap();
    assert_eq!(peer_id.pubkey, "invalid");
    assert!(PeerId::from_string("").is_none());
    assert!(PeerId::from_string("abc123:legacy").is_none());
}

#[test]
fn test_peer_id_short() {
    let peer_id = PeerId::new("abc123def456ghijklmnop".to_string());
    assert_eq!(peer_id.short(), "abc123de");
}

#[test]
fn test_signaling_message_hello_serialize() {
    let msg = SignalingMessage::Hello {
        peer_id: "test123".to_string(),
        roots: vec!["abc".to_string(), "def".to_string()],
        hash_get: true,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"hello\""));
    assert!(json.contains("\"peerId\":\"test123\""));
}

#[test]
fn test_signaling_message_offer_serialize() {
    let msg = SignalingMessage::Offer {
        peer_id: "peer1".to_string(),
        target_peer_id: "peer2".to_string(),
        sdp: "v=0\r\n...".to_string(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"offer\""));
    assert!(json.contains("\"targetPeerId\":\"peer2\""));
}

#[test]
fn test_signaling_message_roundtrip() {
    let original = SignalingMessage::Answer {
        peer_id: "peer1".to_string(),
        target_peer_id: "peer2".to_string(),
        sdp: "v=0\r\ntest sdp".to_string(),
    };
    let json = serde_json::to_string(&original).unwrap();
    let parsed: SignalingMessage = serde_json::from_str(&json).unwrap();

    match parsed {
        SignalingMessage::Answer {
            peer_id,
            target_peer_id,
            sdp,
        } => {
            assert_eq!(peer_id, "peer1");
            assert_eq!(target_peer_id, "peer2");
            assert_eq!(sdp, "v=0\r\ntest sdp");
        }
        _ => panic!("Wrong message type"),
    }
}

// Binary MessagePack protocol tests

#[test]
fn test_encode_decode_request() {
    let hash = [0xab; 32];
    let req = create_request(&hash, 10);
    let encoded = encode_request(&req);

    // First byte should be request type marker
    assert_eq!(encoded[0], MSG_TYPE_REQUEST);

    let parsed = parse_message(&encoded).unwrap();
    match parsed {
        DataMessage::Request(r) => {
            assert_eq!(r.h, hash.to_vec());
            assert_eq!(r.htl, 10);
        }
        _ => panic!("Expected request"),
    }
}

#[test]
fn test_encode_decode_response() {
    let hash = [0xcd; 32];
    let data = vec![1, 2, 3, 4, 5];
    let res = create_response(&hash, data.clone());
    let encoded = encode_response(&res);

    // First byte should be response type marker
    assert_eq!(encoded[0], MSG_TYPE_RESPONSE);

    let parsed = parse_message(&encoded).unwrap();
    match parsed {
        DataMessage::Response(r) => {
            assert_eq!(r.h, hash.to_vec());
            assert_eq!(r.d, data);
            assert!(!is_fragmented(&r));
        }
        _ => panic!("Expected response"),
    }
}

#[test]
fn test_encode_decode_fragment_response() {
    let hash = [0xef; 32];
    let data = vec![10, 20, 30];
    let res = create_fragment_response(&hash, data.clone(), 2, 5);
    let encoded = encode_response(&res);

    let parsed = parse_message(&encoded).unwrap();
    match parsed {
        DataMessage::Response(r) => {
            assert_eq!(r.h, hash.to_vec());
            assert_eq!(r.d, data);
            assert!(is_fragmented(&r));
            assert_eq!(r.i, Some(2));
            assert_eq!(r.n, Some(5));
        }
        _ => panic!("Expected response"),
    }
}

#[test]
fn test_bytes_to_hash() {
    let valid = vec![0x12u8; 32];
    assert!(bytes_to_hash(&valid).is_some());

    let too_short = vec![0x12u8; 16];
    assert!(bytes_to_hash(&too_short).is_none());

    let too_long = vec![0x12u8; 64];
    assert!(bytes_to_hash(&too_long).is_none());
}

#[test]
fn test_parse_invalid_message() {
    // Too short
    assert!(parse_message(&[]).is_none());
    assert!(parse_message(&[0x00]).is_none());

    // Invalid type
    assert!(parse_message(&[0xFF, 0x00]).is_none());
}

#[test]
fn test_mesh_store_config_default() {
    let config = MeshStoreConfig::default();
    assert_eq!(config.pools.follows.satisfied_connections, 8);
    assert_eq!(config.pools.follows.max_connections, 16);
    assert_eq!(config.pools.other.satisfied_connections, 8);
    assert_eq!(config.pools.other.max_connections, 16);
    assert_eq!(config.request_timeout_ms, 10000);
    assert!(!config.debug);
    assert!(config.classifier_tx.is_none());
    assert_eq!(
        config.request_selection_strategy,
        SelectionStrategy::Weighted
    );
    assert!(config.request_fairness_enabled);
    assert_eq!(config.request_dispatch.initial_fanout, 2);
    assert_eq!(config.request_dispatch.hedge_fanout, 1);
    assert_eq!(config.request_dispatch.max_fanout, 8);
    assert_eq!(config.request_dispatch.hedge_interval_ms, 120);
}

#[test]
fn test_peer_state_equality() {
    assert_eq!(PeerState::New, PeerState::New);
    assert_eq!(PeerState::Connected, PeerState::Connected);
    assert_ne!(PeerState::New, PeerState::Connected);
}

#[test]
fn test_mesh_stats_default() {
    let stats = MeshStats::default();
    assert_eq!(stats.connected_peers, 0);
    assert_eq!(stats.pending_requests, 0);
    assert_eq!(stats.bytes_sent, 0);
    assert_eq!(stats.bytes_received, 0);
}

// HTL (Hops To Live) tests

#[test]
fn test_max_htl_constant() {
    assert_eq!(MAX_HTL, 10);
}

#[test]
fn test_should_forward() {
    // HTL > 0 should allow forwarding
    assert!(should_forward(10));
    assert!(should_forward(5));
    assert!(should_forward(1));
    assert!(should_forward_htl(1));
    // HTL = 0 should not forward
    assert!(!should_forward(0));
    assert!(!should_forward_htl(0));
}

#[test]
fn test_htl_config_decrement_middle_values() {
    // Middle values (2-9) always decrement
    let config = PeerHTLConfig::from_samples(1.0, 1.0);
    assert_eq!(config.decrement(5), 4);
    assert_eq!(config.decrement(2), 1);
    assert_eq!(config.decrement(9), 8);
}

#[test]
fn test_htl_config_decrement_at_max() {
    // At MAX_HTL, decrement depends on config
    let config_dec = PeerHTLConfig::from_samples(0.0, 1.0);
    assert_eq!(config_dec.decrement(MAX_HTL), MAX_HTL - 1);

    let config_no_dec = PeerHTLConfig::from_samples(1.0, 1.0);
    assert_eq!(config_no_dec.decrement(MAX_HTL), MAX_HTL);
}

#[test]
fn test_htl_config_decrement_at_min() {
    // At HTL=1, decrement depends on config
    let config_dec = PeerHTLConfig::from_samples(1.0, 0.0);
    assert_eq!(config_dec.decrement(1), 0);

    let config_no_dec = PeerHTLConfig::from_samples(1.0, 1.0);
    assert_eq!(config_no_dec.decrement(1), 1);
}

#[test]
fn test_htl_config_decrement_zero() {
    // HTL=0 stays at 0
    let config = PeerHTLConfig::from_samples(0.0, 0.0);
    assert_eq!(config.decrement(0), 0);
}

#[test]
fn test_htl_config_random_creates_valid_config() {
    // Just verify random() doesn't panic and creates a valid config
    for _ in 0..10 {
        let config = PeerHTLConfig::random();
        // Test that it works with various HTL values
        let _ = config.decrement(MAX_HTL);
        let _ = config.decrement(5);
        let _ = config.decrement(1);
        let _ = config.decrement(0);
    }
}

#[test]
fn test_htl_policy_profiles_differ_at_max() {
    let dec_cfg = PeerHTLConfig::from_samples(0.6, 0.6);
    let blob_next =
        dec_cfg.decrement_with_policy(BLOB_REQUEST_POLICY.max_htl, &BLOB_REQUEST_POLICY);
    let mesh_next = dec_cfg.decrement_with_policy(BLOB_REQUEST_POLICY.max_htl, &MESH_EVENT_POLICY);
    assert_eq!(blob_next, BLOB_REQUEST_POLICY.max_htl);
    assert_eq!(mesh_next, MESH_EVENT_POLICY.max_htl - 1);
}

#[test]
fn test_timed_seen_set_dedupe_and_ttl() {
    use std::thread;
    use std::time::Duration;

    let mut seen = TimedSeenSet::new(2, Duration::from_millis(20));
    assert!(seen.insert_if_new("a".to_string()));
    assert!(!seen.insert_if_new("a".to_string()));
    assert!(seen.insert_if_new("b".to_string()));
    assert_eq!(seen.len(), 2);

    thread::sleep(Duration::from_millis(25));
    assert!(seen.insert_if_new("a".to_string()));
    assert_eq!(seen.len(), 1);
}

#[test]
fn test_mesh_frame_roundtrip_and_validation() {
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::Custom(NOSTR_KIND_HASHTREE), "")
        .sign_with_keys(&keys)
        .unwrap();
    let frame = MeshNostrFrame::new_event_with_id(event, "peer-a", "frame-1", MESH_DEFAULT_HTL);
    assert!(validate_mesh_frame(&frame).is_ok());

    let encoded = serde_json::to_string(&frame).unwrap();
    let decoded: MeshNostrFrame = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.protocol, MESH_PROTOCOL);
    assert_eq!(decoded.version, MESH_PROTOCOL_VERSION);
    assert_eq!(decoded.frame_id, "frame-1");
    assert!(validate_mesh_frame(&decoded).is_ok());
}

#[test]
fn test_mesh_frame_validation_rejects_invalid_protocol_and_htl() {
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::Custom(NOSTR_KIND_HASHTREE), "")
        .sign_with_keys(&keys)
        .unwrap();
    let mut frame = MeshNostrFrame::new_event_with_id(event, "peer-a", "frame-1", MESH_DEFAULT_HTL);

    frame.protocol = "invalid".to_string();
    assert_eq!(validate_mesh_frame(&frame), Err("invalid protocol"));

    frame.protocol = MESH_PROTOCOL.to_string();
    frame.sender_peer_id = "peer-a:legacy".to_string();
    assert_eq!(validate_mesh_frame(&frame), Err("invalid sender peer id"));

    frame.sender_peer_id = "peer-a".to_string();
    frame.htl = 0;
    assert_eq!(validate_mesh_frame(&frame), Err("invalid htl"));

    frame.htl = MESH_MAX_HTL + 1;
    assert_eq!(validate_mesh_frame(&frame), Err("invalid htl"));
}
