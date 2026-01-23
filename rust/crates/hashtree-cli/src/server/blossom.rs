//! Blossom protocol implementation (BUD-01, BUD-02)
//!
//! Implements blob storage endpoints with Nostr-based authentication.
//! See: https://github.com/hzrd149/blossom

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, Response, StatusCode},
    response::IntoResponse,
};
use base64::Engine;
use hashtree_core::from_hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

use super::auth::AppState;
use super::mime::get_mime_type;

/// Blossom authorization event kind (NIP-98 style)
const BLOSSOM_AUTH_KIND: u16 = 24242;

/// Cache-Control header for immutable content-addressed data (1 year)
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// Default maximum upload size in bytes (5 MB)
pub const DEFAULT_MAX_UPLOAD_SIZE: usize = 5 * 1024 * 1024;

/// Check if a pubkey has write access based on allowed_npubs config
/// Returns Ok(()) if allowed, Err with JSON error body if denied
fn check_write_access(state: &AppState, pubkey: &str) -> Result<(), Response<Body>> {
    // Check if pubkey is in the allowed list (converted from npub to hex)
    if state.allowed_pubkeys.contains(pubkey) {
        tracing::debug!("Blossom write allowed for {}... (allowed npub)", &pubkey[..8.min(pubkey.len())]);
        return Ok(());
    }

    // Not in allowed list
    tracing::info!("Blossom write denied for {}... (not in allowed_npubs)", &pubkey[..8.min(pubkey.len())]);
    Err(Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"error":"Write access denied. Your pubkey is not in the allowed list."}"#))
        .unwrap())
}

/// Blob descriptor returned by upload and list endpoints
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobDescriptor {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    #[serde(rename = "type")]
    pub mime_type: String,
    pub uploaded: u64,
}

/// Query parameters for list endpoint
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// Parsed Nostr authorization event
#[derive(Debug)]
pub struct BlossomAuth {
    pub pubkey: String,
    pub kind: u16,
    pub created_at: u64,
    pub expiration: Option<u64>,
    pub action: Option<String>,       // "upload", "delete", "list", "get"
    pub blob_hashes: Vec<String>,     // x tags
    pub server: Option<String>,       // server tag
}

/// Parse and verify Nostr authorization from header
/// Returns the verified auth or an error response
pub fn verify_blossom_auth(
    headers: &HeaderMap,
    required_action: &str,
    required_hash: Option<&str>,
) -> Result<BlossomAuth, (StatusCode, &'static str)> {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or((StatusCode::UNAUTHORIZED, "Missing Authorization header"))?;

    let nostr_event = auth_header
        .strip_prefix("Nostr ")
        .ok_or((StatusCode::UNAUTHORIZED, "Invalid auth scheme, expected 'Nostr'"))?;

    // Decode base64 event
    let engine = base64::engine::general_purpose::STANDARD;
    let event_bytes = engine
        .decode(nostr_event)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid base64 in auth header"))?;

    let event_json: serde_json::Value = serde_json::from_slice(&event_bytes)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid JSON in auth event"))?;

    // Extract event fields
    let kind = event_json["kind"]
        .as_u64()
        .ok_or((StatusCode::BAD_REQUEST, "Missing kind in event"))?;

    if kind != BLOSSOM_AUTH_KIND as u64 {
        return Err((StatusCode::BAD_REQUEST, "Invalid event kind, expected 24242"));
    }

    let pubkey = event_json["pubkey"]
        .as_str()
        .ok_or((StatusCode::BAD_REQUEST, "Missing pubkey in event"))?
        .to_string();

    let created_at = event_json["created_at"]
        .as_u64()
        .ok_or((StatusCode::BAD_REQUEST, "Missing created_at in event"))?;

    let sig = event_json["sig"]
        .as_str()
        .ok_or((StatusCode::BAD_REQUEST, "Missing signature in event"))?;

    // Verify signature
    if !verify_nostr_signature(&event_json, &pubkey, sig) {
        return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
    }

    // Parse tags
    let tags = event_json["tags"]
        .as_array()
        .ok_or((StatusCode::BAD_REQUEST, "Missing tags in event"))?;

    let mut expiration: Option<u64> = None;
    let mut action: Option<String> = None;
    let mut blob_hashes: Vec<String> = Vec::new();
    let mut server: Option<String> = None;

    for tag in tags {
        let tag_arr = tag.as_array();
        if let Some(arr) = tag_arr {
            if arr.len() >= 2 {
                let tag_name = arr[0].as_str().unwrap_or("");
                let tag_value = arr[1].as_str().unwrap_or("");

                match tag_name {
                    "t" => action = Some(tag_value.to_string()),
                    "x" => blob_hashes.push(tag_value.to_lowercase()),
                    "expiration" => expiration = tag_value.parse().ok(),
                    "server" => server = Some(tag_value.to_string()),
                    _ => {}
                }
            }
        }
    }

    // Validate expiration
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    if let Some(exp) = expiration {
        if exp < now {
            return Err((StatusCode::UNAUTHORIZED, "Authorization expired"));
        }
    }

    // Validate created_at is not in the future (with 60s tolerance)
    if created_at > now + 60 {
        return Err((StatusCode::BAD_REQUEST, "Event created_at is in the future"));
    }

    // Validate action matches
    if let Some(ref act) = action {
        if act != required_action {
            return Err((StatusCode::FORBIDDEN, "Action mismatch"));
        }
    } else {
        return Err((StatusCode::BAD_REQUEST, "Missing 't' tag for action"));
    }

    // Validate hash if required
    if let Some(hash) = required_hash {
        if !blob_hashes.is_empty() && !blob_hashes.contains(&hash.to_lowercase()) {
            return Err((StatusCode::FORBIDDEN, "Blob hash not authorized"));
        }
    }

    Ok(BlossomAuth {
        pubkey,
        kind: kind as u16,
        created_at,
        expiration,
        action,
        blob_hashes,
        server,
    })
}

/// Verify Nostr event signature using secp256k1
fn verify_nostr_signature(event: &serde_json::Value, pubkey: &str, sig: &str) -> bool {
    use secp256k1::{Message, Secp256k1, schnorr::Signature, XOnlyPublicKey};

    // Compute event ID (sha256 of serialized event)
    let content = event["content"].as_str().unwrap_or("");
    let full_serialized = format!(
        "[0,\"{}\",{},{},{},\"{}\"]",
        pubkey,
        event["created_at"],
        event["kind"],
        event["tags"],
        escape_json_string(content),
    );

    let mut hasher = Sha256::new();
    hasher.update(full_serialized.as_bytes());
    let event_id = hasher.finalize();

    // Parse pubkey and signature
    let pubkey_bytes = match hex::decode(pubkey) {
        Ok(b) => b,
        Err(_) => return false,
    };

    let sig_bytes = match hex::decode(sig) {
        Ok(b) => b,
        Err(_) => return false,
    };

    let secp = Secp256k1::verification_only();

    let xonly_pubkey = match XOnlyPublicKey::from_slice(&pubkey_bytes) {
        Ok(pk) => pk,
        Err(_) => return false,
    };

    let signature = match Signature::from_slice(&sig_bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };

    let message = match Message::from_digest_slice(&event_id) {
        Ok(m) => m,
        Err(_) => return false,
    };

    secp.verify_schnorr(&signature, &message, &xonly_pubkey).is_ok()
}

/// Escape string for JSON serialization
fn escape_json_string(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c.is_control() => {
                result.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => result.push(c),
        }
    }
    result
}

/// CORS preflight handler for all Blossom endpoints
/// Echoes back Access-Control-Request-Headers to allow any headers
pub async fn cors_preflight(headers: HeaderMap) -> impl IntoResponse {
    // Echo back requested headers, or use sensible defaults that cover common Blossom headers
    let allowed_headers = headers
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("Authorization, Content-Type, X-SHA-256, x-sha-256");

    // Always include common headers in addition to what was requested
    let full_allowed = format!(
        "{}, Authorization, Content-Type, X-SHA-256, x-sha-256, Accept, Cache-Control",
        allowed_headers
    );

    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD, PUT, DELETE, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, full_allowed)
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .unwrap()
}

/// HEAD /<sha256> - Check if blob exists
pub async fn head_blob(
    State(state): State<AppState>,
    Path(id): Path<String>,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let is_localhost = connect_info.0.ip().is_loopback();
    let (hash_part, ext) = parse_hash_and_extension(&id);

    if !is_valid_sha256(&hash_part) {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Invalid SHA256 hash")
            .body(Body::empty())
            .unwrap();
    }

    let sha256_hex = hash_part.to_lowercase();
    let sha256_bytes: [u8; 32] = match from_hex(&sha256_hex) {
        Ok(b) => b,
        Err(_) => return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Invalid SHA256 format")
            .body(Body::empty())
            .unwrap(),
    };

    // Blossom only serves raw blobs (not merkle tree structures)
    match state.store.get_blob(&sha256_bytes) {
        Ok(Some(data)) => {
            let mime_type = ext
                .map(|e| get_mime_type(&format!("file{}", e)))
                .unwrap_or("application/octet-stream");

            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime_type)
                .header(header::CONTENT_LENGTH, data.len())
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
            if is_localhost {
                builder = builder.header("X-Source", "local");
            }
            builder.body(Body::empty()).unwrap()
        }
        Ok(None) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Blob not found")
            .body(Body::empty())
            .unwrap(),
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::empty())
            .unwrap(),
    }
}

/// PUT /upload - Upload a new blob (BUD-02)
pub async fn upload_blob(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Check size limit first (before auth to save resources)
    let max_size = state.max_upload_bytes;
    if body.len() > max_size {
        return Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(
                r#"{{"error":"Upload size {} bytes exceeds maximum {} bytes ({} MB)"}}"#,
                body.len(),
                max_size,
                max_size / 1024 / 1024
            )))
            .unwrap();
    }

    // Verify authorization
    let auth = match verify_blossom_auth(&headers, "upload", None) {
        Ok(a) => a,
        Err((status, reason)) => {
            return Response::builder()
                .status(status)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", reason)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"error":"{}"}}"#, reason)))
                .unwrap();
        }
    };

    // Get content type from header
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    // Check write access: either in allowed_npubs list OR public_writes is enabled
    let is_allowed = check_write_access(&state, &auth.pubkey).is_ok();
    let can_upload = is_allowed || state.public_writes;

    if !can_upload {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":"Write access denied. Your pubkey is not in the allowed list and public writes are disabled."}"#))
            .unwrap();
    }

    // Compute SHA256 of uploaded data
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let sha256_hash: [u8; 32] = hasher.finalize().into();
    let sha256_hex = hex::encode(sha256_hash);

    // If auth has x tags, verify hash matches
    if !auth.blob_hashes.is_empty() && !auth.blob_hashes.contains(&sha256_hex) {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Uploaded blob hash does not match authorized hash")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":"Hash mismatch"}"#))
            .unwrap();
    }

    // Convert pubkey hex to bytes
    let pubkey_bytes = match from_hex(&auth.pubkey) {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Invalid pubkey format")
                .body(Body::empty())
                .unwrap();
        }
    };

    let size = body.len() as u64;

    // Store the blob (only track ownership if user is in allowed list)
    let store_result = store_blossom_blob(&state, &body, &sha256_hash, &pubkey_bytes, is_allowed);

    match store_result {
        Ok(()) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            // Determine file extension from content type
            let ext = mime_to_extension(&content_type);

            let descriptor = BlobDescriptor {
                url: format!("/{}{}", sha256_hex, ext),
                sha256: sha256_hex,
                size,
                mime_type: content_type,
                uploaded: now,
            };

            Response::builder()
                .status(StatusCode::OK)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_string(&descriptor).unwrap()))
                .unwrap()
        }
        Err(e) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Storage error")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"error":"{}"}}"#, e)))
            .unwrap(),
    }
}

/// DELETE /<sha256> - Delete a blob (BUD-02)
/// Note: Blob is only fully deleted when ALL owners have removed it
pub async fn delete_blob(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let (hash_part, _) = parse_hash_and_extension(&id);

    if !is_valid_sha256(&hash_part) {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Invalid SHA256 hash")
            .body(Body::empty())
            .unwrap();
    }

    let sha256_hex = hash_part.to_lowercase();

    // Convert hash to bytes
    let sha256_bytes = match from_hex(&sha256_hex) {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Invalid SHA256 hash format")
                .body(Body::empty())
                .unwrap();
        }
    };

    // Verify authorization with hash requirement
    let auth = match verify_blossom_auth(&headers, "delete", Some(&sha256_hex)) {
        Ok(a) => a,
        Err((status, reason)) => {
            return Response::builder()
                .status(status)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", reason)
                .body(Body::empty())
                .unwrap();
        }
    };

    // Convert pubkey hex to bytes
    let pubkey_bytes = match from_hex(&auth.pubkey) {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Invalid pubkey format")
                .body(Body::empty())
                .unwrap();
        }
    };

    // Check ownership - user must be one of the owners (O(1) lookup with composite key)
    match state.store.is_blob_owner(&sha256_bytes, &pubkey_bytes) {
        Ok(true) => {
            // User is an owner, proceed with delete
        }
        Ok(false) => {
            // Check if blob exists at all (for proper error message)
            match state.store.blob_has_owners(&sha256_bytes) {
                Ok(true) => {
                    return Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .header("X-Reason", "Not a blob owner")
                        .body(Body::empty())
                        .unwrap();
                }
                Ok(false) => {
                    return Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .header("X-Reason", "Blob not found")
                        .body(Body::empty())
                        .unwrap();
                }
                Err(_) => {
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(Body::empty())
                        .unwrap();
                }
            }
        }
        Err(_) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::empty())
                .unwrap();
        }
    }

    // Remove this user's ownership (blob only deleted when no owners remain)
    match state.store.delete_blossom_blob(&sha256_bytes, &pubkey_bytes) {
        Ok(fully_deleted) => {
            // Return 200 OK whether blob was fully deleted or just removed from user's list
            // The client doesn't need to know if other owners still exist
            Response::builder()
                .status(StatusCode::OK)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Blob-Deleted", if fully_deleted { "true" } else { "false" })
                .body(Body::empty())
                .unwrap()
        }
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::empty())
            .unwrap(),
    }
}

/// GET /list/<pubkey> - List blobs for a pubkey (BUD-02)
pub async fn list_blobs(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Validate pubkey format (64 hex chars)
    if pubkey.len() != 64 || !pubkey.chars().all(|c| c.is_ascii_hexdigit()) {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Invalid pubkey format")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("[]"))
            .unwrap();
    }

    let pubkey_hex = pubkey.to_lowercase();
    let pubkey_bytes: [u8; 32] = match from_hex(&pubkey_hex) {
        Ok(b) => b,
        Err(_) => return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Invalid pubkey format")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("[]"))
            .unwrap(),
    };

    // Optional auth verification for list
    let _auth = verify_blossom_auth(&headers, "list", None).ok();

    // Get blobs for this pubkey
    match state.store.list_blobs_by_pubkey(&pubkey_bytes) {
        Ok(blobs) => {
            // Apply filters
            let mut filtered: Vec<_> = blobs
                .into_iter()
                .filter(|b| {
                    if let Some(since) = query.since {
                        if b.uploaded < since {
                            return false;
                        }
                    }
                    if let Some(until) = query.until {
                        if b.uploaded > until {
                            return false;
                        }
                    }
                    true
                })
                .collect();

            // Sort by uploaded descending (most recent first)
            filtered.sort_by(|a, b| b.uploaded.cmp(&a.uploaded));

            // Apply limit
            let limit = query.limit.unwrap_or(100).min(1000);
            filtered.truncate(limit);

            Response::builder()
                .status(StatusCode::OK)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_string(&filtered).unwrap()))
                .unwrap()
        }
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("[]"))
            .unwrap(),
    }
}

// Helper functions

fn parse_hash_and_extension(id: &str) -> (&str, Option<&str>) {
    if let Some(dot_pos) = id.rfind('.') {
        (&id[..dot_pos], Some(&id[dot_pos..]))
    } else {
        (id, None)
    }
}

fn is_valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn store_blossom_blob(
    state: &AppState,
    data: &[u8],
    sha256: &[u8; 32],
    pubkey: &[u8; 32],
    track_ownership: bool,
) -> anyhow::Result<()> {
    // Store as raw blob only - no tree creation needed for blossom
    // This avoids sync_block_on which can deadlock under load
    state.store.put_blob(data)?;

    // Only track ownership for social graph members
    // Non-members can upload (if public_writes=true) but can't delete
    if track_ownership {
        state.store.set_blob_owner(sha256, pubkey)?;
    }

    Ok(())
}

fn mime_to_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/svg+xml" => ".svg",
        "video/mp4" => ".mp4",
        "video/webm" => ".webm",
        "audio/mpeg" => ".mp3",
        "audio/ogg" => ".ogg",
        "application/pdf" => ".pdf",
        "text/plain" => ".txt",
        "text/html" => ".html",
        "application/json" => ".json",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_sha256() {
        assert!(is_valid_sha256("e2bab35b5296ec2242ded0a01f6d6723a5cd921239280c0a5f0b5589303336b6"));
        assert!(is_valid_sha256("0000000000000000000000000000000000000000000000000000000000000000"));

        // Too short
        assert!(!is_valid_sha256("e2bab35b5296ec2242ded0a01f6d6723"));
        // Too long
        assert!(!is_valid_sha256("e2bab35b5296ec2242ded0a01f6d6723a5cd921239280c0a5f0b5589303336b6aa"));
        // Invalid chars
        assert!(!is_valid_sha256("zzbab35b5296ec2242ded0a01f6d6723a5cd921239280c0a5f0b5589303336b6"));
        // Empty
        assert!(!is_valid_sha256(""));
    }

    #[test]
    fn test_parse_hash_and_extension() {
        let (hash, ext) = parse_hash_and_extension("abc123.png");
        assert_eq!(hash, "abc123");
        assert_eq!(ext, Some(".png"));

        let (hash2, ext2) = parse_hash_and_extension("abc123");
        assert_eq!(hash2, "abc123");
        assert_eq!(ext2, None);

        let (hash3, ext3) = parse_hash_and_extension("abc.123.jpg");
        assert_eq!(hash3, "abc.123");
        assert_eq!(ext3, Some(".jpg"));
    }

    #[test]
    fn test_mime_to_extension() {
        assert_eq!(mime_to_extension("image/png"), ".png");
        assert_eq!(mime_to_extension("image/jpeg"), ".jpg");
        assert_eq!(mime_to_extension("video/mp4"), ".mp4");
        assert_eq!(mime_to_extension("application/octet-stream"), "");
        assert_eq!(mime_to_extension("unknown/type"), "");
    }
}
