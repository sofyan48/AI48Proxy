use super::*;
use axum::{
    Router,
    extract::State as AxumState,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
};
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

#[derive(Default)]
struct Mock {
    body: StdMutex<String>,
    status: StdMutex<Option<StatusCode>>,
    hits: AtomicUsize,
    headers: StdMutex<Vec<HeaderMap>>,
    entered: Notify,
    release: Notify,
    block: std::sync::atomic::AtomicBool,
}

struct Server {
    url: String,
    mock: Arc<Mock>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(AxumState(mock): AxumState<Arc<Mock>>, headers: HeaderMap) -> impl IntoResponse {
    mock.hits.fetch_add(1, Ordering::SeqCst);
    mock.headers.lock().unwrap().push(headers);
    mock.entered.notify_one();
    if mock.block.load(Ordering::SeqCst) {
        mock.release.notified().await;
    }
    let status = mock.status.lock().unwrap().unwrap_or(StatusCode::OK);
    let body = mock.body.lock().unwrap().clone();
    (
        status,
        [("location", "http://127.0.0.1:1/never-follow")],
        body,
    )
}

async fn server(label: &str) -> Server {
    let mock = Arc::new(Mock::default());
    *mock.body.lock().unwrap() = catalog(label);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/catalog", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/catalog", get(serve))
        .with_state(mock.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Server { url, mock, task }
}

fn catalog(label: &str) -> String {
    let mut raw: Value = serde_json::from_str(EMBEDDED).unwrap();
    raw["models"].as_array_mut().unwrap().truncate(1);
    raw["models"][0]["slug"] = "gpt-5.5".into();
    raw["models"][0]["description"] = label.into();
    serde_json::to_string(&raw).unwrap()
}

fn config(source: &str) -> crate::config::Config {
    let source = serde_json::to_string(source).unwrap();
    crate::config::Config::parse(format!("credential-concurrency: {{lifecycle-config-revision: 7}}\nmodels:\n  codex-catalog: {source}\n").as_bytes()).unwrap()
}

async fn due(cache: &Catalog) {
    cache.state.lock().await.next_attempt = None;
}

#[tokio::test]
async fn codex_catalog_refresh_cache_and_last_valid_fallback() {
    let server = server("first").await;
    let cfg = config(&server.url);
    assert!(
        cfg.unsupported_features().is_empty(),
        "{:?}",
        cfg.unsupported_features()
    );
    let cache = &cfg.providers.codex_catalog;
    let cancel = CancellationToken::new();
    let first = cache.snapshot(&cancel).await.unwrap();
    assert_eq!(&*first, catalog("first"));
    *server.mock.body.lock().unwrap() = catalog("second");
    assert!(Arc::ptr_eq(&first, &cache.snapshot(&cancel).await.unwrap()));
    assert_eq!(server.mock.hits.load(Ordering::SeqCst), 1);
    due(cache).await;
    let second = cache.snapshot(&cancel).await.unwrap();
    assert_eq!(&*second, catalog("second"));
    for body in [
        "not JSON".to_owned(),
        "{\"models\":[]}".into(),
        "{\"models\":[{\"slug\":\"bad\"}]}".into(),
        "x".repeat(MAX_BYTES + 1),
    ] {
        *server.mock.body.lock().unwrap() = body;
        due(cache).await;
        assert!(Arc::ptr_eq(
            &second,
            &cache.snapshot(&cancel).await.unwrap()
        ));
        let hits = server.mock.hits.load(Ordering::SeqCst);
        cache.snapshot(&cancel).await.unwrap();
        assert_eq!(
            server.mock.hits.load(Ordering::SeqCst),
            hits,
            "failure backoff"
        );
    }
    *server.mock.status.lock().unwrap() = Some(StatusCode::FOUND);
    due(cache).await;
    assert!(Arc::ptr_eq(
        &second,
        &cache.snapshot(&cancel).await.unwrap()
    ));
    for headers in server.mock.headers.lock().unwrap().iter() {
        for key in [
            "authorization",
            "proxy-authorization",
            "x-api-key",
            "cookie",
        ] {
            assert!(!headers.contains_key(key), "leaked {key}");
        }
    }
}

#[tokio::test]
async fn codex_catalog_config_reload_isolates_inflight_generations() {
    let old = server("old").await;
    old.mock.block.store(true, Ordering::SeqCst);
    let cfg_old = config(&old.url);
    let cancel = CancellationToken::new();
    let old_fetch = cfg_old.providers.codex_catalog.snapshot(&cancel);
    tokio::pin!(old_fetch);
    tokio::select! {
        _ = old.mock.entered.notified() => {},
        _ = &mut old_fetch => panic!("old fetch should block"),
    }
    let new = server("new").await;
    let cfg_new = config(&new.url);
    let fresh = cfg_new
        .providers
        .codex_catalog
        .snapshot(&cancel)
        .await
        .unwrap();
    assert_eq!(&*fresh, catalog("new"));
    old.mock.release.notify_one();
    assert_eq!(&*old_fetch.await.unwrap(), catalog("old"));
    assert!(Arc::ptr_eq(
        &fresh,
        &cfg_new
            .providers
            .codex_catalog
            .snapshot(&cancel)
            .await
            .unwrap()
    ));
    let cleared = config("");
    assert_eq!(
        &*cleared
            .providers
            .codex_catalog
            .snapshot(&cancel)
            .await
            .unwrap(),
        EMBEDDED
    );
    // Even an unrelated config replacement intentionally invalidates the cache.
    let same_source = config(&new.url);
    assert!(!Arc::ptr_eq(
        &cfg_new.providers.codex_catalog,
        &same_source.providers.codex_catalog
    ));
}

#[tokio::test]
async fn codex_catalog_cancellation_releases_singleflight_without_publication() {
    let server = server("cancelled").await;
    server.mock.block.store(true, Ordering::SeqCst);
    let cfg = config(&server.url);
    let cache = &cfg.providers.codex_catalog;
    let cancel = CancellationToken::new();
    let fetch = cache.snapshot(&cancel);
    tokio::pin!(fetch);
    tokio::select! {
        _ = server.mock.entered.notified() => {},
        _ = &mut fetch => panic!("fetch should block"),
    }
    cancel.cancel();
    assert!(fetch.await.is_none());
    assert_eq!(&*cache.state.lock().await.data, EMBEDDED);
    server.mock.block.store(false, Ordering::SeqCst);
    server.mock.release.notify_one();
    assert_eq!(
        &*cache.snapshot(&CancellationToken::new()).await.unwrap(),
        catalog("cancelled")
    );
    assert!(cache.snapshot(&cancel).await.is_none());
}

#[tokio::test]
async fn codex_catalog_dropped_caller_and_singleflight() {
    let server = server("shared").await;
    server.mock.block.store(true, Ordering::SeqCst);
    let cfg = config(&server.url);
    let cache = &cfg.providers.codex_catalog;
    let cancel = CancellationToken::new();
    {
        let fetch = cache.snapshot(&cancel);
        tokio::pin!(fetch);
        tokio::select! {
            _ = server.mock.entered.notified() => {},
            _ = &mut fetch => panic!("fetch should block"),
        }
    }
    assert!(
        cache.state.try_lock().is_ok(),
        "dropped caller releases mutex"
    );
    server.mock.block.store(false, Ordering::SeqCst);
    server.mock.release.notify_one();
    let hits = server.mock.hits.load(Ordering::SeqCst);
    let (a, b) = tokio::join!(cache.snapshot(&cancel), cache.snapshot(&cancel));
    assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
    assert_eq!(server.mock.hits.load(Ordering::SeqCst), hits + 1);
}

#[tokio::test]
async fn codex_catalog_local_file_refresh_cadence_and_offline_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let cfg = config(path.to_str().unwrap());
    let cache = &cfg.providers.codex_catalog;
    let cancel = CancellationToken::new();
    assert_eq!(&*cache.snapshot(&cancel).await.unwrap(), EMBEDDED);
    std::fs::write(&path, catalog("file")).unwrap();
    due(cache).await;
    assert_eq!(&*cache.snapshot(&cancel).await.unwrap(), catalog("file"));
    std::fs::write(&path, catalog("changed")).unwrap();
    assert_eq!(&*cache.snapshot(&cancel).await.unwrap(), catalog("file"));
    due(cache).await;
    assert_eq!(&*cache.snapshot(&cancel).await.unwrap(), catalog("changed"));
    std::fs::remove_file(path).unwrap();
    due(cache).await;
    assert_eq!(&*cache.snapshot(&cancel).await.unwrap(), catalog("changed"));
}

#[tokio::test(start_paused = true)]
async fn codex_catalog_stalled_http_is_bounded_and_falls_back() {
    let server = server("never delivered").await;
    server.mock.block.store(true, Ordering::SeqCst);
    let cfg = config(&server.url);
    let cache = &cfg.providers.codex_catalog;
    let cancel = CancellationToken::new();
    let fetch = cache.snapshot(&cancel);
    tokio::pin!(fetch);
    tokio::select! {
        _ = server.mock.entered.notified() => {},
        _ = &mut fetch => panic!("fetch should block"),
    }
    tokio::time::advance(FETCH_TIMEOUT).await;
    assert_eq!(&*fetch.await.unwrap(), EMBEDDED);
    assert!(cache.state.lock().await.next_attempt.is_some());
}

#[tokio::test(start_paused = true)]
async fn codex_catalog_deadlines_use_monotonic_time() {
    let cache = Catalog::default();
    let now = Instant::now();
    cache.state.lock().await.next_attempt = Some(now + REFRESH);
    tokio::time::advance(REFRESH - Duration::from_secs(1)).await;
    assert!(Instant::now() < cache.state.lock().await.next_attempt.unwrap());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(Instant::now() >= cache.state.lock().await.next_attempt.unwrap());
    cache.state.lock().await.next_attempt = Some(Instant::now() + RETRY);
    tokio::time::advance(RETRY).await;
    assert!(Instant::now() >= cache.state.lock().await.next_attempt.unwrap());
}

#[test]
fn codex_catalog_config_validation_and_precedence() {
    validate(EMBEDDED).unwrap();
    for source in [
        "relative.json",
        "file:///tmp/catalog",
        "ftp://example.test/catalog",
        "https://user:secret@example.test/catalog",
        "embed",
        "disabled",
    ] {
        let raw = serde_yaml::from_str(&format!("models: {{codex-catalog: {source:?}}}")).unwrap();
        assert!(Catalog::parse(&raw).is_err(), "{source}");
    }
    for raw in ["models: {codex-catalog: 42}", "models: {codex-catalog: []}"] {
        assert!(Catalog::parse(&serde_yaml::from_str(raw).unwrap()).is_err());
    }
    let raw = serde_yaml::from_str("models: {catalog: 'https://unused.test/catalog', codex-catalog: 'https://explicit.test/catalog'}").unwrap();
    assert_eq!(
        Catalog::parse(&raw).unwrap().source,
        "https://explicit.test/catalog"
    );
    let raw = serde_yaml::from_str("models: {catalog: 'https://unused.test/catalog'}").unwrap();
    assert!(Catalog::parse(&raw).unwrap().source.is_empty());
}
