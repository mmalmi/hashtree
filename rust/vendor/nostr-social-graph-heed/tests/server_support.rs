use nostr_social_graph::SocialGraph;
use nostr_social_graph_heed::HeedSocialGraph;
use tempfile::TempDir;

const ADAM: &str = "020f2d21ae09bf35fcdfb65decf1478b846f5f728ab30c5eaabcd6d081a81c3e";
const FIATJAF: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
const SNOWDEN: &str = "84dee6e676e5bb67b4ad4e042cf70cbd8681155db535942fcc6a0533858a7240";

#[test]
fn export_state_round_trips_into_memory_graph() {
    let tempdir = TempDir::new().unwrap();
    let mut store = HeedSocialGraph::open(tempdir.path(), ADAM).unwrap();
    store
        .handle_event(&event(ADAM, 3, 1_000, vec![FIATJAF]), true, 1.0)
        .unwrap();
    store
        .handle_event(&event(FIATJAF, 3, 1_100, vec![SNOWDEN]), true, 1.0)
        .unwrap();

    let state = store.export_state().unwrap();
    let graph = SocialGraph::from_state(state).unwrap();

    assert!(graph.is_following(ADAM, FIATJAF));
    assert!(graph.is_following(FIATJAF, SNOWDEN));
    assert_eq!(graph.get_follow_distance(ADAM), 0);
    assert_eq!(graph.get_follow_distance(FIATJAF), 1);
    assert_eq!(graph.get_follow_distance(SNOWDEN), 2);
}

#[test]
fn export_state_from_path_round_trips_without_write_access() {
    let tempdir = TempDir::new().unwrap();
    let mut store = HeedSocialGraph::open(tempdir.path(), ADAM).unwrap();
    store
        .handle_event(&event(ADAM, 3, 1_000, vec![FIATJAF]), true, 1.0)
        .unwrap();
    store
        .handle_event(&event(FIATJAF, 3, 1_100, vec![SNOWDEN]), true, 1.0)
        .unwrap();
    drop(store);

    let readonly_dir = TempDir::new().unwrap();
    std::fs::copy(
        tempdir.path().join("data.mdb"),
        readonly_dir.path().join("data.mdb"),
    )
    .unwrap();
    std::fs::copy(
        tempdir.path().join("lock.mdb"),
        readonly_dir.path().join("lock.mdb"),
    )
    .unwrap();

    let state = HeedSocialGraph::export_state_from_path(readonly_dir.path()).unwrap();
    let graph = SocialGraph::from_state(state).unwrap();

    assert!(graph.is_following(ADAM, FIATJAF));
    assert!(graph.is_following(FIATJAF, SNOWDEN));
    assert_eq!(graph.get_follow_distance(ADAM), 0);
    assert_eq!(graph.get_follow_distance(FIATJAF), 1);
    assert_eq!(graph.get_follow_distance(SNOWDEN), 2);
}

#[test]
fn replace_state_persists_a_full_snapshot() {
    let tempdir = TempDir::new().unwrap();
    let mut graph = SocialGraph::new(ADAM);
    graph.handle_event(&event(ADAM, 3, 1_000, vec![FIATJAF]), true, 1.0);
    graph.handle_event(&event(FIATJAF, 3, 1_100, vec![SNOWDEN]), true, 1.0);

    let mut store = HeedSocialGraph::open(tempdir.path(), ADAM).unwrap();
    store.replace_state(&graph.export_state()).unwrap();
    drop(store);

    let reopened = HeedSocialGraph::open(tempdir.path(), SNOWDEN).unwrap();
    assert_eq!(reopened.get_root().unwrap(), ADAM);
    assert!(reopened.is_following(ADAM, FIATJAF).unwrap());
    assert!(reopened.is_following(FIATJAF, SNOWDEN).unwrap());
    assert_eq!(reopened.get_follow_distance(ADAM).unwrap(), 0);
    assert_eq!(reopened.get_follow_distance(FIATJAF).unwrap(), 1);
    assert_eq!(reopened.get_follow_distance(SNOWDEN).unwrap(), 2);
}

fn event(
    pubkey: &str,
    kind: u32,
    created_at: u64,
    tagged: Vec<&str>,
) -> nostr_social_graph::NostrEvent {
    nostr_social_graph::NostrEvent {
        created_at,
        content: String::new(),
        tags: tagged
            .into_iter()
            .map(|pk| vec!["p".to_string(), pk.to_string()])
            .collect(),
        kind,
        pubkey: pubkey.to_string(),
        id: format!("{pubkey}:{kind}:{created_at}"),
        sig: "00".repeat(64),
    }
}
