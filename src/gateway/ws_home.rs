//! Home admission is socket-scoped; usage is response-scoped.
use super::super::{auth_headers, lifecycle, session, upstream::Format, usage::Usage};
use crate::{
    execution::ExecutionGuard,
    home::client::{HomeClient, Selection},
    reporting::{EnqueueError, UsageRecord, usage_payload},
};
use anyhow::{Result, ensure};
use axum::http::HeaderMap;
use chrono::Utc;
use serde_json::{Value, json};
use std::{sync::Arc, time::Instant};
use tokio::sync::oneshot;
use uuid::Uuid;

pub(super) fn dispatch_request(headers: &HeaderMap, body: &Value) -> Value {
    let mut request = json!({"type":"auth", "model":body["model"], "count":1,
        "concurrency_protocol":1, "retry_round":0, "excluded_auth_ids":[],
        "headers":auth_headers(headers), "query":{}});
    session::dispatch_fields(headers, body, &mut request);
    request
}

pub(super) async fn dispatch(
    home: Arc<HomeClient>,
    request: Value,
) -> Result<(Selection, ExecutionGuard)> {
    let admission = home.registry.admission();
    let (tx, rx) = oneshot::channel();
    // This owner settles admission even when the downstream disappears while RPOP runs.
    tokio::spawn(async move {
        let mut admission = admission;
        let result = match home.dispatch(request).await {
            Ok(selection) => {
                let (id, model, accounted) = selection
                    .concurrency
                    .as_ref()
                    .map(|c| (c.credential_id.clone(), c.model.clone(), c.accounted))
                    .unwrap_or_else(|| {
                        (
                            selection.auth["id"].as_str().unwrap_or("").into(),
                            selection.model.clone(),
                            false,
                        )
                    });
                let guard = admission.begin(
                    id,
                    model,
                    accounted,
                    Uuid::new_v4().to_string(),
                    "stream".into(),
                );
                Ok((selection, guard))
            }
            Err(error) => {
                if !lifecycle::resolve_rejection(&mut admission, &error) {
                    home.fence();
                }
                Err(error)
            }
        };
        let _ = tx.send(result);
    });
    rx.await
        .map_err(|_| anyhow::anyhow!("admission owner stopped"))?
}

pub(super) fn principal(value: &Value, expected: Option<&str>) -> Result<String> {
    ensure!(
        value.get("authenticated").and_then(Value::as_bool) == Some(true)
            && value.get("error").is_none(),
        "authentication failed"
    );
    let principal = value
        .get("principal")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| anyhow::anyhow!("authentication principal missing"))?;
    ensure!(
        expected.is_none_or(|expected| expected == principal),
        "authentication principal changed"
    );
    Ok(principal.to_owned())
}

pub(super) async fn authorize(
    home: &HomeClient,
    headers: &HeaderMap,
    expected: Option<&str>,
    model: Option<&str>,
) -> Result<String> {
    let request = json!({"type":"auth-validate", "headers":auth_headers(headers), "query":{}});
    let bytes = home
        .command(vec![b"RPOP".to_vec(), serde_json::to_vec(&request)?])
        .await?
        .bytes()?;
    let value: Value = serde_json::from_slice(&bytes)?;
    let principal = principal(&value, expected)?;
    if let Some(model) = model {
        // Recheck Home's scoped model view without acquiring another concurrency slot.
        let request = json!({"type":"models", "headers":auth_headers(headers), "query":{}});
        let bytes = home
            .command(vec![b"GET".to_vec(), serde_json::to_vec(&request)?])
            .await?
            .bytes()?;
        let models: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            models.get("error").is_none()
                && models
                    .as_object()
                    .is_some_and(|buckets| buckets.values().any(|entries| {
                        entries.as_array().is_some_and(|entries| {
                            entries
                                .iter()
                                .any(|entry| entry.get("id").and_then(Value::as_str) == Some(model))
                        })
                    })),
            "model access denied"
        );
    }
    Ok(principal)
}

pub(super) struct Turn {
    home: Arc<HomeClient>,
    pub record: UsageRecord,
    pub usage: Usage,
    started: Instant,
    attribution: Value,
}
impl Turn {
    pub fn new(home: Arc<HomeClient>, selection: &Selection, alias: &str) -> Self {
        Self {
            home,
            started: Instant::now(),
            attribution: lifecycle::attribution(selection, alias),
            usage: Usage::new(Format::OpenAi),
            record: UsageRecord {
                execution_id: Uuid::new_v4().to_string(),
                request_id: Uuid::new_v4().to_string(),
                provider: selection.provider.clone(),
                executor_type: "ai48proxy".into(),
                model: selection.model.clone(),
                auth_index: selection.auth_index.clone(),
                timestamp: Utc::now(),
                stream: true,
                failed: true,
                status_code: 499,
                ..Default::default()
            },
        }
    }
    pub fn event(&mut self, event: &Value) {
        if self.record.ttft_ms == 0 {
            self.record.ttft_ms = self.started.elapsed().as_millis().min(i64::MAX as u128) as i64;
        }
        self.usage.json(event);
    }
}
impl Drop for Turn {
    fn drop(&mut self) {
        self.record.latency_ms = self.started.elapsed().as_millis().min(i64::MAX as u128) as i64;
        self.record.breakdown = self.usage.breakdown();
        let Ok(mut payload) = usage_payload(&self.record) else {
            self.home.fence();
            return;
        };
        if let (Some(fields), Some(attribution)) =
            (payload.as_object_mut(), self.attribution.as_object())
        {
            fields.extend(attribution.clone());
        }
        match self.home.reporting.enqueue(payload.clone()) {
            Ok(_) => {}
            Err(EnqueueError::Full) => {
                self.home.fence();
                let queue = self.home.reporting.clone();
                let bound = self.home.config().lifecycle.cancel_bound;
                tokio::spawn(async move {
                    if !lifecycle::retain_for_drain(queue, payload, bound).await {
                        tracing::error!(
                            "WebSocket usage could not be retained before shutdown deadline"
                        );
                    }
                });
            }
            Err(_) => self.home.fence(),
        }
    }
}
