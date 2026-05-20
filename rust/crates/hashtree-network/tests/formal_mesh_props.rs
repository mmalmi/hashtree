use std::time::Duration;
use tokio::time::Instant;

use hashtree_network::{should_forward_htl, PeerHTLConfig, PeerSelector, MAX_HTL};

#[test]
fn test_htl_monotonicity_and_forwarding_bounds() {
    let configs = [
        PeerHTLConfig::from_samples(1.0, 1.0),
        PeerHTLConfig::from_samples(1.0, 0.0),
        PeerHTLConfig::from_samples(0.0, 1.0),
        PeerHTLConfig::from_samples(0.0, 0.0),
    ];

    for cfg in configs {
        for htl in 0..=MAX_HTL {
            let next = cfg.decrement(htl);
            assert!(next <= htl, "htl increased: {htl} -> {next}");
            assert_eq!(should_forward_htl(htl), htl > 0);

            if htl == 0 {
                assert_eq!(next, 0);
            } else if (2..MAX_HTL).contains(&htl) {
                assert_eq!(next, htl - 1);
            } else if htl == MAX_HTL {
                assert!(next == MAX_HTL || next == MAX_HTL - 1);
            } else if htl == 1 {
                assert!(next == 1 || next == 0);
            }
        }
    }
}

#[test]
fn test_peer_selector_avoids_backed_off_when_alternatives_exist() {
    let mut selector = PeerSelector::new();
    selector.add_peer("peer1");
    selector.add_peer("peer2");
    selector.add_peer("peer3");

    selector.record_timeout("peer1");
    selector.record_timeout("peer2");

    let selected = selector.select_peers();
    assert_eq!(selected, vec!["peer3".to_string()]);
}

#[test]
fn test_peer_selector_returns_all_when_all_backed_off() {
    let mut selector = PeerSelector::new();
    selector.add_peer("peer1");
    selector.add_peer("peer2");

    selector.record_timeout("peer1");
    selector.record_timeout("peer2");

    let selected = selector.select_peers();
    assert_eq!(selected.len(), 2);
    assert!(selected.contains(&"peer1".to_string()));
    assert!(selected.contains(&"peer2".to_string()));
}

#[test]
fn test_peer_selector_fairness_filters_over_selected_peer() {
    let mut selector = PeerSelector::new();
    for i in 1..=5 {
        selector.add_peer(format!("peer{i}"));
    }

    let now = Instant::now();
    {
        let dominant = selector.get_stats_mut("peer1").unwrap();
        dominant.connected_at = now - Duration::from_secs(30);
        dominant.requests_sent = 900;
        dominant.successes = 900;
    }

    for i in 2..=5 {
        let peer_id = format!("peer{i}");
        let stats = selector.get_stats_mut(&peer_id).unwrap();
        stats.connected_at = now - Duration::from_secs(30);
        stats.requests_sent = 10;
        stats.successes = 10;
    }

    let selected = selector.select_peers();
    assert!(!selected.is_empty());
    assert!(
        !selected.contains(&"peer1".to_string()),
        "dominant peer should be filtered by fairness"
    );
}
