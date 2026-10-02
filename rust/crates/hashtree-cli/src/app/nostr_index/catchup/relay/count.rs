use super::*;

impl RelaySource {
    pub(super) async fn count(
        &mut self,
        relay: &str,
        query: &CatchupQuery,
    ) -> Result<Option<usize>> {
        let deadline = Instant::now() + self.timeout;
        let mut budget = QueryBudget::default();
        let result = tokio::time::timeout_at(deadline, async {
            for attempt in 0..=1 {
                match self.count_inner(relay, query, &mut budget).await {
                    Ok(count) => return Ok(count),
                    Err(failure) => {
                        self.sockets.remove(relay);
                        if attempt == 1 || !failure.reconnect {
                            return Err(failure.error);
                        }
                        if failure.gateway_retry_after.is_some_and(|delay| {
                            delay >= deadline.saturating_duration_since(Instant::now())
                        }) {
                            return Err(failure.error);
                        }
                        tokio::time::sleep(failure.gateway_retry_after.unwrap_or(RECONNECT_DELAY))
                            .await;
                    }
                }
            }
            unreachable!("bounded count reconnect loop always returns")
        })
        .await
        .unwrap_or_else(|_| {
            Err(CatchupError(
                "timeout before COUNT; coverage incomplete".into(),
            ))
        });
        if result.is_err() {
            self.sockets.remove(relay);
        }
        result
    }

    async fn count_inner(
        &mut self,
        relay: &str,
        query: &CatchupQuery,
        budget: &mut QueryBudget,
    ) -> std::result::Result<Option<usize>, QueryFailure> {
        self.ensure_connected(relay).await?;
        self.sequence += 1;
        let id = format!("catchup-count-{}", self.sequence);
        let socket = self.sockets.get_mut(relay).expect("connected source");
        socket
            .send(Message::Text(
                serde_json::json!(["COUNT", id, {
                    "authors": [query.author], "kinds": query.kinds,
                    "since": query.since, "until": query.until,
                }])
                .to_string(),
            ))
            .await
            .map_err(|error| QueryFailure::transport("send COUNT", error))?;
        loop {
            let message = socket
                .next()
                .await
                .ok_or_else(|| QueryFailure {
                    error: CatchupError("socket ended before COUNT".into()),
                    reconnect: true,
                    gateway_retry_after: None,
                })?
                .map_err(|error| QueryFailure::transport("socket before COUNT", error))?;
            budget.messages += 1;
            if budget.messages > query.limit.saturating_mul(4).saturating_add(100) {
                return Err(
                    CatchupError("source message budget exhausted before COUNT".into()).into(),
                );
            }
            let raw = match message {
                Message::Text(text) => text,
                Message::Ping(payload) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|error| QueryFailure::transport("send COUNT PONG", error))?;
                    continue;
                }
                Message::Pong(_) => continue,
                Message::Close(frame) => {
                    return Err(QueryFailure {
                        error: CatchupError("socket closed before COUNT".into()),
                        reconnect: frame.as_ref().is_none_or(|frame| {
                            matches!(frame.code, CloseCode::Normal | CloseCode::Away)
                        }),
                        gateway_retry_after: None,
                    })
                }
                _ => {
                    return Err(
                        CatchupError("unexpected non-text message before COUNT".into()).into(),
                    )
                }
            };
            budget.bytes = budget.bytes.saturating_add(raw.len());
            if budget.bytes > self.max_bytes.saturating_add(4096) {
                return Err(
                    CatchupError("source byte budget exhausted before COUNT".into()).into(),
                );
            }
            let value: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|_| CatchupError("invalid COUNT response JSON".into()))?;
            let parts = value
                .as_array()
                .ok_or_else(|| CatchupError("COUNT response must be an array".into()))?;
            if parts.get(1).and_then(|part| part.as_str()) != Some(id.as_str()) {
                continue;
            }
            match parts.first().and_then(|part| part.as_str()) {
                Some("CLOSED") if parts.len() == 3 && parts[2].is_string() => return Ok(None),
                Some("COUNT") if parts.len() == 3 => {
                    let result = parts[2]
                        .as_object()
                        .ok_or_else(|| CatchupError("COUNT payload must be an object".into()))?;
                    let count = result
                        .get("count")
                        .and_then(|value| value.as_u64())
                        .filter(|count| *count <= (1u64 << 53) - 1)
                        .and_then(|count| usize::try_from(count).ok())
                        .ok_or_else(|| {
                            CatchupError("COUNT must be a nonnegative safe integer".into())
                        })?;
                    match result.get("approximate") {
                        None | Some(serde_json::Value::Bool(false)) => {}
                        Some(serde_json::Value::Bool(true)) => return Ok(None),
                        _ => {
                            return Err(
                                CatchupError("COUNT approximate must be boolean".into()).into()
                            )
                        }
                    }
                    if result.contains_key("hll") {
                        return Ok(None);
                    }
                    // This is relay-reported evidence, not a global completeness
                    // proof. NIP-45 COUNT is one-shot; it creates no subscription
                    // requiring CLOSE. No event query or event buffer is reset.
                    return Ok(Some(count));
                }
                _ => return Err(CatchupError("unexpected matching COUNT response".into()).into()),
            }
        }
    }
}
