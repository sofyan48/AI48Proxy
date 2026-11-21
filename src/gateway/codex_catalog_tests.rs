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
// Yield without advancing paused time: local HTTP must complete before the next tick.
async fn published(cache: &Catalog, expected: &str) {
    for _ in 0..10000 {
        if &*cache.state.lock().await.data == expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("catalog was not published");
}
async fn settled(cache: &Catalog) {
    loop {
        if let Ok(guard) = cache.refresh_lock.try_lock() {
            drop(guard);
            return;
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn codex_catalog_refresh_cache_and_last_valid_fallback() {
    let server = server("first").await;
    let cfg = config(&server.url);
    assert!(cfg.unsupported_features().is_empty());
    let cache = &cfg.providers.codex_catalog;
    let cancel = CancellationToken::new();
    assert_eq!(&*cache.snapshot(&cancel).await.unwrap(), EMBEDDED);
    assert_eq!(
        server.mock.hits.load(Ordering::SeqCst),
        0,
        "snapshot is read-only"
    );
    cache.refresh(&cancel, &[]).await;
    let first = cache.snapshot(&cancel).await.unwrap();
    assert_eq!(&*first, catalog("first"));
    assert!(Arc::ptr_eq(&first, &cache.snapshot(&cancel).await.unwrap()));
    for body in [
        "not JSON".to_owned(),
        "{\"models\":[]}".into(),
        "{\"models\":[{\"slug\":\"bad\"}]}".into(),
        "x".repeat(MAX_BYTES + 1),
    ] {
        *server.mock.body.lock().unwrap() = body;
        cache.refresh(&cancel, &[]).await;
        assert!(Arc::ptr_eq(&first, &cache.snapshot(&cancel).await.unwrap()));
    }
    *server.mock.status.lock().unwrap() = Some(StatusCode::FOUND);
    cache.refresh(&cancel, &[]).await;
    assert!(Arc::ptr_eq(&first, &cache.snapshot(&cancel).await.unwrap()));
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

#[tokio::test(start_paused = true)]
async fn codex_catalog_automatic_initial_and_periodic_refresh() {
    let server = server("initial").await;
    let cfg = config(&server.url);
    let cancel = CancellationToken::new();
    let cache = &cfg.providers.codex_catalog;
    let mut updater = Updater::start(cache.clone(), &cancel);
    published(cache, &catalog("initial")).await;
    tokio::task::yield_now().await;
    *server.mock.body.lock().unwrap() = catalog("periodic");
    tokio::time::advance(REFRESH - Duration::from_secs(1)).await;
    assert_eq!(server.mock.hits.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    published(cache, &catalog("periodic")).await;
    assert_eq!(server.mock.hits.load(Ordering::SeqCst), 2);
    updater.stop().await;
}

#[tokio::test(start_paused = true)]
async fn codex_catalog_failure_retries_at_go_interval() {
    let server = server("bad").await;
    *server.mock.status.lock().unwrap() = Some(StatusCode::INTERNAL_SERVER_ERROR);
    let cache = config(&server.url).providers.codex_catalog;
    let cancel = CancellationToken::new();
    let mut updater = Updater::start(cache.clone(), &cancel);
    server.mock.entered.notified().await;
    settled(&cache).await;
    tokio::task::yield_now().await;
    *server.mock.status.lock().unwrap() = None;
    *server.mock.body.lock().unwrap() = catalog("recovered");
    tokio::time::advance(REFRESH - Duration::from_secs(1)).await;
    assert_eq!(server.mock.hits.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    published(&cache, &catalog("recovered")).await;
    updater.stop().await;
}

#[tokio::test(start_paused = true)]
async fn codex_catalog_config_reload_and_shutdown_cancel_inflight() {
    let old = server("old").await;
    old.mock.block.store(true, Ordering::SeqCst);
    let cache = config(&old.url).providers.codex_catalog;
    let cancel = CancellationToken::new();
    let mut updater = Updater::start(cache.clone(), &cancel);
    old.mock.entered.notified().await;
    let new = server("new").await;
    let mut next = config(&new.url).providers.codex_catalog;
    updater.replace(&mut next, &cancel).await;
    assert!(
        cache.refresh_lock.try_lock().is_ok(),
        "old request was dropped"
    );
    old.mock.release.notify_one();
    published(&next, &catalog("new")).await;
    assert_eq!(&*cache.state.lock().await.data, EMBEDDED);
    let mut unchanged = config(&new.url).providers.codex_catalog;
    updater.replace(&mut unchanged, &cancel).await;
    assert!(Arc::ptr_eq(&next, &unchanged));
    new.mock.block.store(true, Ordering::SeqCst);
    // Consume the startup notification before waiting for the periodic request.
    new.mock.entered.notified().await;
    tokio::time::advance(REFRESH).await;
    new.mock.entered.notified().await;
    cancel.cancel();
    updater.stop().await;
    assert!(next.refresh_lock.try_lock().is_ok());
    assert_eq!(&*next.state.lock().await.data, catalog("new"));
    tokio::time::advance(REFRESH * 2).await;
    assert_eq!(old.mock.hits.load(Ordering::SeqCst), 1);
    assert_eq!(new.mock.hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn codex_catalog_reload_keeps_last_valid_and_drop_aborts() {
    let first = server("valid").await;
    let cache = config(&first.url).providers.codex_catalog;
    let cancel = CancellationToken::new();
    let mut updater = Updater::start(cache.clone(), &cancel);
    published(&cache, &catalog("valid")).await;
    let bad = server("bad").await;
    bad.mock.block.store(true, Ordering::SeqCst);
    let mut next = config(&bad.url).providers.codex_catalog;
    updater.replace(&mut next, &cancel).await;
    bad.mock.entered.notified().await;
    assert_eq!(&*next.snapshot(&cancel).await.unwrap(), catalog("valid"));
    drop(updater);
    settled(&next).await;
    bad.mock.release.notify_one();
    assert_eq!(&*next.snapshot(&cancel).await.unwrap(), catalog("valid"));
}

#[tokio::test]
async fn codex_catalog_public_default_order_fallback_and_explicit_precedence() {
    let primary = server("primary").await;
    let secondary = server("secondary").await;
    *primary.mock.body.lock().unwrap() = "invalid".into();
    let defaults = vec![primary.url.clone(), secondary.url.clone()];
    let cache = config("").providers.codex_catalog;
    let cancel = CancellationToken::new();
    let mut updater = Updater::start_with_defaults(cache.clone(), &cancel, defaults.clone());
    published(&cache, &catalog("secondary")).await;
    updater.stop().await;
    assert_eq!(primary.mock.hits.load(Ordering::SeqCst), 1);
    assert_eq!(secondary.mock.hits.load(Ordering::SeqCst), 1);
    *primary.mock.body.lock().unwrap() = catalog("primary");
    cache.refresh(&cancel, &defaults).await;
    assert_eq!(&*cache.snapshot(&cancel).await.unwrap(), catalog("primary"));
    assert_eq!(secondary.mock.hits.load(Ordering::SeqCst), 1);
    let explicit = config(&secondary.url).providers.codex_catalog;
    explicit.refresh(&cancel, &defaults).await;
    assert_eq!(primary.mock.hits.load(Ordering::SeqCst), 2);
    assert_eq!(
        &*explicit.snapshot(&cancel).await.unwrap(),
        catalog("secondary")
    );
}

#[tokio::test]
async fn codex_catalog_cancellation_releases_singleflight_without_publication() {
    let server = server("cancelled").await;
    server.mock.block.store(true, Ordering::SeqCst);
    let cache = config(&server.url).providers.codex_catalog;
    let cancel = CancellationToken::new();
    let fetch = cache.refresh(&cancel, &[]);
    tokio::pin!(fetch);
    tokio::select! {
        _ = server.mock.entered.notified() => {},
        _ = &mut fetch => panic!("fetch should block"),
    }
    cancel.cancel();
    fetch.await;
    assert!(cache.refresh_lock.try_lock().is_ok());
    assert_eq!(&*cache.state.lock().await.data, EMBEDDED);
    assert!(cache.snapshot(&cancel).await.is_none());
}

#[tokio::test]
async fn codex_catalog_local_file_refresh_cadence_and_offline_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let cache = config(path.to_str().unwrap()).providers.codex_catalog;
    let cancel = CancellationToken::new();
    cache.refresh(&cancel, &[]).await;
    assert_eq!(&*cache.snapshot(&cancel).await.unwrap(), EMBEDDED);
    std::fs::write(&path, catalog("file")).unwrap();
    cache.refresh(&cancel, &[]).await;
    assert_eq!(&*cache.snapshot(&cancel).await.unwrap(), catalog("file"));
    std::fs::remove_file(path).unwrap();
    cache.refresh(&cancel, &[]).await;
    assert_eq!(&*cache.snapshot(&cancel).await.unwrap(), catalog("file"));
}

#[tokio::test(start_paused = true)]
async fn codex_catalog_stalled_http_is_bounded_and_falls_back() {
    let server = server("never delivered").await;
    server.mock.block.store(true, Ordering::SeqCst);
    let cache = config(&server.url).providers.codex_catalog;
    let cancel = CancellationToken::new();
    let worker_cache = cache.clone();
    let worker_cancel = cancel.clone();
    let fetch = tokio::spawn(async move { worker_cache.refresh(&worker_cancel, &[]).await });
    for _ in 0..10000 {
        if server.mock.hits.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(server.mock.hits.load(Ordering::SeqCst), 1);
    tokio::time::advance(FETCH_TIMEOUT).await;
    fetch.await.unwrap();
    assert_eq!(&*cache.snapshot(&cancel).await.unwrap(), EMBEDDED);
}

#[test]
fn codex_catalog_config_validation_and_precedence() {
    validate(EMBEDDED).unwrap();
    assert_eq!(
        DEFAULT_URLS,
        [
            "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/codex_client_models.json",
            "https://models.router-for.me/codex_client_models.json",
        ]
    );
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
    let explicit = Catalog::parse(&raw).unwrap();
    assert_eq!(explicit.sources(&[]), ["https://explicit.test/catalog"]);
    for raw in [
        "{}",
        "models: {codex-catalog: ''}",
        "models: {catalog: 'https://unused.test/catalog'}",
        "home: {enabled: true}",
    ] {
        let cache = Catalog::parse(&serde_yaml::from_str(raw).unwrap()).unwrap();
        let defaults = DEFAULT_URLS.map(str::to_owned);
        assert_eq!(cache.sources(&defaults), DEFAULT_URLS);
    }
}
