//! A small BUD-02 owner listing shared by the loopback Blossom fixtures.

use axum::{
    body::Body,
    http::{header, HeaderMap, Response, StatusCode},
};
use nostr::base64::{engine::general_purpose::STANDARD, Engine};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct OwnerClaims(HashMap<String, BTreeMap<String, u64>>);

#[derive(serde::Deserialize)]
pub struct ListQuery {
    since: Option<u64>,
    until: Option<u64>,
    limit: Option<usize>,
}

impl OwnerClaims {
    pub fn record(&mut self, author: Option<&str>, hash: &str) {
        if let Some(owner) = author {
            self.0
                .entry(owner.to_owned())
                .or_default()
                .entry(hash.to_owned())
                .or_insert_with(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                });
        }
    }

    pub fn contains(&self, owner: &str, hash: &str) -> bool {
        self.0
            .get(owner)
            .is_some_and(|hashes| hashes.contains_key(hash))
    }

    pub fn list(&self, headers: &HeaderMap, owner: &str, query: &ListQuery) -> Response<Body> {
        if signed_author(headers, "list").as_deref() != Some(owner) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }
        let mut rows: Vec<_> = self
            .0
            .get(owner)
            .into_iter()
            .flatten()
            .filter(|(_, time)| {
                query.since.is_none_or(|since| **time >= since)
                    && query.until.is_none_or(|until| **time <= until)
            })
            .collect();
        rows.sort_by_key(|(_, time)| Reverse(**time));
        let descriptors: Vec<_> = rows
            .into_iter()
            .take(query.limit.unwrap_or(100).min(1000))
            .map(|(hash, time)| serde_json::json!({"sha256":hash,"uploaded":time}))
            .collect();
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&descriptors).unwrap()))
            .unwrap()
    }
}

pub fn signed_author(headers: &HeaderMap, operation: &str) -> Option<String> {
    let encoded = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Nostr ")?;
    let event: nostr::Event = serde_json::from_slice(&STANDARD.decode(encoded).ok()?).ok()?;
    event.verify().ok()?;
    (event.kind.as_u16() == 24242
        && event.tags.iter().any(|tag| {
            let fields = tag.as_slice();
            fields.len() == 2 && fields[0] == "t" && fields[1] == operation
        }))
    .then(|| event.pubkey.to_hex())
}
