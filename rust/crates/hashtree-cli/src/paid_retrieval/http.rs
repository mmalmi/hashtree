//! The paid exchange over HTTP: `POST <gateway>/paid-blob`, one encoded
//! request as the body, one encoded reply in a `200`.
//!
//! The carrier lives here and not in `hashtree-paid-retrieval`, so that the
//! crate keeps no HTTP dependency.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use hashtree_core::StoreError;
use hashtree_paid_retrieval::{PaidBlobTransport, MAX_HEADER_BYTES, MAX_MESSAGE_BYTES};
use reqwest::{header, redirect, StatusCode, Url};

/// Path of the exchange below a gateway's base URL.
pub const PAID_BLOB_HTTP_PATH: &str = "/paid-blob";

/// Time to open a connection; the read's own deadline bounds the rest.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn failed(what: impl std::fmt::Display) -> StoreError {
    StoreError::Other(format!("paid route gateway {what}"))
}

/// The endpoint below `gateway`, a base URL. It has no user name, password
/// or fragment, and its scheme is `http` or `https`.
pub fn paid_blob_endpoint(gateway: &str) -> Result<Url> {
    let mut url = Url::parse(gateway.trim()).context("the gateway URL is invalid")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("the gateway URL must be http or https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("the gateway URL must not carry a user name or password");
    }
    if url.query().is_some() {
        bail!("the gateway URL must not carry a query");
    }
    let base = url.path().trim_end_matches('/').to_string();
    url.set_path(&format!("{base}{PAID_BLOB_HTTP_PATH}"));
    url.set_fragment(None);
    Ok(url)
}

/// One gateway's paid exchange, for one `PaidBlobRoute`.
///
/// Every protocol outcome is a `200`. Any other status, a network error or a
/// reply above the protocol's size limit is a failure, never "the provider
/// has no such blob". Nothing is retried here; the route's requests are safe
/// to repeat. The request names the account, a bearer secret, so redirects
/// are not followed and no cookies are kept. Over plain `http` to another
/// host the account id travels unencrypted.
pub struct HttpPaidTransport {
    client: reqwest::Client,
    url: Url,
}

impl HttpPaidTransport {
    /// The exchange of the gateway whose base URL is `gateway`.
    pub fn new(gateway: &str) -> Result<Self> {
        Self::at_endpoint(paid_blob_endpoint(gateway)?)
    }

    /// The exchange at `url`, an endpoint from [`paid_blob_endpoint`].
    pub fn at_endpoint(url: Url) -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .context("Failed to build the paid gateway HTTP client")?;
        Ok(Self { client, url })
    }

    /// Where requests are sent.
    pub fn url(&self) -> &Url {
        &self.url
    }
}

#[async_trait]
impl PaidBlobTransport for HttpPaidTransport {
    async fn exchange(&self, request: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        if request.len() > MAX_HEADER_BYTES {
            return Err(failed("request is too large"));
        }
        let response = self
            .client
            .post(self.url.clone())
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CACHE_CONTROL, "no-store")
            .body(request)
            .send()
            .await
            .map_err(|error| failed(format!("request failed: {error}")))?;
        if response.status() != StatusCode::OK {
            return Err(failed(format!("answered HTTP {}", response.status())));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_MESSAGE_BYTES as u64)
        {
            return Err(failed("reply is larger than a paid-retrieval message"));
        }
        let mut response = response;
        let mut reply = Vec::new();
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|error| failed(format!("reply could not be read: {error}")))?;
            let Some(chunk) = chunk else { break };
            if reply.len() + chunk.len() > MAX_MESSAGE_BYTES {
                return Err(failed("reply is larger than a paid-retrieval message"));
            }
            reply.extend_from_slice(&chunk);
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_sits_below_the_gateway_base() {
        let endpoint = |gateway| paid_blob_endpoint(gateway).unwrap().to_string();
        assert_eq!(
            endpoint("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/paid-blob"
        );
        assert_eq!(
            endpoint("https://gw.example/"),
            "https://gw.example/paid-blob"
        );
        assert_eq!(
            endpoint("https://gw.example/hashtree//#x"),
            "https://gw.example/hashtree/paid-blob"
        );
        for refused in [
            "ftp://gw.example",
            "https://user:secret@gw.example",
            "https://gw.example/?account=1",
            "not a url",
        ] {
            assert!(paid_blob_endpoint(refused).is_err(), "{refused}");
        }
    }
}
