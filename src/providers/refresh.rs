//! Home refresh delegation. No provider token endpoint is ever called here.
use super::{access_token, text};
use crate::home::client::{HomeClient, Selection};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub struct RefreshedAuth {
    pub auth: Value,
    pub auth_index: String,
}

/// Invoke only before downstream response bytes are committed; caller owns the
/// bounded retry and execution-release lifecycle. Replace selection.auth/index
/// with this result before preparing a retry. Never reuse the rejected builder.
pub async fn refresh(home: &HomeClient, selection: &Selection) -> Result<RefreshedAuth> {
    let request = refresh_request(selection)?;
    let bytes = home
        .command(vec![b"GET".to_vec(), serde_json::to_vec(&request)?])
        .await
        .map_err(|_| anyhow::anyhow!("Home refresh transport unavailable"))?
        .bytes()
        .map_err(|_| anyhow::anyhow!("invalid Home refresh frame"))?;
    let response: Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid Home refresh JSON"))?;
    if matches!(
        selection.provider.as_str(),
        "kimi" | "kimi-ai" | "meta" | "xai"
    ) {
        return parse_provider_refresh(response, selection);
    }
    parse_refresh(response, &selection.auth_index)
}

/// Status gate only, not an automatic replay loop. 403/429 do not rotate tokens.
pub async fn refresh_on_401(
    home: &HomeClient,
    selection: &Selection,
    status: reqwest::StatusCode,
) -> Result<Option<RefreshedAuth>> {
    if status != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    // Meta's minted key is refreshable only when Home retains its DCA token.
    if selection.provider == "meta"
        && (!text(&selection.auth, "/metadata/dca_token").is_empty()
            || !text(&selection.auth, "/attributes/dca_token").is_empty())
    {
        return Ok(Some(refresh(home, selection).await?));
    }
    // API-key selections have no refreshable token; do not trigger OAuth refresh.
    if !text(&selection.auth, "/attributes/api_key").is_empty() {
        return Ok(None);
    }
    Ok(Some(refresh(home, selection).await?))
}

pub(super) fn refresh_request(selection: &Selection) -> Result<Value> {
    ensure!(
        !selection.auth_index.trim().is_empty(),
        "Home refresh auth_index missing"
    );
    let token = match selection.provider.as_str() {
        "kimi" | "kimi-ai" => super::kimi::token(&selection.auth),
        "meta" => super::meta::token(&selection.auth),
        _ => access_token(&selection.auth),
    };
    ensure!(
        !token.is_empty(),
        "Home refresh requires observed access token"
    );
    Ok(
        json!({"type":"refresh", "auth_index":selection.auth_index.trim(),
        "access_token_sha256":hex::encode(Sha256::digest(token.as_bytes()))}),
    )
}

fn parse_provider_refresh(response: Value, selection: &Selection) -> Result<RefreshedAuth> {
    ensure!(
        response.is_object() && response.get("error").is_none(),
        "Home credential refresh rejected"
    );
    let index = text(&response, "/auth_index").to_owned();
    let auth = response.get("auth").cloned().unwrap_or(response);
    let token = match selection.provider.as_str() {
        "meta" => super::meta::token(&auth),
        "kimi" | "kimi-ai" => super::kimi::token(&auth),
        _ => access_token(&auth),
    };
    ensure!(
        auth.is_object() && !token.is_empty(),
        "Home returned invalid refreshed credential"
    );
    ensure!(
        auth.get("disabled").and_then(Value::as_bool) != Some(true)
            && text(&auth, "/status") != "disabled",
        "Home refreshed credential disabled"
    );
    Ok(RefreshedAuth {
        auth,
        auth_index: if index.is_empty() {
            selection.auth_index.clone()
        } else {
            index
        },
    })
}

pub(super) fn parse_refresh(response: Value, previous_index: &str) -> Result<RefreshedAuth> {
    ensure!(
        response.is_object() && response.get("error").is_none(),
        "Home credential refresh rejected"
    );
    let (auth, index) = if response.get("auth").is_some() {
        let index = text(&response, "/auth_index");
        (
            response["auth"].clone(),
            if index.is_empty() {
                previous_index
            } else {
                index
            }
            .to_owned(),
        )
    } else {
        (response, previous_index.to_owned())
    };
    ensure!(
        auth.is_object() && !access_token(&auth).is_empty(),
        "Home returned invalid refreshed credential"
    );
    ensure!(
        auth.get("disabled").and_then(Value::as_bool) != Some(true)
            && text(&auth, "/status") != "disabled",
        "Home refreshed credential disabled"
    );
    Ok(RefreshedAuth {
        auth,
        auth_index: index,
    })
}
