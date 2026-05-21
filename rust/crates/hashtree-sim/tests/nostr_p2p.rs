#[cfg(feature = "nostr")]
mod nostr_p2p {
    use hashtree_network::{SignalingMessage, NOSTR_KIND_HASHTREE};
    use hashtree_sim::nostr_mesh::NostrMesh;
    use nostr::{Alphabet, Filter, Kind, SingleLetterTag};

    #[tokio::test]
    async fn nostr_publish_and_req_over_one_hop_resolves_tree() {
        let mut mesh = NostrMesh::new();
        let a_pub = mesh.add_node("a");
        mesh.add_node("b");
        mesh.add_node("c");
        mesh.link("a", "b");
        mesh.link("b", "c");

        let tree_name = "photos";
        let hash_hex = "1234".repeat(16);

        mesh.publish_hashtree_root("a", tree_name, &hash_hex, 2)
            .expect("publish");

        let filter = Filter::new()
            .authors(vec![a_pub])
            .kinds(vec![Kind::Custom(30078)])
            .custom_tag(SingleLetterTag::lowercase(Alphabet::D), [tree_name])
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), ["hashtree"])
            .limit(10);

        mesh.request("c", "sub-1", vec![filter], 2);

        mesh.drain(200);

        let resolved = mesh
            .resolve_hashtree_hash("c", tree_name)
            .expect("resolved hash");
        assert_eq!(resolved, hash_hex);
    }

    #[tokio::test]
    async fn mesh_signaling_over_nostr_mesh_reaches_target() {
        let mut mesh = NostrMesh::new();
        let a_pub = mesh.add_node("a");
        mesh.add_node("b");
        let c_pub = mesh.add_node("c");
        mesh.link("a", "b");
        mesh.link("b", "c");

        let offer = SignalingMessage::Offer {
            peer_id: format!("{}:a", a_pub.to_hex()),
            target_peer_id: format!("{}:c", c_pub.to_hex()),
            sdp: "fake-sdp".to_string(),
        };

        mesh.send_signaling("a", &c_pub, &offer, 2)
            .expect("send signaling");

        mesh.drain(200);

        let received = mesh.received_signaling("c");
        assert!(received.iter().any(|msg| match msg {
            SignalingMessage::Offer { sdp, .. } => sdp == "fake-sdp",
            _ => false,
        }));

        let count = mesh
            .received_events("c")
            .iter()
            .filter(|event| event.kind == Kind::Custom(NOSTR_KIND_HASHTREE))
            .count();
        assert!(count >= 1);
    }

    #[tokio::test]
    async fn mesh_signaling_frame_dedupes_replayed_frame_id() {
        let mut mesh = NostrMesh::new();
        let a_pub = mesh.add_node("a");
        mesh.add_node("b");
        let c_pub = mesh.add_node("c");
        mesh.link("a", "b");
        mesh.link("b", "c");

        let offer = SignalingMessage::Offer {
            peer_id: format!("{}:a", a_pub.to_hex()),
            target_peer_id: format!("{}:c", c_pub.to_hex()),
            sdp: "fake-sdp".to_string(),
        };

        mesh.send_signaling_with_frame_id("a", &c_pub, &offer, "frame-replay", 2)
            .expect("send signaling 1");
        mesh.send_signaling_with_frame_id("a", &c_pub, &offer, "frame-replay", 2)
            .expect("send signaling 2");

        mesh.drain(200);

        let received = mesh.received_signaling("c");
        let offer_count = received
            .iter()
            .filter(|msg| match msg {
                SignalingMessage::Offer { sdp, .. } => sdp == "fake-sdp",
                _ => false,
            })
            .count();
        assert_eq!(offer_count, 1, "replayed frame id must not amplify");
    }
}
