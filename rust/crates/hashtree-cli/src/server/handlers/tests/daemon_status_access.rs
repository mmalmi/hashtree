use super::{daemon_status, test_app_state};
use crate::{server::auth::AppState, storage::HashtreeStore};
use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc};
use tempfile::TempDir;
use tower::ServiceExt;

async fn request_status(state: AppState, peer: &str, headers: HeaderMap) -> Response {
    let mut request = Request::builder()
        .uri("/api/status")
        .extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()))
        .body(Body::empty())
        .unwrap();
    *request.headers_mut() = headers;
    Router::new()
        .route("/api/status", get(daemon_status))
        .with_state(state)
        .oneshot(request)
        .await
        .unwrap()
}

async fn assert_forbidden(response: Response) {
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload, json!({"error": "localhost only"}));
}

#[tokio::test]
async fn daemon_status_allows_direct_ipv4_and_ipv6_loopback() {
    let temp = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp.path()).unwrap());
    let state = test_app_state(store, Vec::new());
    for peer in ["127.0.0.1:21417", "127.42.0.7:21417", "[::1]:21417"] {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("localhost:8080"));
        headers.insert("accept", HeaderValue::from_static("application/json"));
        headers.insert("x-request-id", HeaderValue::from_static("direct-cli-test"));
        let response = request_status(state.clone(), peer, headers).await;
        assert_eq!(response.status(), StatusCode::OK, "{peer}");
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["status"], "running");
        assert!(payload["queues"].is_object());
    }
}

#[tokio::test]
async fn daemon_status_rejects_remote_peers_and_forged_local_claims() {
    let temp = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp.path()).unwrap());
    let state = test_app_state(store, Vec::new());
    for peer in ["192.0.2.1:21417", "10.0.0.1:21417", "[2001:db8::1]:21417"] {
        assert_forbidden(request_status(state.clone(), peer, HeaderMap::new()).await).await;
        let mut headers = HeaderMap::new();
        headers.insert("x-client-ip", HeaderValue::from_static("127.0.0.1"));
        headers.insert("x-local-request", HeaderValue::from_static("true"));
        headers.insert("origin", HeaderValue::from_static("http://localhost"));
        headers.insert("x-forwarded-for", HeaderValue::from_static("127.0.0.1"));
        headers.insert("cf-connecting-ip", HeaderValue::from_static("::1"));
        headers.insert("forwarded", HeaderValue::from_static("for=127.0.0.1"));
        assert_forbidden(request_status(state.clone(), peer, headers).await).await;
    }
}

#[tokio::test]
async fn daemon_status_rejects_proxy_header_presence_for_loopback_peers() {
    let temp = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp.path()).unwrap());
    let state = test_app_state(store, Vec::new());
    for peer in ["127.0.0.1:21417", "[::1]:21417"] {
        for name in [
            "Cf-RaY",
            "CF-Connecting-IP",
            "CF-Connecting-IPv6",
            "Forwarded",
            "X-Forwarded-For",
            "X-Forwarded-Host",
            "X-Forwarded-Proto",
            "X-Real-IP",
        ] {
            for value in [
                HeaderValue::from_static(""),
                HeaderValue::from_static("127.0.0.1"),
                HeaderValue::from_static("for=\"[::1]\", for=192.0.2.1"),
                HeaderValue::from_static("not-an-ip-or-protocol"),
                HeaderValue::from_bytes(&[0xff]).unwrap(),
            ] {
                let mut headers = HeaderMap::new();
                headers.insert("host", HeaderValue::from_static("public.example"));
                headers.insert("origin", HeaderValue::from_static("https://public.example"));
                headers.append(HeaderName::from_bytes(name.as_bytes()).unwrap(), value);
                assert_forbidden(request_status(state.clone(), peer, headers.clone()).await).await;
                headers.append(
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_static("127.0.0.1"),
                );
                assert_forbidden(request_status(state.clone(), peer, headers).await).await;
            }
        }
    }
}
