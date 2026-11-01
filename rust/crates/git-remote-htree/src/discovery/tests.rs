use super::*;
use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};

fn signed(keys: &Keys, kind: Kind, content: &str, tags: Vec<Tag>, time: u64) -> Event {
    EventBuilder::new(kind, content)
        .tags(tags)
        .custom_created_at(Timestamp::from(time))
        .sign_with_keys(keys)
        .unwrap()
}
fn follows(keys: &Keys, people: &[&Keys], time: u64) -> Event {
    signed(
        keys,
        Kind::ContactList,
        "",
        people
            .iter()
            .map(|k| Tag::public_key(k.public_key()))
            .collect(),
        time,
    )
}
fn root(keys: &Keys, name: &str, git: bool, time: u64) -> Event {
    let mut tags = vec![
        Tag::identifier(name),
        Tag::parse(["l", "hashtree"]).unwrap(),
        Tag::parse(["hash", &"11".repeat(32)]).unwrap(),
    ];
    if git {
        tags.push(Tag::parse(["l", "git"]).unwrap());
    }
    signed(keys, Kind::Custom(30064), "", tags, time)
}

#[test]
fn shared_query_preserves_owner_and_subpath() {
    assert_eq!(
        Query::parse("htree://alice/photos/2026/cat.jpg").unwrap(),
        Query {
            owner: Some("alice".into()),
            name: "photos".into(),
            path: Some("2026/cat.jpg".into())
        }
    );
    assert_eq!(Query::parse("iris-chat").unwrap().owner, None);
    for input in [
        "",
        "htree:///repo",
        "alice/",
        "alice/repo#k=secret",
        "foo\nbar",
    ] {
        assert!(Query::parse(input).is_err());
    }
}

#[test]
fn graph_ranks_matches_and_applies_latest_social_state() {
    let me = Keys::generate();
    let friend = Keys::generate();
    let second = Keys::generate();
    let stranger = Keys::generate();
    let mut state = state::State::new([me.public_key()]);
    for event in [
        follows(&me, &[&friend], 1),
        follows(&friend, &[&second], 1),
        root(&friend, "iris-chat", true, 1),
        root(&second, "iris-chat", true, 1),
        root(&stranger, "iris-chat", true, 1),
        root(&me, "iris-chat-backup", true, 1),
    ] {
        state.ingest(event);
    }
    let query = Query::parse("iris-chat").unwrap();
    let found = state.results(&query, true);
    assert_eq!(
        found
            .candidates
            .iter()
            .map(|c| c.distance)
            .collect::<Vec<_>>(),
        [1, 2, 0]
    );
    state.ingest(signed(
        &me,
        Kind::MuteList,
        "",
        vec![Tag::public_key(second.public_key())],
        2,
    ));
    assert_eq!(state.results(&query, true).candidates.len(), 2);
    state.ingest(follows(&me, &[], 3));
    state.ingest(follows(&me, &[&friend], 1));
    assert_eq!(state.results(&query, true).candidates.len(), 1);
}

#[test]
fn content_search_matches_profiles_and_keeps_paths_but_git_filters_trees() {
    let author = Keys::generate();
    let mut state = state::State::new([author.public_key()]);
    state.ingest(signed(
        &author,
        Kind::Metadata,
        r#"{"name":"Alice","display_name":"Alice Example"}"#,
        vec![],
        1,
    ));
    state.ingest(root(&author, "photos", false, 1));
    let query = Query::parse("alice/photos/cat.jpg").unwrap();
    let found = state.results(&query, false);
    assert_eq!(found.candidates.len(), 1);
    assert!(found.candidates[0].url.ends_with("/photos/cat.jpg"));
    assert!(state.results(&query, true).candidates.is_empty());
    state.ingest(signed(
        &author,
        Kind::Metadata,
        r#"{"name":"Bob"}"#,
        vec![],
        2,
    ));
    assert!(state.results(&query, false).candidates.is_empty());
}

#[test]
fn rejects_forged_events_and_never_resurrects_invalid_new_heads() {
    let author = Keys::generate();
    let mut state = state::State::new([author.public_key()]);
    let mut forged = root(&author, "repo", true, 1);
    forged.content.push('x');
    state.ingest(forged);
    let query = Query::parse("repo").unwrap();
    assert!(state.results(&query, true).candidates.is_empty());
    state.ingest(root(&author, "repo", true, 1));
    assert_eq!(state.results(&query, true).candidates.len(), 1);
    state.ingest(signed(
        &author,
        Kind::Custom(30064),
        "",
        vec![Tag::identifier("repo")],
        2,
    ));
    assert!(state.results(&query, true).candidates.is_empty());
}

#[test]
fn selection_requires_explicit_number_and_script_output_has_commands() {
    let results = SearchResults {
        candidates: vec![Candidate {
            url: "htree://npub1example/repo".into(),
            name: "repo".into(),
            publisher: "Alice".into(),
            distance: 1,
        }],
        truncated: false,
    };
    assert_eq!(
        selection::read_choice(&mut &b"1\n"[..], &results).unwrap(),
        results.candidates[0].url
    );
    for value in ["", "\n", "0\n", "2\n", "-1\n"] {
        assert!(selection::read_choice(&mut value.as_bytes(), &results).is_err());
    }
    assert!(suggestions(&results, "git clone").contains("git clone 'htree://npub1example/repo'"));
    assert!(choose(&results, "htree get", true).is_err());
}

#[test]
fn local_lists_override_stale_follows_and_hide_muted_publishers() {
    let me = Keys::generate();
    let friend = Keys::generate();
    let old = Keys::generate();
    let mut state = state::State::new([me.public_key()]);
    state.local_follows = Some([friend.public_key()].into());
    state.ingest(follows(&me, &[&old], 1));
    state.ingest(root(&old, "repo", true, 1));
    state.ingest(root(&friend, "repo", true, 1));
    let query = Query::parse("repo").unwrap();
    assert_eq!(state.results(&query, true).candidates.len(), 1);
    state.local_mutes.insert(friend.public_key());
    assert!(state.results(&query, true).candidates.is_empty());
    state.local_follows = Some(Default::default());
    assert_eq!(state.distances().len(), 1);
}

#[test]
fn local_lists_accept_legacy_and_reasoned_mutes_and_fail_on_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mutes.json");
    let author = Keys::generate().public_key();
    assert!(local::list(&path).unwrap().is_none());
    for value in [
        serde_json::json!([author.to_hex()]),
        serde_json::json!([{"pubkey": author.to_hex(), "reason": "test"}]),
    ] {
        std::fs::write(&path, value.to_string()).unwrap();
        assert_eq!(local::list(&path).unwrap().unwrap(), [author]);
    }
    std::fs::write(&path, "invalid").unwrap();
    assert!(local::list(&path).is_err());
}

#[test]
fn hides_private_heads_and_bounds_results() {
    let author = Keys::generate();
    let mut state = state::State::new([author.public_key()]);
    for i in 0..10 {
        state.ingest(root(&author, &format!("repo-{i}"), true, 1));
    }
    let query = Query::parse("repo").unwrap();
    let results = state.results(&query, true);
    assert_eq!(results.candidates.len(), 8);
    assert!(results.truncated);
    let public = root(&author, "repo-private", true, 1);
    let mut tags = public.tags.clone().to_vec();
    tags.push(Tag::parse(["selfEncryptedKey", "secret"]).unwrap());
    state.ingest(public);
    state.ingest(signed(&author, Kind::Custom(30064), "", tags, 2));
    assert!(state
        .results(&Query::parse("repo-private").unwrap(), true)
        .candidates
        .is_empty());
}
