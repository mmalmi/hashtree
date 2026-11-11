use super::{auth::AppState, handlers, INTERNAL_JSON_BODY_LIMIT_BYTES};
use axum::{
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use std::net::{IpAddr, SocketAddr};

/// A peer is local only when it connects from loopback and the request carries
/// no forwarding markers. A reverse proxy on the same host also connects from
/// loopback, so markers can only remove access; their values never establish
/// locality.
pub(super) fn is_direct_loopback(ip: IpAddr, headers: &HeaderMap) -> bool {
    ip.is_loopback()
        && !headers.keys().any(|name| {
            let name = name.as_str();
            name.starts_with("x-forwarded-")
                || matches!(
                    name,
                    "forwarded"
                        | "via"
                        | "x-real-ip"
                        | "cf-ray"
                        | "cf-connecting-ip"
                        | "cf-connecting-ipv6"
                )
        })
}

#[derive(Clone, Copy)]
enum LocalApi {
    Read,
    Publish,
}

async fn require_direct_loopback(
    State(api): State<LocalApi>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if is_direct_loopback(peer.ip(), request.headers()) {
        return next.run(request).await;
    }
    match api {
        LocalApi::Read => (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "localhost only"})),
        )
            .into_response(),
        LocalApi::Publish => Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from("Nostr event publication is loopback-only"))
            .unwrap(),
    }
}

/// Routes that serve local callers only. `/api/status` enforces the same
/// boundary inside its handler.
pub(super) fn routes(public_snapshot: bool) -> Router<AppState> {
    let local_read = middleware::from_fn_with_state(LocalApi::Read, require_direct_loopback);
    let snapshot = get(handlers::socialgraph_snapshot);
    let snapshot = if public_snapshot {
        snapshot
    } else {
        snapshot.layer(local_read)
    };
    Router::new()
        .route("/api/status", get(handlers::daemon_status))
        .route("/api/socialgraph/snapshot", snapshot)
        .route(
            "/api/nostr/events",
            post(handlers::publish_nostr_event)
                .layer(DefaultBodyLimit::max(INTERNAL_JSON_BODY_LIMIT_BYTES))
                .layer(middleware::from_fn_with_state(
                    LocalApi::Publish,
                    require_direct_loopback,
                )),
        )
}
