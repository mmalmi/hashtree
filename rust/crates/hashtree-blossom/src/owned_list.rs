use super::*;

/// Maximum number of owner descriptors returned by a Hashtree BUD-02 list page.
pub const OWNER_LIST_PAGE_SIZE: usize = 1000;
const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
struct OwnedBlob {
    sha256: String,
    uploaded: u64,
}

impl BlossomClient {
    /// The public key used for signed Blossom requests.
    pub fn signer_public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// Check positive BUD-02 owner records for the client's signer on one writer.
    /// A capped timestamp bucket can hide records. Never infer ownership for an
    /// omitted hash, even if the server also reports that its bytes exist.
    pub async fn unconfirmed_owners_on_server(
        &self,
        server: &str,
        requested: &HashSet<[u8; 32]>,
    ) -> Result<HashSet<[u8; 32]>, BlossomError> {
        Ok(self.scan_owner_records(server, requested, false).await?.0)
    }

    /// Plan writes against the last observed owner timestamp, including rows from
    /// earlier pushes. Returns possible new claims and occupied slots. A small
    /// push that already fits needs only the first page. The publisher must
    /// still obtain positive owner records for every block after writing.
    pub async fn owner_claim_window(
        &self,
        server: &str,
        requested: &HashSet<[u8; 32]>,
    ) -> Result<(HashSet<[u8; 32]>, usize), BlossomError> {
        self.scan_owner_records(server, requested, true).await
    }

    async fn scan_owner_records(
        &self,
        server: &str,
        requested: &HashSet<[u8; 32]>,
        preparing_writes: bool,
    ) -> Result<(HashSet<[u8; 32]>, usize), BlossomError> {
        let mut missing = requested.clone();
        let mut until = None;
        let mut initial_bucket = 0;
        let mut first_page_at = None;
        while !missing.is_empty() {
            let page = self.owned_list_page(server, until).await?;
            if preparing_writes && until.is_none() {
                first_page_at = Some(std::time::Instant::now());
                if let Some(newest) = page.iter().map(|row| row.uploaded).max() {
                    initial_bucket = page.iter().filter(|row| row.uploaded == newest).count();
                }
            }
            let mut oldest = None;
            for descriptor in &page {
                if until.is_some_and(|until| descriptor.uploaded > until) {
                    return Err(BlossomError::UploadFailed(
                        "owner list ignored its time bound".into(),
                    ));
                }
                let mut hash = [0; 32];
                hex::decode_to_slice(&descriptor.sha256, &mut hash)
                    .map_err(|_| BlossomError::UploadFailed("invalid hash in owner list".into()))?;
                missing.remove(&hash);
                oldest = Some(oldest.map_or(descriptor.uploaded, |last: u64| {
                    last.min(descriptor.uploaded)
                }));
            }
            if page.len() < OWNER_LIST_PAGE_SIZE
                || missing.is_empty()
                || (preparing_writes && missing.len() <= OWNER_LIST_PAGE_SIZE - initial_bucket)
            {
                break;
            }
            let oldest = oldest.expect("a full page is nonempty");
            // Re-read the boundary second to include any rows that fell over
            // the previous page boundary. At the server's timestamp limit,
            // older seconds are still usable as positive evidence.
            until = if until == Some(oldest) {
                oldest.checked_sub(1)
            } else {
                Some(oldest)
            };
            if until.is_none() {
                break;
            }
        }
        // A full second since the page arrived means its timestamp has ended,
        // without assuming the publisher's clock agrees with the writer.
        if first_page_at.is_some_and(|time| time.elapsed() >= Duration::from_secs(1)) {
            initial_bucket = 0;
        }
        Ok((missing, initial_bucket))
    }

    async fn owned_list_page(
        &self,
        server: &str,
        until: Option<u64>,
    ) -> Result<Vec<OwnedBlob>, BlossomError> {
        let mut url = Url::parse(&format!(
            "{}/list/{}",
            server.trim_end_matches('/'),
            self.keys.public_key().to_hex()
        ))
        .map_err(|err| BlossomError::UploadFailed(format!("invalid owner list URL: {err}")))?;
        url.query_pairs_mut()
            .append_pair("limit", &OWNER_LIST_PAGE_SIZE.to_string());
        if let Some(until) = until {
            url.query_pairs_mut()
                .append_pair("until", &until.to_string());
        }
        let expires = Timestamp::now().as_secs().saturating_add(60);
        let event = EventBuilder::new(Kind::Custom(24242), "List owned blobs")
            .tags([
                Tag::custom(TagKind::custom("t"), ["list"]),
                Tag::custom(
                    TagKind::custom("server"),
                    [url.host_str().unwrap_or_default()],
                ),
                Tag::custom(TagKind::custom("expiration"), [expires.to_string()]),
            ])
            .sign_with_keys(&self.keys)
            .map_err(|err| BlossomError::Signing(err.to_string()))?;
        let token = base64::engine::general_purpose::STANDARD.encode(event.as_json());
        // This client disables automatic redirects. Keep author-scoped
        // credentials and ownership evidence on the selected writer.
        let response = self
            .upload_http
            .get(url)
            .header(AUTHORIZATION, format!("Nostr {token}"))
            .send()
            .await?;
        if response.status() != StatusCode::OK {
            return Err(BlossomError::UploadFailed(format!(
                "owner list returned {}",
                response.status()
            )));
        }
        let page: Vec<OwnedBlob> =
            bounded_descriptor_json(response, MAX_PAGE_BYTES, "owner list").await?;
        if page.len() > OWNER_LIST_PAGE_SIZE {
            return Err(BlossomError::UploadFailed(
                "owner list exceeded its requested page size".into(),
            ));
        }
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn hash(index: u32) -> [u8; 32] {
        let mut bytes = [0; 32];
        bytes[..4].copy_from_slice(&index.to_be_bytes());
        bytes
    }

    #[tokio::test]
    async fn claim_window_counts_existing_rows_before_consecutive_pushes() {
        let client = BlossomClient::new_empty(Keys::generate());
        let author = client.signer_public_key().to_hex();
        let requested: HashSet<_> = (700..1200).map(hash).collect();
        let rows = |range: std::ops::Range<u32>, timestamp: u64| {
            range.map(
                move |i| serde_json::json!({"sha256": hex::encode(hash(i)), "uploaded": timestamp}),
            )
        };
        // Exactly 1,000 rows fill the first page. The bounded scan looks at
        // the older boundary as well before allowing only 300 new claims.
        let first: Vec<_> = rows(0..700, 10).chain(rows(1200..1500, 9)).collect();
        let boundary: Vec<_> = rows(1200..1500, 9).collect();
        // A later repeat finds all 500 requested rows among the latest page.
        let repeat: Vec<_> = rows(700..1200, 11).chain(rows(0..500, 10)).collect();
        let pages = [("", first), ("&until=9", boundary), ("", repeat)];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (cursor, page) in pages {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                assert_eq!(
                    line,
                    format!("GET /list/{author}?limit=1000{cursor} HTTP/1.1\r\n")
                );
                loop {
                    line.clear();
                    assert!(stream.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                let body = serde_json::to_vec(&page).unwrap();
                let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                stream
                    .get_mut()
                    .write_all(headers.as_bytes())
                    .await
                    .unwrap();
                stream.get_mut().write_all(&body).await.unwrap();
            }
        });

        let (unknown, occupied) = client.owner_claim_window(&url, &requested).await.unwrap();
        assert_eq!(unknown, requested);
        assert_eq!(occupied, 700);
        assert_eq!(OWNER_LIST_PAGE_SIZE - occupied, 300);

        let (unknown, occupied) = client.owner_claim_window(&url, &requested).await.unwrap();
        assert!(unknown.is_empty());
        assert_eq!(occupied, 500);
        server.await.unwrap();
    }
}
