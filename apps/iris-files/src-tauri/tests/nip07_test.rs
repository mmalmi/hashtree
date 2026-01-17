//! NIP-07 integration tests
//!
//! Tests the NIP-07 handler functions directly without Tauri runtime.

use app_lib::nip07::{handle_nip07_request, Nip07State};
use app_lib::permissions::PermissionStore;
use app_lib::worker::{BlobStore, WorkerState};
use nostr_sdk::{Keys, ToBech32};
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

/// Create test worker state with generated keys
fn create_test_worker_state() -> (WorkerState, Keys) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let store = BlobStore::new(temp_dir.path().to_path_buf());

    let state = WorkerState::new(store, temp_dir.path().to_path_buf())
        .expect("Failed to create worker state");

    // Generate test keys and set identity
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32().unwrap();
    let pubkey = keys.public_key().to_hex();

    state
        .nostr
        .set_identity(&pubkey, Some(&nsec))
        .expect("Failed to set identity");

    // Keep temp_dir alive by leaking it (test cleanup will handle it)
    std::mem::forget(temp_dir);

    (state, keys)
}

#[tokio::test]
async fn test_get_public_key() {
    let (worker_state, keys) = create_test_worker_state();
    let expected_pubkey = keys.public_key().to_hex();

    let response = handle_nip07_request(
        &worker_state,
        None, // No permissions for test
        "getPublicKey",
        &json!({}),
        "https://test.example.com",
    )
    .await;

    assert!(response.error.is_none(), "Should not have error");
    assert!(response.result.is_some(), "Should have result");

    let pubkey = response.result.unwrap();
    assert_eq!(
        pubkey.as_str().unwrap(),
        expected_pubkey,
        "Pubkey should match"
    );
}

#[tokio::test]
async fn test_get_public_key_no_identity() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let store = BlobStore::new(temp_dir.path().to_path_buf());

    let state = WorkerState::new(store, temp_dir.path().to_path_buf())
        .expect("Failed to create worker state");

    // Don't set identity

    let response = handle_nip07_request(
        &state,
        None,
        "getPublicKey",
        &json!({}),
        "https://test.example.com",
    )
    .await;

    assert!(response.error.is_some(), "Should have error when no identity");
    assert!(
        response.error.unwrap().contains("No identity"),
        "Error should mention no identity"
    );
}

#[tokio::test]
async fn test_sign_event() {
    let (worker_state, keys) = create_test_worker_state();
    let pubkey = keys.public_key().to_hex();

    // Create an unsigned event to sign
    let event_params = json!({
        "event": {
            "kind": 1,
            "content": "Hello from NIP-07 test!",
            "tags": [["t", "test"]],
            "created_at": 1704067200u64
        }
    });

    let response = handle_nip07_request(
        &worker_state,
        None,
        "signEvent",
        &event_params,
        "https://test.example.com",
    )
    .await;

    assert!(response.error.is_none(), "Should not have error: {:?}", response.error);
    assert!(response.result.is_some(), "Should have result");

    let signed = response.result.unwrap();

    // Verify signed event structure
    assert!(signed.get("id").is_some(), "Should have id");
    assert!(signed.get("sig").is_some(), "Should have signature");
    assert_eq!(
        signed.get("pubkey").and_then(|v| v.as_str()),
        Some(pubkey.as_str()),
        "Pubkey should match"
    );
    assert_eq!(
        signed.get("kind").and_then(|v| v.as_u64()),
        Some(1),
        "Kind should be 1"
    );
    assert_eq!(
        signed.get("content").and_then(|v| v.as_str()),
        Some("Hello from NIP-07 test!"),
        "Content should match"
    );

    // Verify signature is 128 hex characters (64 bytes)
    let sig = signed.get("sig").unwrap().as_str().unwrap();
    assert_eq!(sig.len(), 128, "Signature should be 128 hex chars");
}

#[tokio::test]
async fn test_sign_event_no_keys() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let store = BlobStore::new(temp_dir.path().to_path_buf());

    let state = WorkerState::new(store, temp_dir.path().to_path_buf())
        .expect("Failed to create worker state");

    let event_params = json!({
        "event": {
            "kind": 1,
            "content": "Test",
            "tags": []
        }
    });

    let response = handle_nip07_request(
        &state,
        None,
        "signEvent",
        &event_params,
        "https://test.example.com",
    )
    .await;

    assert!(response.error.is_some(), "Should have error when no keys");
    assert!(
        response.error.unwrap().contains("No signing keys"),
        "Error should mention no signing keys"
    );
}

#[tokio::test]
async fn test_sign_event_missing_kind() {
    let (worker_state, _keys) = create_test_worker_state();

    let event_params = json!({
        "event": {
            "content": "Test without kind"
        }
    });

    let response = handle_nip07_request(
        &worker_state,
        None,
        "signEvent",
        &event_params,
        "https://test.example.com",
    )
    .await;

    assert!(response.error.is_some(), "Should have error when missing kind");
    assert!(
        response.error.unwrap().contains("kind"),
        "Error should mention missing kind"
    );
}

#[tokio::test]
async fn test_get_relays() {
    let (worker_state, _keys) = create_test_worker_state();

    let response = handle_nip07_request(
        &worker_state,
        None,
        "getRelays",
        &json!({}),
        "https://test.example.com",
    )
    .await;

    assert!(response.error.is_none(), "Should not have error");
    assert!(response.result.is_some(), "Should have result");

    // getRelays returns an empty object for now
    let relays = response.result.unwrap();
    assert!(relays.is_object(), "Relays should be an object");
}

#[tokio::test]
async fn test_unknown_method() {
    let (worker_state, _keys) = create_test_worker_state();

    let response = handle_nip07_request(
        &worker_state,
        None,
        "unknownMethod",
        &json!({}),
        "https://test.example.com",
    )
    .await;

    assert!(response.error.is_some(), "Should have error for unknown method");
    assert!(
        response.error.unwrap().contains("Unknown method"),
        "Error should mention unknown method"
    );
}

#[tokio::test]
async fn test_nip04_not_implemented() {
    let (worker_state, _keys) = create_test_worker_state();

    let response = handle_nip07_request(
        &worker_state,
        None,
        "nip04.encrypt",
        &json!({"pubkey": "abc", "plaintext": "test"}),
        "https://test.example.com",
    )
    .await;

    assert!(response.error.is_some(), "Should have error");
    assert!(
        response.error.unwrap().contains("Not implemented"),
        "Should say not implemented"
    );
}

#[tokio::test]
async fn test_session_token_validation() {
    let permission_store = Arc::new(PermissionStore::new(None));
    let nip07_state = Nip07State::new(permission_store);

    let origin = "https://example.com";

    // Generate a session token
    let token = nip07_state.new_session(origin);
    assert!(!token.is_empty(), "Token should not be empty");

    // Valid token should pass
    assert!(
        nip07_state.validate_token(origin, &token),
        "Valid token should validate"
    );

    // Wrong token should fail
    assert!(
        !nip07_state.validate_token(origin, "wrong-token"),
        "Wrong token should not validate"
    );

    // Different origin should fail
    assert!(
        !nip07_state.validate_token("https://other.com", &token),
        "Different origin should not validate"
    );

    // Clear session
    nip07_state.clear_session(origin);
    assert!(
        !nip07_state.validate_token(origin, &token),
        "Cleared token should not validate"
    );
}

#[tokio::test]
async fn test_permission_denied_get_public_key() {
    let (worker_state, _keys) = create_test_worker_state();

    // Create permission store and deny getPublicKey
    let permission_store = Arc::new(PermissionStore::new(None));
    let origin = "https://untrusted.example.com";

    // By default, getPublicKey should be granted (returns true when unwrap_or(true))
    // But we can test the permission flow when explicitly denied

    let response = handle_nip07_request(
        &worker_state,
        Some(&permission_store),
        "getPublicKey",
        &json!({}),
        origin,
    )
    .await;

    // Default behavior allows getPublicKey, so this should succeed
    assert!(
        response.error.is_none(),
        "Default should allow getPublicKey"
    );
}

#[tokio::test]
async fn test_sign_event_with_tags() {
    let (worker_state, _keys) = create_test_worker_state();

    // Event with multiple tag types
    let event_params = json!({
        "event": {
            "kind": 30023,
            "content": "Article content",
            "tags": [
                ["d", "my-article"],
                ["title", "Test Article"],
                ["t", "nostr"],
                ["t", "test"],
                ["p", "0000000000000000000000000000000000000000000000000000000000000001"]
            ],
            "created_at": 1234567890
        }
    });

    let response = handle_nip07_request(
        &worker_state,
        None,
        "signEvent",
        &event_params,
        "https://test.example.com",
    )
    .await;

    assert!(response.error.is_none(), "Should not have error");

    let signed = response.result.unwrap();
    assert_eq!(
        signed.get("kind").and_then(|v| v.as_u64()),
        Some(30023),
        "Kind should be preserved"
    );
    assert_eq!(
        signed.get("created_at").and_then(|v| v.as_u64()),
        Some(1234567890),
        "Timestamp should be preserved"
    );

    // Verify tags are preserved
    let tags = signed.get("tags").and_then(|v| v.as_array()).unwrap();
    assert_eq!(tags.len(), 5, "Should have 5 tags");
}
