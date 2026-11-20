pub(super) mod completion {
    use crate::{home::client::HomeClient, observability::RequestLogRecord};
    use axum::{
        body::{Body, BodyDataStream},
        response::Response,
    };
    use bytes::Bytes;
    use futures_util::Stream;
    use std::{
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU16, Ordering},
        },
        task::{Context, Poll},
        time::Instant,
    };

    tokio::task_local! { pub(in crate::gateway) static CONTEXT: Arc<RequestContext>; }

    #[derive(Default)]
    pub(in crate::gateway) struct RequestContext {
        provider: Mutex<String>,
        status: AtomicU16,
    }
    impl RequestContext {
        pub(in crate::gateway) fn selected(&self, provider: &str) {
            *self.provider.lock().unwrap_or_else(|e| e.into_inner()) = provider.to_owned();
        }
        pub(in crate::gateway) fn status(&self, status: u16) {
            self.status.store(status, Ordering::Release);
        }
    }

    pub(in crate::gateway) struct Completion {
        home: Arc<HomeClient>,
        record: Option<RequestLogRecord>,
        started: Instant,
        pub(in crate::gateway) context: Arc<RequestContext>,
    }
    impl Completion {
        pub(in crate::gateway) fn new(home: Arc<HomeClient>, record: RequestLogRecord) -> Self {
            Self {
                home,
                record: Some(record),
                started: Instant::now(),
                context: Arc::default(),
            }
        }
        fn finish(&mut self, status: u16) {
            if let Some(record) = self.record.take() {
                let provider = self
                    .context
                    .provider
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                crate::observability::submit(
                    &self.home,
                    record
                        .provider(&provider)
                        .finish(status, self.started.elapsed()),
                );
            }
        }
        pub(in crate::gateway) fn response(self, response: Response) -> Response {
            let (mut parts, body) = response.into_parts();
            parts.extensions.insert(self.context.clone());
            let status = parts.status.as_u16();
            Response::from_parts(
                parts,
                Body::from_stream(LoggedBody {
                    source: Some(body.into_data_stream()),
                    completion: self,
                    status,
                }),
            )
        }
    }
    impl Drop for Completion {
        fn drop(&mut self) {
            self.finish(499);
        }
    }
    struct LoggedBody {
        source: Option<BodyDataStream>,
        completion: Completion,
        status: u16,
    }
    impl Stream for LoggedBody {
        type Item = Result<Bytes, axum::Error>;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let Some(source) = self.source.as_mut() else {
                return Poll::Ready(None);
            };
            let next = Pin::new(source).poll_next(cx);
            match &next {
                Poll::Ready(None) => {
                    self.source.take();
                    let observed = self.completion.context.status.load(Ordering::Acquire);
                    let status = if observed == 0 { self.status } else { observed };
                    self.completion.finish(status);
                }
                Poll::Ready(Some(Err(_))) => {
                    self.source.take();
                    let observed = self.completion.context.status.load(Ordering::Acquire);
                    self.completion
                        .finish(if observed >= 400 { observed } else { 502 });
                }
                _ => {}
            }
            next
        }
    }
    impl Drop for LoggedBody {
        fn drop(&mut self) {
            // Cancel transport/body work before recording the disconnect.
            self.source.take();
        }
    }
}
use std::{sync::Arc, time::Instant};

use chrono::Utc;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::{
    execution::{AdmissionGuard, ExecutionGuard, ReleaseFrame},
    home::client::{HomeClient, Rejection, Selection},
    reporting::{EnqueueError, UsageRecord, usage_payload},
};

pub(super) struct Attempt {
    home: Arc<HomeClient>,
    guard: ExecutionGuard,
    pub record: UsageRecord,
    started: Instant,
    attribution: Value,
    context: Option<Arc<super::completion::RequestContext>>,
    pub(super) downstream_complete: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Attempt {
    pub fn cancellation(&self) -> tokio_util::sync::CancellationToken {
        self.guard.cancellation()
    }

    pub fn refreshed(&mut self, selection: &Selection) {
        let alias = self
            .attribution
            .get("alias")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        self.attribution = attribution(selection, &alias);
        self.record.auth_index = selection.auth_index.clone();
    }

    pub fn first_byte(&mut self) {
        if self.record.ttft_ms == 0 {
            self.record.ttft_ms = self.started.elapsed().as_millis().min(i64::MAX as u128) as i64;
        }
    }

    pub fn finish(&mut self) -> Option<ReleaseFrame> {
        self.guard.finish()
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if self
            .downstream_complete
            .as_ref()
            .is_some_and(|done| !done.load(std::sync::atomic::Ordering::Acquire))
            && self.record.status_code < 400
        {
            self.record.local_failure(499);
        }
        if let Some(context) = &self.context {
            context.status(self.record.status_code);
        }
        // Synchronous completion is safe even if the HTTP future/body is dropped.
        self.guard.finish();
        self.record.latency_ms = self.started.elapsed().as_millis().min(i64::MAX as u128) as i64;
        tracing::info!(
            request_id = %self.record.request_id,
            execution_id = %self.record.execution_id,
            provider = %self.record.provider,
            status = self.record.status_code,
            latency_ms = self.record.latency_ms,
            failed = self.record.failed,
            "Gateway execution completed"
        );
        let Ok(mut payload) = usage_payload(&self.record) else {
            self.home.fence();
            return;
        };
        if let (Some(payload), Some(attribution)) =
            (payload.as_object_mut(), self.attribution.as_object())
        {
            payload.extend(attribution.clone());
        }
        match self.home.reporting.enqueue(payload.clone()) {
            Ok(_) => {}
            Err(EnqueueError::Full) => {
                self.home.fence();
                let queue = self.home.reporting.clone();
                let bound = self.home.config().lifecycle.cancel_bound;
                tracing::warn!(
                    "Usage queue full; membership fenced, retaining event for bounded shutdown drain"
                );
                tokio::spawn(async move {
                    if !retain_for_drain(queue, payload, bound).await {
                        tracing::error!(
                            "Usage event could not be retained before shutdown deadline; durable reporting is required to prevent loss"
                        );
                    }
                });
            }
            Err(_) => self.home.fence(),
        }
    }
}

/// The queue remains bounded; the original immutable event is retained only for
/// the shutdown budget. This is not durable delivery across process termination.
pub(super) async fn retain_for_drain(
    queue: Arc<crate::reporting::ReportingQueue>,
    payload: Value,
    bound: std::time::Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + bound;
    loop {
        match queue.enqueue(payload.clone()) {
            Ok(_) => return true,
            Err(EnqueueError::Full) => {}
            Err(_) => return false,
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep_until(
            (tokio::time::Instant::now() + std::time::Duration::from_millis(25)).min(deadline),
        )
        .await;
    }
}

/// Home billing reads api_key (the authenticated downstream principal), not
/// user_api_key. Never substitute the upstream provider key or emit raw tokens.
pub(super) fn attribution(selection: &Selection, alias: &str) -> Value {
    let mut fields = serde_json::Map::new();
    fields.insert(
        "api_key".into(),
        Value::String(selection.user_api_key.clone()),
    );
    fields.insert("alias".into(), Value::String(alias.to_owned()));
    if let Some(token) = selection.auth.get("metadata").and_then(observed_token) {
        fields.insert(
            "access_token_sha256".into(),
            Value::String(hex::encode(Sha256::digest(token.as_bytes()))),
        );
    }
    Value::Object(fields)
}

fn observed_token(metadata: &Value) -> Option<&str> {
    for key in ["access_token", "accessToken"] {
        if let Some(token) = metadata
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(token);
        }
    }
    for key in ["token", "Token"] {
        if let Some(nested) = metadata.get(key).filter(|v| v.is_object()) {
            for key in ["access_token", "accessToken"] {
                if let Some(token) = nested
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    return Some(token);
                }
            }
        }
    }
    None
}

/// Only a typed rejection proves that no remote admission occurred.
pub(super) fn resolve_rejection(admission: &mut AdmissionGuard, err: &anyhow::Error) -> bool {
    if err.downcast_ref::<Rejection>().is_some() {
        admission.finish();
        true
    } else {
        false
    }
}

/// The spawned owner survives cancellation of the HTTP handler. The admission stays
/// pending until selection is guarded; a failed receiver drops the guarded result.
pub(super) async fn dispatch(
    home: Arc<HomeClient>,
    request: Value,
    request_id: String,
    stream: bool,
) -> anyhow::Result<(Selection, Attempt)> {
    // Capture the caller alias before moving the request into the dispatch transport.
    let alias = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let context = super::completion::CONTEXT.try_with(Arc::clone).ok();
    let (tx, rx) = oneshot::channel();
    // Register synchronously, before the task can be delayed by the scheduler.
    let admission = home.registry.admission();
    tokio::spawn(async move {
        let mut admission = admission;
        let result = match home.dispatch(request).await {
            Ok(selection) => {
                if let Some(context) = &context {
                    context.selected(&selection.provider);
                }
                let (credential, model, accounted) = match &selection.concurrency {
                    Some(c) => (c.credential_id.clone(), c.model.clone(), c.accounted),
                    None => (
                        selection
                            .auth
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or(&selection.auth_index)
                            .to_owned(),
                        selection.model.clone(),
                        false,
                    ),
                };
                let guard = admission.begin(
                    credential,
                    model,
                    accounted,
                    request_id.clone(),
                    if stream { "stream" } else { "request" }.into(),
                );
                let attempt = Attempt {
                    home: home.clone(),
                    guard,
                    started: Instant::now(),
                    attribution: attribution(&selection, &alias),
                    context,
                    downstream_complete: None,
                    record: UsageRecord {
                        execution_id: Uuid::new_v4().to_string(),
                        request_id,
                        provider: selection.provider.clone(),
                        executor_type: "ai48proxy".into(),
                        model: selection.model.clone(),
                        auth_index: selection.auth_index.clone(),
                        timestamp: Utc::now(),
                        stream,
                        failed: true,
                        status_code: 499,
                        // Selection alone is not evidence of an upstream credential failure.
                        // The response owner explicitly clears this upon upstream feedback.
                        fail_code: "request_scoped".into(),
                        ..Default::default()
                    },
                };
                Ok((selection, attempt))
            }
            Err(err) => {
                if !resolve_rejection(&mut admission, &err) {
                    // Unknown failures may have admitted remotely. Never retry this lifetime.
                    home.fence();
                }
                Err(err)
            }
        };
        let _ = tx.send(result);
    });
    rx.await
        .map_err(|_| anyhow::anyhow!("dispatch owner stopped"))?
}
