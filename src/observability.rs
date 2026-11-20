//! Best-effort bounded Home log forwarding. No free-form tracing values or bodies cross this boundary.
use crate::home::{client::HomeClient, client::supervisor::Supervisor};
use serde::Serialize;
use std::sync::{
    Arc, OnceLock, Weak,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context};

#[path = "observability_request.rs"]
mod request;
pub use request::RequestLogRecord;

const REQUEST_CAPACITY: usize = 128;
const APP_CAPACITY: usize = 1024;
/// Hard cap on serialized JSON, independent of Home's 64 MiB RESP limit.
pub const MAX_PAYLOAD_BYTES: usize = 8192;
static SINK: OnceLock<Arc<Queue>> = OnceLock::new();

#[derive(Default)]
struct Counters {
    overflow: AtomicU64,
    unavailable: AtomicU64,
    failed: AtomicU64,
    sent: AtomicU64,
}

/// Process-wide best-effort counters; no log emission from the sender (avoids recursion).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LogStats {
    pub overflow: u64,
    pub unavailable: u64,
    pub failed: u64,
    pub sent: u64,
}

struct Delivery {
    owner: Weak<HomeClient>,
    payload: Vec<u8>,
}
struct Queue {
    request: mpsc::Sender<Delivery>,
    app: mpsc::Sender<Delivery>,
    supervisor: Weak<Supervisor>,
    counters: Counters,
    stop: CancellationToken,
}
impl Queue {
    fn submit(&self, owner: &Arc<HomeClient>, payload: Vec<u8>, app: bool) -> bool {
        if self.stop.is_cancelled() || !owner.ready() || payload.len() > MAX_PAYLOAD_BYTES {
            self.counters.unavailable.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.enqueue(
            Delivery {
                owner: Arc::downgrade(owner),
                payload,
            },
            app,
        )
    }

    fn enqueue(&self, delivery: Delivery, app: bool) -> bool {
        let sender = if app { &self.app } else { &self.request };
        match sender.try_send(delivery) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.counters.overflow.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.counters.unavailable.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }
}

/// Submit without network I/O, waiting, or spawning a task. False means disabled/dropped.
/// Install `LogForwarder` in the executable first. Config is rechecked on every submission.
pub fn submit(home: &Arc<HomeClient>, record: RequestLogRecord) -> bool {
    if !home.config().request_log {
        return false;
    }
    #[cfg(test)]
    if let Some(sender) = request_test_sinks()
        .lock()
        .unwrap()
        .get(&Arc::as_ptr(home).addr())
    {
        return sender.try_send(record.render()).is_ok();
    }
    let Some(queue) = SINK.get() else {
        return false;
    };
    match serde_json::to_vec(&record.render()) {
        Ok(payload) => queue.submit(home, payload, false),
        Err(_) => {
            queue.counters.failed.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

#[cfg(test)]
fn request_test_sinks()
-> &'static std::sync::Mutex<std::collections::HashMap<usize, mpsc::Sender<RequestLogRecord>>> {
    static SINKS: OnceLock<
        std::sync::Mutex<std::collections::HashMap<usize, mpsc::Sender<RequestLogRecord>>>,
    > = OnceLock::new();
    SINKS.get_or_init(Default::default)
}

#[cfg(test)]
pub(crate) fn capture_requests(home: &Arc<HomeClient>) -> mpsc::Receiver<RequestLogRecord> {
    let (tx, rx) = mpsc::channel(16);
    request_test_sinks()
        .lock()
        .unwrap()
        .insert(Arc::as_ptr(home).addr(), tx);
    rx
}

#[cfg(test)]
pub(crate) fn stop_capture_requests(home: &Arc<HomeClient>) {
    request_test_sinks()
        .lock()
        .unwrap()
        .remove(&Arc::as_ptr(home).addr());
}

pub struct LogForwarder {
    queue: Arc<Queue>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl LogForwarder {
    /// One process owner. Tracks supervisor replacements without rebinding queued records.
    pub fn install(supervisor: &Arc<Supervisor>) -> anyhow::Result<Self> {
        let (request, request_rx) = mpsc::channel(REQUEST_CAPACITY);
        let (app, app_rx) = mpsc::channel(APP_CAPACITY);
        let queue = Arc::new(Queue {
            request,
            app,
            supervisor: Arc::downgrade(supervisor),
            counters: Counters::default(),
            stop: CancellationToken::new(),
        });
        anyhow::ensure!(
            SINK.set(queue.clone()).is_ok(),
            "Home log forwarder already installed"
        );
        let tasks = vec![
            tokio::spawn(run(queue.clone(), request_rx, false)),
            tokio::spawn(run(queue.clone(), app_rx, true)),
        ];
        Ok(Self { queue, tasks })
    }

    pub fn stats(&self) -> LogStats {
        let c = &self.queue.counters;
        LogStats {
            overflow: c.overflow.load(Ordering::Relaxed),
            unavailable: c.unavailable.load(Ordering::Relaxed),
            failed: c.failed.load(Ordering::Relaxed),
            sent: c.sent.load(Ordering::Relaxed),
        }
    }

    /// Stop admission, drain queued deliveries within a single total deadline, then join.
    /// A fenced Home cannot accept logs; those records are intentionally discarded.
    pub async fn shutdown(mut self, bound: std::time::Duration) -> LogStats {
        self.queue.stop.cancel();
        let deadline = tokio::time::Instant::now() + bound;
        for task in &mut self.tasks {
            if tokio::time::timeout_at(deadline, &mut *task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
        self.stats()
    }
}
impl Drop for LogForwarder {
    fn drop(&mut self) {
        self.queue.stop.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn run(queue: Arc<Queue>, mut rx: mpsc::Receiver<Delivery>, app: bool) {
    let mut unsupported: Option<Weak<HomeClient>> = None;
    loop {
        let delivery = tokio::select! {
            biased;
            _ = queue.stop.cancelled(), if !rx.is_closed() => { rx.close(); continue; }
            delivery = rx.recv() => match delivery { Some(v) => v, None => break },
        };
        let Some(home) = delivery.owner.upgrade() else {
            queue.counters.unavailable.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if !home.ready()
            || unsupported
                .as_ref()
                .is_some_and(|old| old.ptr_eq(&delivery.owner))
        {
            queue.counters.unavailable.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        // No retries: a lost acknowledgement must not duplicate Home's persisted log.
        let cancel = home.cancellation();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow::anyhow!("Home membership ended")),
            result = async {
                if app { home.push_app_log(delivery.payload).await }
                else { home.push_request_log(delivery.payload).await }
            } => result,
        };
        match result {
            Ok(()) => {
                queue.counters.sent.fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => {
                // Inspect only for compatibility; never emit server error strings.
                if err.chain().any(|cause| {
                    let message = cause.to_string().to_ascii_lowercase();
                    ["unsupported key", "unknown command", "unsupported command"]
                        .iter()
                        .any(|s| message.contains(s))
                }) {
                    unsupported = Some(Arc::downgrade(&home));
                }
                queue.counters.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Privacy-first tracing adapter: static callsite identity and allowlisted numeric fields only.
/// `message`, errors, URLs, headers, bodies, and arbitrary string/debug fields are never formatted.
#[derive(Clone, Copy, Default)]
pub struct HomeLogLayer;
impl<S: Subscriber> Layer<S> for HomeLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let Some(queue) = SINK.get() else {
            return;
        };
        let Some(home) = queue.supervisor.upgrade().and_then(|s| s.current()) else {
            return;
        };
        if !home.ready() {
            return;
        }
        let meta = event.metadata();
        if !meta.target().starts_with("ai48proxy") {
            return;
        }
        let mut fields = SafeFields(String::new());
        event.record(&mut fields);
        let timestamp = chrono::Utc::now().to_rfc3339();
        let line = format!(
            "[{timestamp}] [{}] {} {}{}",
            meta.level(),
            meta.target(),
            meta.name(),
            fields.0
        );
        #[derive(Serialize)]
        struct AppLog {
            line: String,
            level: String,
            timestamp: String,
        }
        let payload = AppLog {
            line,
            level: meta.level().as_str().to_ascii_lowercase(),
            timestamp,
        };
        if let Ok(payload) = serde_json::to_vec(&payload) {
            queue.submit(&home, payload, true);
        }
    }
}
struct SafeFields(String);
impl Visit for SafeFields {
    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if matches!(
            field.name(),
            "status"
                | "upstream_status"
                | "latency_ms"
                | "retry_round"
                | "revision"
                | "delay_ms"
                | "seconds"
        ) {
            use std::fmt::Write;
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        if let Ok(value) = u64::try_from(value) {
            self.record_u64(field, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    #[test]
    fn tracing_does_not_format_free_form_values() {
        struct Secret;
        impl std::fmt::Debug for Secret {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("must not format");
            }
        }
        struct Check;
        impl<S: Subscriber> Layer<S> for Check {
            fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
                let mut fields = SafeFields(String::new());
                event.record(&mut fields);
                assert_eq!(fields.0, " status=502");
            }
        }
        let subscriber = tracing_subscriber::registry().with(Check);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(status = 502u64, token = ?Secret, body = "secret", "secret message");
        });
    }

    #[test]
    fn queues_are_bounded_without_waiting() {
        let (tx, _rx) = mpsc::channel(1);
        let (app, _app_rx) = mpsc::channel(1);
        let queue = Queue {
            request: tx,
            app,
            supervisor: Weak::new(),
            counters: Counters::default(),
            stop: CancellationToken::new(),
        };
        let delivery = || Delivery {
            owner: Weak::new(),
            payload: vec![0],
        };
        assert!(queue.enqueue(delivery(), false));
        assert!(!queue.enqueue(delivery(), false));
        assert_eq!(queue.counters.overflow.load(Ordering::Relaxed), 1);
        // App-log capacity is independent of the full request-log queue.
        assert!(queue.enqueue(delivery(), true));
    }
}
