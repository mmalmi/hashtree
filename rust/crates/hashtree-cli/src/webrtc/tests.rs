//! WebRTC module tests

use super::types::*;

#[test]
fn test_peer_id_display() {
    let peer_id = PeerId::new("abc123def456".to_string());
    assert_eq!(peer_id.to_string(), "abc123def456");
}

#[test]
fn test_peer_id_short() {
    let peer_id = PeerId::new("abc123def456ghijklmnop".to_string());
    assert_eq!(peer_id.short(), "abc123de");
}

#[test]
fn test_peer_id_from_string() {
    let peer_id = PeerId::from_string("abc123").unwrap();
    assert_eq!(peer_id.pubkey, "abc123");
}

#[test]
fn test_peer_id_from_string_invalid() {
    let peer_id = PeerId::from_string("plain-peer").unwrap();
    assert_eq!(peer_id.pubkey, "plain-peer");
    assert!(PeerId::from_string("a:b:c").is_none());
    assert!(PeerId::from_string("").is_none());
}

#[test]
fn test_signaling_message_hello() {
    let msg = SignalingMessage::Hello {
        peer_id: "my-pubkey".to_string(),
        roots: Vec::new(),
        hash_get: true,
    };
    assert_eq!(msg.msg_type(), "hello");
    assert_eq!(msg.peer_id(), "my-pubkey");
    assert!(msg.target_peer_id().is_none());
}

#[test]
fn test_signaling_message_offer() {
    let msg = SignalingMessage::Offer {
        peer_id: "peer-id".to_string(),
        target_peer_id: "recipient".to_string(),
        sdp: "test".to_string(),
    };
    assert_eq!(msg.msg_type(), "offer");
    assert_eq!(msg.target_peer_id(), Some("recipient"));
    assert_eq!(msg.peer_id(), "peer-id");
}

#[test]
fn test_webrtc_config_default() {
    let config = WebRTCConfig::default();
    assert!(!config.relays.is_empty());
    assert!(config.max_outbound > 0);
    assert!(config.max_inbound > 0);
    assert!(!config.stun_servers.is_empty());
    assert!(!config.multicast.enabled);
    assert_eq!(config.multicast.max_peers, 0);
    assert!(!config.wifi_aware.enabled);
    assert_eq!(config.wifi_aware.max_peers, 0);
    assert!(!config.bluetooth.enabled);
    assert_eq!(config.bluetooth.max_peers, 0);
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
fn test_peer_direction_display() {
    assert_eq!(PeerDirection::Inbound.to_string(), "inbound");
    assert_eq!(PeerDirection::Outbound.to_string(), "outbound");
}

#[test]
fn test_peer_signal_path_wifi_aware_round_trip() {
    assert_eq!(
        super::PeerSignalPath::from_source_name("wifi-aware"),
        super::PeerSignalPath::WifiAware
    );
    assert_eq!(super::PeerSignalPath::WifiAware.to_string(), "wifi-aware");
}

// Wire format tests for hashtree-ts interop
#[test]
fn test_wire_format_request_encode_decode() {
    let req = DataRequest {
        h: vec![0xab; 32],
        htl: 10,
        q: Some(42),
    };
    let encoded = encode_request(&req).unwrap();

    // First byte should be request type
    assert_eq!(encoded[0], MSG_TYPE_REQUEST);

    // Should round-trip
    let parsed = parse_message(&encoded).unwrap();
    match parsed {
        DataMessage::Request(r) => {
            assert_eq!(r.h, vec![0xab; 32]);
            assert_eq!(r.htl, 10);
            assert_eq!(r.q, Some(42));
        }
        _ => panic!("Expected request"),
    }
}

#[test]
fn test_wire_format_response_encode_decode() {
    let res = DataResponse {
        h: vec![0xcd; 32],
        d: vec![1, 2, 3, 4, 5],
        i: None,
        n: None,
    };
    let encoded = encode_response(&res).unwrap();

    // First byte should be response type
    assert_eq!(encoded[0], MSG_TYPE_RESPONSE);

    // Should round-trip
    let parsed = parse_message(&encoded).unwrap();
    match parsed {
        DataMessage::Response(r) => {
            assert_eq!(r.h, vec![0xcd; 32]);
            assert_eq!(r.d, vec![1, 2, 3, 4, 5]);
        }
        _ => panic!("Expected response"),
    }
}

#[test]
fn test_wire_format_constants() {
    // These must match hashtree-ts constants
    assert_eq!(MSG_TYPE_REQUEST, 0x00);
    assert_eq!(MSG_TYPE_RESPONSE, 0x01);
    assert_eq!(MSG_TYPE_QUOTE_REQUEST, 0x02);
    assert_eq!(MSG_TYPE_QUOTE_RESPONSE, 0x03);
    assert_eq!(MSG_TYPE_PAYMENT, 0x04);
    assert_eq!(MSG_TYPE_PAYMENT_ACK, 0x05);
}

#[test]
fn test_wire_format_quote_request_encode_decode() {
    let req = DataQuoteRequest {
        h: vec![0xaa; 32],
        p: 3,
        t: 1_500,
        m: Some("https://mint-a.example".to_string()),
    };
    let encoded = encode_quote_request(&req).unwrap();

    assert_eq!(encoded[0], MSG_TYPE_QUOTE_REQUEST);

    let parsed = parse_message(&encoded).unwrap();
    match parsed {
        DataMessage::QuoteRequest(r) => {
            assert_eq!(r.h, vec![0xaa; 32]);
            assert_eq!(r.p, 3);
            assert_eq!(r.t, 1_500);
            assert_eq!(r.m.as_deref(), Some("https://mint-a.example"));
        }
        _ => panic!("Expected quote request"),
    }
}

#[test]
fn test_wire_format_quote_response_encode_decode() {
    let res = DataQuoteResponse {
        h: vec![0xbb; 32],
        a: true,
        q: Some(9),
        p: Some(3),
        t: Some(1_500),
        m: Some("https://mint-b.example".to_string()),
    };
    let encoded = encode_quote_response(&res).unwrap();

    assert_eq!(encoded[0], MSG_TYPE_QUOTE_RESPONSE);

    let parsed = parse_message(&encoded).unwrap();
    match parsed {
        DataMessage::QuoteResponse(r) => {
            assert_eq!(r.h, vec![0xbb; 32]);
            assert!(r.a);
            assert_eq!(r.q, Some(9));
            assert_eq!(r.p, Some(3));
            assert_eq!(r.t, Some(1_500));
            assert_eq!(r.m.as_deref(), Some("https://mint-b.example"));
        }
        _ => panic!("Expected quote response"),
    }
}

#[test]
fn test_wire_format_payment_encode_decode() {
    let req = DataPayment {
        h: vec![0xcc; 32],
        q: 9,
        c: 1,
        p: 3,
        m: Some("https://mint-b.example".to_string()),
        tok: "cashuBtoken".to_string(),
    };
    let encoded = encode_payment(&req).unwrap();

    assert_eq!(encoded[0], MSG_TYPE_PAYMENT);

    let parsed = parse_message(&encoded).unwrap();
    match parsed {
        DataMessage::Payment(r) => {
            assert_eq!(r.h, vec![0xcc; 32]);
            assert_eq!(r.q, 9);
            assert_eq!(r.c, 1);
            assert_eq!(r.p, 3);
            assert_eq!(r.m.as_deref(), Some("https://mint-b.example"));
            assert_eq!(r.tok, "cashuBtoken");
        }
        _ => panic!("Expected payment"),
    }
}

#[test]
fn test_wire_format_payment_ack_encode_decode() {
    let res = DataPaymentAck {
        h: vec![0xdd; 32],
        q: 9,
        c: 1,
        a: false,
        e: Some("invalid token".to_string()),
    };
    let encoded = encode_payment_ack(&res).unwrap();

    assert_eq!(encoded[0], MSG_TYPE_PAYMENT_ACK);

    let parsed = parse_message(&encoded).unwrap();
    match parsed {
        DataMessage::PaymentAck(r) => {
            assert_eq!(r.h, vec![0xdd; 32]);
            assert_eq!(r.q, 9);
            assert_eq!(r.c, 1);
            assert!(!r.a);
            assert_eq!(r.e.as_deref(), Some("invalid token"));
        }
        _ => panic!("Expected payment ack"),
    }
}

#[test]
fn test_wire_format_chunk_encode_decode() {
    let chunk = DataChunk {
        h: vec![0xee; 32],
        q: 9,
        c: 1,
        n: 3,
        p: 1,
        d: vec![1, 2, 3, 4],
    };
    let encoded = encode_chunk(&chunk).unwrap();

    assert_eq!(encoded[0], MSG_TYPE_CHUNK);

    let parsed = parse_message(&encoded).unwrap();
    match parsed {
        DataMessage::Chunk(res) => {
            assert_eq!(res.h, vec![0xee; 32]);
            assert_eq!(res.q, 9);
            assert_eq!(res.c, 1);
            assert_eq!(res.n, 3);
            assert_eq!(res.p, 1);
            assert_eq!(res.d, vec![1, 2, 3, 4]);
        }
        _ => panic!("Expected chunk"),
    }
}

#[test]
fn test_blob_policy_matches_legacy_defaults() {
    assert_eq!(BLOB_REQUEST_POLICY.max_htl, MAX_HTL);
    assert!((BLOB_REQUEST_POLICY.p_at_max - 0.5).abs() < f64::EPSILON);
    assert!((BLOB_REQUEST_POLICY.p_at_min - 0.25).abs() < f64::EPSILON);
}

#[test]
fn test_mesh_policy_is_probabilistic_and_tighter() {
    assert_eq!(MESH_EVENT_POLICY.mode, HtlMode::Probabilistic);
    assert_eq!(MESH_EVENT_POLICY.max_htl, 4);
    assert!((MESH_EVENT_POLICY.p_at_max - 0.75).abs() < f64::EPSILON);
    assert!((MESH_EVENT_POLICY.p_at_min - 0.5).abs() < f64::EPSILON);
}

#[test]
fn test_mesh_frame_validation_accepts_kind_25050_event() {
    let keys = nostr::Keys::generate();
    let event = nostr::EventBuilder::new(
        nostr::Kind::Ephemeral(WEBRTC_KIND as u16),
        "",
        [nostr::Tag::parse(&["l", HELLO_TAG]).unwrap()],
    )
    .to_event(&keys)
    .unwrap();

    let frame = MeshNostrFrame::new_event(event, "peer-a", MESH_EVENT_POLICY.max_htl);
    assert!(validate_mesh_frame(&frame).is_ok());
}

#[test]
fn test_mesh_frame_validation_rejects_non_webrtc_kind() {
    let keys = nostr::Keys::generate();
    let event = nostr::EventBuilder::new(nostr::Kind::TextNote, "nope", [])
        .to_event(&keys)
        .unwrap();
    let frame = MeshNostrFrame::new_event(event, "peer-a", MESH_EVENT_POLICY.max_htl);
    assert!(validate_mesh_frame(&frame).is_err());
}

#[test]
fn test_formal_htl_policy_monotonicity_and_bounds() {
    let sample_points = [0.0, 0.2, 0.49, 0.5, 0.75, 0.99];
    for policy in [BLOB_REQUEST_POLICY, MESH_EVENT_POLICY] {
        for htl in 0..=(policy.max_htl + 4) {
            let bounded = htl.min(policy.max_htl);
            for at_max_sample in sample_points {
                for at_min_sample in sample_points {
                    let cfg = PeerHTLConfig {
                        at_max_sample,
                        at_min_sample,
                    };
                    let next = decrement_htl_with_policy(htl, &policy, &cfg);
                    assert!(next <= bounded, "HTL must never increase");

                    if bounded == 0 {
                        assert_eq!(next, 0, "HTL 0 must stay at 0");
                        continue;
                    }

                    if bounded == policy.max_htl {
                        let expected = if at_max_sample < policy.p_at_max {
                            bounded - 1
                        } else {
                            bounded
                        };
                        assert_eq!(next, expected, "max HTL decrement rule mismatch");
                        continue;
                    }

                    if bounded == 1 {
                        let expected = if at_min_sample < policy.p_at_min {
                            0
                        } else {
                            1
                        };
                        assert_eq!(next, expected, "min HTL decrement rule mismatch");
                        continue;
                    }

                    assert_eq!(next, bounded - 1, "middle HTL values must decrement");
                }
            }
        }
    }
}

#[test]
fn test_formal_should_forward_htl_equivalence() {
    for htl in 0u8..=u8::MAX {
        assert_eq!(should_forward_htl(htl), htl > 0);
    }
}

#[test]
fn test_formal_mesh_frame_validation_rejects_protocol_version_and_htl_bounds() {
    let keys = nostr::Keys::generate();
    let event = nostr::EventBuilder::new(
        nostr::Kind::Ephemeral(WEBRTC_KIND as u16),
        "",
        [nostr::Tag::parse(&["l", HELLO_TAG]).unwrap()],
    )
    .to_event(&keys)
    .unwrap();

    let mut frame = MeshNostrFrame::new_event(event, "peer-a", MESH_EVENT_POLICY.max_htl);
    assert!(validate_mesh_frame(&frame).is_ok());

    frame.protocol = "invalid".to_string();
    assert_eq!(validate_mesh_frame(&frame), Err("invalid protocol"));
    frame.protocol = MESH_PROTOCOL.to_string();

    frame.version = MESH_PROTOCOL_VERSION + 1;
    assert_eq!(validate_mesh_frame(&frame), Err("invalid version"));
    frame.version = MESH_PROTOCOL_VERSION;

    frame.htl = 0;
    assert_eq!(validate_mesh_frame(&frame), Err("invalid htl"));
    frame.htl = MESH_MAX_HTL + 1;
    assert_eq!(validate_mesh_frame(&frame), Err("invalid htl"));
}

#[test]
fn test_formal_mesh_frame_validation_requires_non_empty_ids() {
    let keys = nostr::Keys::generate();
    let event = nostr::EventBuilder::new(
        nostr::Kind::Ephemeral(WEBRTC_KIND as u16),
        "",
        [nostr::Tag::parse(&["l", HELLO_TAG]).unwrap()],
    )
    .to_event(&keys)
    .unwrap();

    let mut frame = MeshNostrFrame::new_event(event, "peer-a", MESH_EVENT_POLICY.max_htl);

    frame.frame_id.clear();
    assert_eq!(validate_mesh_frame(&frame), Err("missing frame id"));

    frame.frame_id = "frame-1".to_string();
    frame.sender_peer_id = "peer-a:legacy".to_string();
    assert_eq!(validate_mesh_frame(&frame), Err("invalid sender peer id"));

    frame.sender_peer_id = "peer-a".to_string();
    frame.sender_peer_id.clear();
    assert_eq!(validate_mesh_frame(&frame), Err("missing sender peer id"));
}
