//! Config-owned Codex catalog cache. No global source map or detached refresh tasks.
//! Empty source deliberately stays offline; explicit models.codex-catalog wins.
use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use tokio::{io::AsyncReadExt, sync::Mutex, time::Instant};
use tokio_util::sync::CancellationToken;

pub(crate) const EMBEDDED: &str = include_str!("codex_models_catalog.json");
const MAX_BYTES: usize = 8 << 20;
const REFRESH: Duration = Duration::from_secs(3 * 60 * 60);
const RETRY: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct State {
    data: Arc<str>,
    next_attempt: Option<Instant>,
}

/// One cache per parsed config; clones share it, replacement configs never do.
/// Holding the async mutex coalesces readers. Dropping a refresh future releases
/// it without marking the attempt complete, so cancellation cannot wedge refresh.
#[derive(Debug)]
pub(crate) struct Catalog {
    source: String,
    state: Mutex<State>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl PartialEq for Catalog {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Catalog {
    fn new(source: String) -> Self {
        Self {
            source,
            state: Mutex::new(State {
                data: Arc::from(EMBEDDED),
                next_attempt: None,
            }),
        }
    }

    pub(crate) fn parse(raw: &serde_yaml::Value) -> Result<Arc<Self>> {
        let value = raw
            .get("models")
            .and_then(|models| models.get("codex-catalog"));
        let source = match value {
            None | Some(serde_yaml::Value::Null) => String::new(),
            Some(serde_yaml::Value::String(source)) => source.clone(),
            Some(_) => anyhow::bail!("models.codex-catalog must be a string"),
        };
        if !source.is_empty() && !Path::new(&source).is_absolute() {
            catalog_url(&source)?;
        }
        Ok(Arc::new(Self::new(source)))
    }

    pub(crate) async fn snapshot(&self, cancel: &CancellationToken) -> Option<Arc<str>> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            data = self.refresh(cancel) => data,
        }
    }

    async fn refresh(&self, cancel: &CancellationToken) -> Option<Arc<str>> {
        let mut state = self.state.lock().await;
        if self.source.is_empty() || state.next_attempt.is_some_and(|next| Instant::now() < next) {
            return (!cancel.is_cancelled()).then(|| state.data.clone());
        }
        let result = tokio::time::timeout(FETCH_TIMEOUT, read_source(&self.source)).await;
        // No await between the lifetime check and publication. The cache belongs
        // only to this immutable config generation, never to a newer source.
        if cancel.is_cancelled() {
            return None;
        }
        match result {
            Ok(Ok(data)) => {
                state.data = Arc::from(data);
                state.next_attempt = Some(Instant::now() + REFRESH);
            }
            _ => {
                state.next_attempt = Some(Instant::now() + RETRY);
                // Do not expose URLs (which may contain signed query parameters).
                tracing::warn!("Codex catalog refresh failed; keeping last valid catalog");
            }
        }
        Some(state.data.clone())
    }
}

fn catalog_url(source: &str) -> Result<url::Url> {
    let url = url::Url::parse(source).context("invalid models.codex-catalog URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "models.codex-catalog must be an HTTP(S) URL or absolute path"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "catalog URL credentials are forbidden"
    );
    Ok(url)
}

async fn read_source(source: &str) -> Result<String> {
    let mut data = Vec::new();
    if Path::new(source).is_absolute() {
        // O_NONBLOCK avoids a FIFO blocking an uncancellable Tokio file open.
        // Check again after open to fence replacement between metadata and open.
        ensure!(
            tokio::fs::metadata(source).await?.is_file(),
            "catalog must be a regular file"
        );
        let mut options = tokio::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NONBLOCK);
        let file = options.open(source).await?;
        ensure!(
            file.metadata().await?.is_file(),
            "catalog must be a regular file"
        );
        file.take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut data)
            .await?;
    } else {
        let url = catalog_url(source)?;
        // Independent client: never reuse gateway headers, proxy credentials,
        // Home mTLS identity, or caller auth. Redirects cannot cross this boundary.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(FETCH_TIMEOUT)
            .build()?;
        let mut response = client.get(url).send().await?;
        ensure!(
            response.status() == reqwest::StatusCode::OK,
            "catalog HTTP failure"
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= MAX_BYTES as u64),
            "catalog too large"
        );
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                chunk.len() <= MAX_BYTES.saturating_sub(data.len()),
                "catalog too large"
            );
            data.extend_from_slice(&chunk);
        }
    }
    ensure!(data.len() <= MAX_BYTES, "catalog too large");
    let data = String::from_utf8(data)?;
    validate(&data)?;
    Ok(data)
}

/// Go registry ValidateCodexClientModelsJSON, not the permissive template parser.
fn validate(data: &str) -> Result<()> {
    let raw: Value = serde_json::from_str(data)?;
    let models = raw
        .get("models")
        .and_then(Value::as_array)
        .context("missing models")?;
    ensure!(!models.is_empty(), "empty models");
    let mut slugs = HashSet::new();
    for model in models {
        let text = |key: &str| -> Result<&str> {
            model
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .context("missing catalog string")
        };
        ensure!(slugs.insert(text("slug")?), "duplicate slug");
        for key in [
            "display_name",
            "description",
            "base_instructions",
            "minimal_client_version",
            "visibility",
        ] {
            text(key)?;
        }
        let integer = |key: &str| -> Result<f64> {
            let n = model
                .get(key)
                .and_then(Value::as_f64)
                .context("missing catalog integer")?;
            ensure!(
                n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n < 9223372036854775808.0,
                "invalid catalog integer"
            );
            Ok(n)
        };
        let context = integer("context_window")?;
        ensure!(
            context > 0.0 && context <= integer("max_context_window")?,
            "invalid context window"
        );
        integer("priority")?;
        let levels = model
            .get("supported_reasoning_levels")
            .and_then(Value::as_array)
            .context("missing reasoning levels")?;
        ensure!(!levels.is_empty(), "empty reasoning levels");
        let mut efforts = HashSet::new();
        for level in levels {
            let effort = level
                .get("effort")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .context("missing effort")?;
            ensure!(efforts.insert(effort), "duplicate effort");
        }
        ensure!(
            efforts.contains(text("default_reasoning_level")?),
            "invalid default effort"
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "codex_catalog_tests.rs"]
mod tests;
