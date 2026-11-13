//! `POST /paid-blob`: the paid exchange over HTTP. One encoded request as the
//! body, one encoded reply in a `200`, whatever its protocol outcome.

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use hashtree_paid_retrieval::MAX_HEADER_BYTES;

use super::auth::AppState;
use crate::paid_retrieval::HttpExchange;

fn answer(status: StatusCode, content_type: &str, body: Body) -> Response {
    Response::builder()
        .status(status)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_TYPE, content_type)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(body)
        .unwrap()
}

fn refuse(status: StatusCode, reason: &'static str) -> Response {
    let mut response = answer(status, "text/plain; charset=utf-8", Body::from(reason));
    response
        .headers_mut()
        .insert("X-Reason", header::HeaderValue::from_static(reason));
    response
}

/// The request body, refused once it is larger than a request may be.
async fn read_request(headers: &HeaderMap, body: Body) -> Result<Vec<u8>, Response> {
    let too_large = || refuse(StatusCode::PAYLOAD_TOO_LARGE, "request is too large");
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > MAX_HEADER_BYTES as u64) {
        return Err(too_large());
    }
    let mut request = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return Err(refuse(StatusCode::BAD_REQUEST, "request could not be read"));
        };
        if request.len() + chunk.len() > MAX_HEADER_BYTES {
            return Err(too_large());
        }
        request.extend_from_slice(&chunk);
    }
    Ok(request)
}

pub(super) async fn paid_blob_exchange(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    let Some(paid) = state.paid_retrieval.clone() else {
        return refuse(StatusCode::NOT_FOUND, "this gateway does not sell blobs");
    };
    let request = match read_request(&headers, body).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    match paid.exchange_over_http(&request).await {
        HttpExchange::NotSold => refuse(StatusCode::NOT_FOUND, "this gateway does not sell blobs"),
        HttpExchange::Busy => {
            let mut response = refuse(StatusCode::SERVICE_UNAVAILABLE, "paid exchanges are busy");
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
            response
        }
        HttpExchange::Reply(reply) => answer(
            StatusCode::OK,
            "application/octet-stream",
            Body::from(reply),
        ),
    }
}
