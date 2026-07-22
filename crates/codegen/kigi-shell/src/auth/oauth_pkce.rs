//! Generic authorization-code + PKCE (S256) OAuth wire with a `127.0.0.1`
//! loopback callback, driven by a registry [`kigi_models::OAuthConfig`] whose
//! `flow` is [`OAuthFlow::PkceLocalhost`] (claude-pro-max JSON + openai-codex
//! FORM).
//!
//! Shape (Pi `earendil-works/pi` `auth/oauth/{anthropic,openai-codex}.ts`):
//! - `verifier = base64url(32 random bytes)`; `challenge = base64url(SHA-256(
//!   verifier))`. `state` is `verifier` for claude ([`generate_pkce`]) or a
//!   fresh-random value for codex ([`generate_pkce_random_state`]).
//! - Browser opens `{auth_host}{device_path}?client_id&response_type=code&
//!   scope&redirect_uri&state&code_challenge&code_challenge_method=S256` plus any
//!   `authorize_extra` params (codex only).
//! - The code returns to a loopback listener on `127.0.0.1:{redirect_port}`
//!   answering ONLY `{redirect_path}` (claude `/callback`, codex
//!   `/auth/callback`), with STRICT `state` validation (a mismatch is rejected —
//!   CSRF guard). A manual paste (redirect URL / `code#state` / bare code) is
//!   accepted as a headless fallback.
//! - Code → token exchange POSTs `{token_host}{token_path}` as JSON
//!   ([`exchange_code`], claude) or FORM ([`exchange_code_form`], codex; NO
//!   `state` field). Refresh: claude JSON here ([`refresh_token`], rotating);
//!   codex takes the generic device refresher's FORM path.
//!
//! SECURITY: the verifier, authorization code, access token, and refresh token
//! are NEVER logged (only non-secret events: authorize URL requested, callback
//! received, token issued, token refreshed).

use anyhow::Context;
use base64::Engine;
use kigi_models::{OAuthConfig, OAuthTokenBody};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::kimi_oauth::{RefreshError, TokenResponse};
use super::model::KimiAuth;

const CODE_GRANT_TYPE: &str = "authorization_code";
const REFRESH_GRANT_TYPE: &str = "refresh_token";
/// Refresh retry budget over the retryable statuses / network blips.
const MAX_REFRESH_RETRIES: u32 = 3;
/// HTTP statuses worth retrying a refresh for (parity with the device wire).
const RETRYABLE_REFRESH_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];

/// PKCE secrets for one login attempt. Two `state` conventions ship:
/// [`generate_pkce`] sets `state == verifier` (Pi's/Claude's convention), while
/// [`generate_pkce_random_state`] mints an INDEPENDENT random state (the OAuth
/// standard, used by ChatGPT/Codex). Either way the state is validated on the
/// callback as the CSRF guard.
#[derive(Debug, Clone)]
pub(crate) struct PkceCodes {
    /// `code_verifier` — the 43-char base64url secret, sent at token exchange.
    pub verifier: String,
    /// `code_challenge = base64url(SHA-256(verifier))`, sent at authorize.
    pub challenge: String,
    /// `state` — the verifier itself, or an independent random value depending
    /// on the provider's dialect; validated on the callback (CSRF guard).
    pub state: String,
}

/// Generate PKCE S256 codes: `verifier = base64url(32 random bytes)`,
/// `challenge = base64url(SHA-256(verifier))`, `state = verifier`.
pub(crate) fn generate_pkce() -> PkceCodes {
    use rand::RngCore;
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    PkceCodes {
        state: verifier.clone(),
        verifier,
        challenge,
    }
}

/// Like [`generate_pkce`] but with an INDEPENDENT fresh-random `state` (16
/// random bytes) instead of `state == verifier`. The ChatGPT/Codex flow uses a
/// distinct state (the verifier never doubles as the CSRF token there), so the
/// verifier stays out of the state carried on the loopback callback.
pub(crate) fn generate_pkce_random_state() -> PkceCodes {
    use rand::RngCore;
    let mut raw = [0u8; 16];
    rand::rng().fill_bytes(&mut raw);
    let state = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    PkceCodes {
        state,
        ..generate_pkce()
    }
}

/// The loopback redirect URI for a PKCE-localhost provider (claude `/callback`,
/// codex `/auth/callback`).
pub(crate) fn redirect_uri(redirect_port: u16, redirect_path: &str) -> String {
    format!("http://localhost:{redirect_port}{redirect_path}")
}

/// Build the browser authorize URL:
/// `{auth_host}{device_path}?client_id&response_type=code&scope&redirect_uri&
/// state&code_challenge&code_challenge_method=S256` plus any config
/// `authorize_extra` params (empty for every config but codex, so their URLs
/// stay byte-identical).
pub(crate) fn build_authorize_url(
    cfg: &OAuthConfig,
    redirect_uri: &str,
    pkce: &PkceCodes,
) -> String {
    let base = format!("{}{}", cfg.auth_host.trim_end_matches('/'), cfg.device_path);
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer
        .append_pair("client_id", cfg.client_id)
        .append_pair("response_type", "code")
        .append_pair("scope", cfg.scope)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", &pkce.state)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256");
    for (key, value) in cfg.authorize_extra {
        serializer.append_pair(key, value);
    }
    format!("{base}?{}", serializer.finish())
}

/// `code` + `state` extracted from a callback (loopback query OR manual paste).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallbackParams {
    pub code: String,
    pub state: Option<String>,
}

/// Parse `code`/`state` from the raw query string of a `/callback?…` request
/// (e.g. `code=abc&state=xyz`). An `error=` param surfaces as an `Err`.
pub(crate) fn parse_callback_query(query: &str) -> anyhow::Result<CallbackParams> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        anyhow::bail!("Authorization server returned an error: {error}");
    }
    let code = code.context("callback missing authorization code")?;
    if code.is_empty() {
        anyhow::bail!("callback authorization code was empty");
    }
    Ok(CallbackParams { code, state })
}

/// Parse a MANUAL paste (headless fallback). Accepts, in order:
/// - a full redirect URL (`http://localhost:…/callback?code=…&state=…`),
/// - a `code#state` pair (Anthropic's console shows this form),
/// - a bare `code` (state then unknown → `None`, caller validation applies).
pub(crate) fn parse_manual_paste(input: &str) -> anyhow::Result<CallbackParams> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty paste");
    }
    // Full redirect URL.
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let url = url::Url::parse(trimmed).context("pasted value is not a valid URL")?;
        return parse_callback_query(url.query().unwrap_or_default());
    }
    // `code#state`.
    if let Some((code, state)) = trimmed.split_once('#') {
        if code.is_empty() {
            anyhow::bail!("pasted code was empty");
        }
        return Ok(CallbackParams {
            code: code.to_owned(),
            state: (!state.is_empty()).then(|| state.to_owned()),
        });
    }
    // Bare code.
    Ok(CallbackParams {
        code: trimmed.to_owned(),
        state: None,
    })
}

/// STRICT state validation (CSRF guard): the callback `state` MUST be present
/// AND equal to the expected value. A mismatch (or absence, when a paste has no
/// state) is rejected — the flow NEVER proceeds on an unverified callback.
pub(crate) fn validate_state(params: &CallbackParams, expected_state: &str) -> anyhow::Result<()> {
    match params.state.as_deref() {
        Some(state) if state == expected_state => Ok(()),
        Some(_) => anyhow::bail!("OAuth state mismatch — rejecting callback (CSRF guard)"),
        None => anyhow::bail!("OAuth callback carried no state — rejecting (CSRF guard)"),
    }
}

/// State validation for a MANUAL paste (headless fallback): a present state
/// MUST match (mismatch rejected — CSRF guard), but an ABSENT state is allowed
/// — a bare-code paste is user-initiated (not a network-reachable callback), so
/// there is no state to check. The loopback path uses the stricter
/// [`validate_state`] (an absent state there IS rejected).
pub(crate) fn validate_pasted_state(
    params: &CallbackParams,
    expected_state: &str,
) -> anyhow::Result<()> {
    match params.state.as_deref() {
        Some(state) if state == expected_state => Ok(()),
        Some(_) => anyhow::bail!("OAuth state mismatch — rejecting pasted code (CSRF guard)"),
        None => Ok(()),
    }
}

/// The token-endpoint URL (`{token_host}{token_path}`).
fn token_url(cfg: &OAuthConfig) -> String {
    format!("{}{}", cfg.token_host.trim_end_matches('/'), cfg.token_path)
}

/// POST the token endpoint with a JSON body, honoring `cfg.token_body`. Claude
/// is JSON; a `Form`-bodied config would be handled by the device wire, so the
/// PKCE path asserts JSON (never silently mis-encodes).
async fn post_token_json(
    cfg: &OAuthConfig,
    body: serde_json::Value,
) -> reqwest::Result<reqwest::Response> {
    debug_assert!(
        matches!(cfg.token_body, OAuthTokenBody::Json),
        "PKCE token exchange expects a JSON token body"
    );
    crate::http::shared_client()
        .post(token_url(cfg))
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
}

/// Exchange an authorization `code` for a token set (JSON body):
/// `{grant_type:"authorization_code", code, state, client_id, redirect_uri,
/// code_verifier}`. Returns the materialized [`KimiAuth`].
pub(crate) async fn exchange_code(
    cfg: &OAuthConfig,
    code: &str,
    pkce: &PkceCodes,
    redirect_uri: &str,
) -> anyhow::Result<KimiAuth> {
    let body = serde_json::json!({
        "grant_type": CODE_GRANT_TYPE,
        "code": code,
        "state": pkce.state,
        "client_id": cfg.client_id,
        "redirect_uri": redirect_uri,
        "code_verifier": pkce.verifier,
    });
    tracing::info!(
        scope_key = cfg.scope_key,
        "auth: exchanging code for token (pkce)"
    );
    let resp = post_token_json(cfg, body)
        .await
        .context("token exchange request failed")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(%status, scope_key = cfg.scope_key, "auth: code exchange failed (pkce)");
        anyhow::bail!("Token exchange failed (HTTP {status}): {body}");
    }
    let tokens: TokenResponse = resp.json().await.context("malformed token payload")?;
    tracing::info!(
        scope_key = cfg.scope_key,
        "auth: pkce code exchange succeeded"
    );
    Ok(tokens.into_auth())
}

/// Exchange an authorization `code` for a token set with a FORM body (codex):
/// `{grant_type=authorization_code, client_id, code, code_verifier,
/// redirect_uri}`. Unlike [`exchange_code`], the `state` is NOT sent in the
/// token body (the ChatGPT/Codex token endpoint does not expect it). Asserts a
/// `Form` config so a JSON provider can never silently mis-encode.
pub(crate) async fn exchange_code_form(
    cfg: &OAuthConfig,
    code: &str,
    pkce: &PkceCodes,
    redirect_uri: &str,
) -> anyhow::Result<KimiAuth> {
    debug_assert!(
        matches!(cfg.token_body, OAuthTokenBody::Form),
        "PKCE form exchange expects a Form token body"
    );
    tracing::info!(
        scope_key = cfg.scope_key,
        "auth: exchanging code for token (pkce form)"
    );
    let resp = crate::http::shared_client()
        .post(token_url(cfg))
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", CODE_GRANT_TYPE),
            ("client_id", cfg.client_id),
            ("code", code),
            ("code_verifier", pkce.verifier.as_str()),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await
        .context("token exchange request failed")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(%status, scope_key = cfg.scope_key, "auth: code exchange failed (pkce form)");
        anyhow::bail!("Token exchange failed (HTTP {status}): {body}");
    }
    let tokens: TokenResponse = resp.json().await.context("malformed token payload")?;
    tracing::info!(
        scope_key = cfg.scope_key,
        "auth: pkce form code exchange succeeded"
    );
    Ok(tokens.into_auth())
}

/// `POST {token_host}{token_path}` with `grant_type=refresh_token` (JSON body).
/// Claude ROTATES the refresh token, so the caller MUST persist the returned
/// one. Retries the retryable statuses / network errors with exponential
/// backoff; 401/403 returns immediately as [`RefreshError::Unauthorized`].
pub(crate) async fn refresh_token(
    cfg: &OAuthConfig,
    refresh_token: &str,
) -> Result<KimiAuth, RefreshError> {
    let mut last_error = String::from("no attempt made");
    for attempt in 0..MAX_REFRESH_RETRIES {
        if attempt > 0 {
            let backoff = std::time::Duration::from_secs(1 << (attempt - 1));
            tracing::warn!(
                attempt,
                backoff_secs = backoff.as_secs(),
                last_error = %last_error,
                "auth: retrying token refresh (pkce)"
            );
            tokio::time::sleep(backoff).await;
        }
        tracing::info!(
            attempt,
            scope_key = cfg.scope_key,
            "auth: token refresh attempt (pkce)"
        );
        let body = serde_json::json!({
            "grant_type": REFRESH_GRANT_TYPE,
            "client_id": cfg.client_id,
            "refresh_token": refresh_token,
        });
        let resp = match post_token_json(cfg, body).await {
            Ok(resp) => resp,
            Err(e) => {
                last_error = format!("network error: {e}");
                continue;
            }
        };
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.unwrap_or_default();
        if status == 401 || status == 403 {
            let err: OAuthErrorBody = serde_json::from_slice(&bytes).unwrap_or_default();
            return Err(RefreshError::Unauthorized {
                status,
                description: err
                    .error_description
                    .unwrap_or_else(|| "Token refresh unauthorized.".to_owned()),
            });
        }
        if status == 200 {
            return match serde_json::from_slice::<TokenResponse>(&bytes) {
                Ok(tokens) => Ok(tokens.into_auth()),
                Err(e) => Err(RefreshError::Fatal {
                    status,
                    description: format!("malformed token payload: {e}"),
                }),
            };
        }
        let err: OAuthErrorBody = serde_json::from_slice(&bytes).unwrap_or_default();
        let description = err
            .error_description
            .unwrap_or_else(|| format!("Token refresh failed (HTTP {status})."));
        if RETRYABLE_REFRESH_STATUSES.contains(&status) {
            last_error = description;
            continue;
        }
        return Err(RefreshError::Fatal {
            status,
            description,
        });
    }
    Err(RefreshError::Exhausted { last_error })
}

#[derive(Deserialize, Default)]
struct OAuthErrorBody {
    #[serde(default)]
    error_description: Option<String>,
}

/// Bind a loopback HTTP listener on `127.0.0.1:{redirect_port}` and wait for a
/// single `GET {redirect_path}?code=…&state=…`, validating `state` STRICTLY
/// against `expected_state` (mismatch → rejected). Returns the authorization
/// code.
///
/// The listener answers ONLY `redirect_path` (claude `/callback`, codex
/// `/auth/callback`); any other path gets 404. It binds `127.0.0.1` (never
/// `0.0.0.0`), so no non-loopback host can reach it.
pub(crate) async fn await_loopback_code(
    redirect_port: u16,
    redirect_path: &str,
    expected_state: &str,
) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", redirect_port))
        .await
        .with_context(|| format!("could not bind loopback 127.0.0.1:{redirect_port}"))?;
    tracing::info!(port = redirect_port, "auth: pkce loopback listener bound");
    loop {
        let (stream, _peer) = listener.accept().await.context("loopback accept failed")?;
        match handle_loopback_conn(stream, redirect_path, expected_state).await {
            LoopbackOutcome::Code(code) => return Ok(code),
            LoopbackOutcome::Rejected(err) => return Err(err),
            // Not the callback GET (favicon, health probe): keep listening.
            LoopbackOutcome::Ignore => continue,
        }
    }
}

enum LoopbackOutcome {
    Code(String),
    Rejected(anyhow::Error),
    Ignore,
}

/// Read the request line of one loopback connection, answer with a small HTML
/// page, and classify the outcome. STRICT: a `/callback` with a bad/missing
/// state is [`LoopbackOutcome::Rejected`] (the browser sees an error page).
async fn handle_loopback_conn(
    mut stream: tokio::net::TcpStream,
    redirect_path: &str,
    expected_state: &str,
) -> LoopbackOutcome {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Read only enough for the request line — a GET has no body.
    let mut buf = [0u8; 8192];
    let n = match stream.read(&mut buf).await {
        Ok(0) => return LoopbackOutcome::Ignore,
        Ok(n) => n,
        Err(_) => return LoopbackOutcome::Ignore,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    let Some(request_line) = head.lines().next() else {
        return LoopbackOutcome::Ignore;
    };
    // `GET {redirect_path}?code=…&state=… HTTP/1.1`
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return LoopbackOutcome::Ignore;
    };
    if method != "GET" {
        let _ = write_http(&mut stream, 405, "Method Not Allowed").await;
        return LoopbackOutcome::Ignore;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != redirect_path {
        let _ = write_http(&mut stream, 404, "Not Found").await;
        return LoopbackOutcome::Ignore;
    }

    let result = parse_callback_query(query)
        .and_then(|params| validate_state(&params, expected_state).map(|()| params.code));
    match result {
        Ok(code) => {
            let _ = write_http(
                &mut stream,
                200,
                "Signed in. You can close this window and return to kigi.",
            )
            .await;
            let _ = stream.flush().await;
            LoopbackOutcome::Code(code)
        }
        Err(e) => {
            let _ = write_http(&mut stream, 400, "Login failed — return to kigi and retry.").await;
            let _ = stream.flush().await;
            LoopbackOutcome::Rejected(e)
        }
    }
}

/// Write a minimal HTTP/1.1 response with an HTML body.
async fn write_http(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let body = format!("<!doctype html><meta charset=utf-8><p>{message}</p>");
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use kigi_models::CLAUDE_OAUTH_CONFIG;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A config pointed at a mock token host (copies Claude's client_id/scope/
    /// paths but overrides the token host).
    fn mock_cfg(token_host: &'static str) -> OAuthConfig {
        OAuthConfig {
            token_host,
            ..CLAUDE_OAUTH_CONFIG
        }
    }

    /// PKCE codes: verifier/challenge are non-empty base64url (no padding), the
    /// challenge is the base64url SHA-256 of the verifier, and state == verifier.
    #[test]
    fn generate_pkce_produces_valid_s256_codes() {
        let pkce = generate_pkce();
        assert_eq!(pkce.state, pkce.verifier, "state must equal the verifier");
        assert!(!pkce.verifier.is_empty() && !pkce.challenge.is_empty());
        for s in [&pkce.verifier, &pkce.challenge] {
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "base64url (no pad) only: {s}"
            );
            assert!(!s.contains('='), "no padding: {s}");
        }
        // challenge == base64url(SHA-256(verifier)).
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expect);
        // Fresh entropy each call.
        assert_ne!(pkce.verifier, generate_pkce().verifier);
    }

    /// The authorize URL carries the fixed params + the PKCE state and S256
    /// challenge, and targets `claude.ai/oauth/authorize`.
    #[test]
    fn authorize_url_has_state_and_s256_challenge() {
        let pkce = generate_pkce();
        let redirect = redirect_uri(53692, "/callback");
        let url = build_authorize_url(&CLAUDE_OAUTH_CONFIG, &redirect, &pkce);
        let parsed = url::Url::parse(&url).expect("valid URL");
        assert_eq!(parsed.host_str(), Some("claude.ai"));
        assert_eq!(parsed.path(), "/oauth/authorize");
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            q.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            q.get("state").map(String::as_str),
            Some(pkce.state.as_str())
        );
        assert_eq!(
            q.get("code_challenge").map(String::as_str),
            Some(pkce.challenge.as_str())
        );
        assert_eq!(
            q.get("client_id").map(String::as_str),
            Some(CLAUDE_OAUTH_CONFIG.client_id)
        );
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some(redirect.as_str())
        );
        // The verifier itself must NEVER appear in the browser URL.
        assert!(
            !url.contains("code_verifier"),
            "the verifier must not ride the authorize URL"
        );
    }

    /// STRICT state validation: an exact match passes; a mismatch or an absent
    /// state is REJECTED (CSRF guard — the flow must never proceed).
    #[test]
    fn state_validation_is_strict() {
        let ok = CallbackParams {
            code: "c".into(),
            state: Some("expected".into()),
        };
        assert!(validate_state(&ok, "expected").is_ok());
        let mismatch = CallbackParams {
            code: "c".into(),
            state: Some("attacker".into()),
        };
        assert!(
            validate_state(&mismatch, "expected").is_err(),
            "a state mismatch MUST be rejected"
        );
        let missing = CallbackParams {
            code: "c".into(),
            state: None,
        };
        assert!(
            validate_state(&missing, "expected").is_err(),
            "an absent state MUST be rejected"
        );
    }

    /// A loopback `/callback` with the WRONG state is rejected end-to-end (the
    /// listener returns an error, never a code) — the CSRF guard on the wire.
    #[tokio::test]
    async fn loopback_rejects_state_mismatch() {
        // Ephemeral port: bind, learn the port, then drive a client at it.
        let probe = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let server =
            tokio::spawn(
                async move { await_loopback_code(port, "/callback", "the-real-state").await },
            );
        // Give the listener a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Attacker callback: valid code, WRONG state.
        let _ = reqwest::get(format!(
            "http://127.0.0.1:{port}/callback?code=stolen&state=wrong-state"
        ))
        .await;
        let outcome = server.await.unwrap();
        let err = outcome.expect_err("a state mismatch must be rejected, never yield a code");
        assert!(err.to_string().contains("state mismatch"), "{err}");
    }

    /// A loopback `/callback` with the MATCHING state yields the code.
    #[tokio::test]
    async fn loopback_returns_code_on_valid_state() {
        let probe = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let server =
            tokio::spawn(async move { await_loopback_code(port, "/callback", "good-state").await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = reqwest::get(format!(
            "http://127.0.0.1:{port}/callback?code=auth-code-123&state=good-state"
        ))
        .await;
        let code = server.await.unwrap().expect("valid state yields the code");
        assert_eq!(code, "auth-code-123");
    }

    /// Manual-paste parsing: full redirect URL, `code#state`, and bare code.
    #[test]
    fn manual_paste_parses_all_three_forms() {
        let from_url =
            parse_manual_paste("http://localhost:53692/callback?code=abc123&state=st-9").unwrap();
        assert_eq!(from_url.code, "abc123");
        assert_eq!(from_url.state.as_deref(), Some("st-9"));

        let from_hash = parse_manual_paste("abc123#st-9").unwrap();
        assert_eq!(from_hash.code, "abc123");
        assert_eq!(from_hash.state.as_deref(), Some("st-9"));

        let bare = parse_manual_paste("  abc123  ").unwrap();
        assert_eq!(bare.code, "abc123");
        assert_eq!(bare.state, None);

        assert!(parse_manual_paste("").is_err());
        // A pasted redirect that carries an error param surfaces the error.
        assert!(parse_manual_paste("http://localhost/callback?error=access_denied").is_err());
    }

    /// Code → token exchange: JSON body carries the grant + verifier, response
    /// materializes a `KimiAuth` with the rotating refresh token.
    #[tokio::test]
    async fn exchange_code_posts_json_and_returns_auth() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_string_contains(
                "\"grant_type\":\"authorization_code\"",
            ))
            .and(body_string_contains("\"code\":\"auth-code-xyz\""))
            .and(body_string_contains("\"code_verifier\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "sk-ant-oat-new",
                "refresh_token": "sk-ant-ort-new",
                "expires_in": 3600,
                "token_type": "bearer",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let cfg = mock_cfg(host);
        let pkce = generate_pkce();
        let auth = exchange_code(
            &cfg,
            "auth-code-xyz",
            &pkce,
            &redirect_uri(53692, "/callback"),
        )
        .await
        .unwrap();
        assert_eq!(auth.key, "sk-ant-oat-new");
        assert_eq!(auth.refresh_token.as_deref(), Some("sk-ant-ort-new"));
        assert_eq!(auth.expires_in, Some(3600));
    }

    /// Refresh rotates the refresh token (JSON body, refresh grant).
    #[tokio::test]
    async fn refresh_rotates_refresh_token() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_string_contains("\"grant_type\":\"refresh_token\""))
            .and(body_string_contains("\"refresh_token\":\"ort-old\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "oat-fresh",
                "refresh_token": "ort-rotated",
                "expires_in": 3600,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let auth = refresh_token(&mock_cfg(host), "ort-old").await.unwrap();
        assert_eq!(auth.key, "oat-fresh");
        assert_eq!(
            auth.refresh_token.as_deref(),
            Some("ort-rotated"),
            "the rotated refresh token must be adopted"
        );
    }

    /// A 401 on refresh maps to Unauthorized (drives the permanent-failure path).
    #[tokio::test]
    async fn refresh_401_maps_to_unauthorized() {
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({ "error_description": "refresh revoked" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = refresh_token(&mock_cfg(host), "ort-dead")
            .await
            .unwrap_err();
        match err {
            RefreshError::Unauthorized {
                status,
                description,
            } => {
                assert_eq!(status, 401);
                assert_eq!(description, "refresh revoked");
            }
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    // ── ChatGPT/Codex PKCE (openai-codex) ────────────────────────────────────

    /// Codex PKCE uses an INDEPENDENT fresh-random state (NOT `state ==
    /// verifier`) so the verifier never rides the callback.
    #[test]
    fn codex_pkce_state_is_independent_of_the_verifier() {
        let pkce = generate_pkce_random_state();
        assert_ne!(
            pkce.state, pkce.verifier,
            "codex state must be fresh-random, not the verifier"
        );
        assert!(!pkce.state.is_empty() && !pkce.verifier.is_empty());
        // Challenge is still the S256 of the verifier.
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expect);
        assert_ne!(
            generate_pkce_random_state().state,
            pkce.state,
            "fresh state"
        );
    }

    /// The codex authorize URL carries the PKCE state + S256 challenge AND the
    /// three codex-only extra params, and targets `auth.openai.com/oauth/
    /// authorize` with the `/auth/callback` redirect. The verifier never rides it.
    #[test]
    fn codex_authorize_url_has_state_challenge_and_three_extra_params() {
        use kigi_models::CODEX_OAUTH_CONFIG;
        let pkce = generate_pkce_random_state();
        let redirect = redirect_uri(1455, "/auth/callback");
        assert_eq!(redirect, "http://localhost:1455/auth/callback");
        let url = build_authorize_url(&CODEX_OAUTH_CONFIG, &redirect, &pkce);
        let parsed = url::Url::parse(&url).expect("valid URL");
        assert_eq!(parsed.host_str(), Some("auth.openai.com"));
        assert_eq!(parsed.path(), "/oauth/authorize");
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(
            q.get("state").map(String::as_str),
            Some(pkce.state.as_str())
        );
        assert_eq!(
            q.get("code_challenge").map(String::as_str),
            Some(pkce.challenge.as_str())
        );
        assert_eq!(
            q.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        // The three codex-only extra params.
        assert_eq!(
            q.get("id_token_add_organizations").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            q.get("codex_cli_simplified_flow").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            q.get("originator").map(String::as_str),
            Some("codex_cli_rs")
        );
        assert!(
            !url.contains("code_verifier"),
            "the verifier must not ride the authorize URL"
        );
    }

    /// The claude authorize URL is UNCHANGED (no extra params) — its empty
    /// `authorize_extra` keeps it byte-identical.
    #[test]
    fn claude_authorize_url_carries_no_extra_params() {
        let pkce = generate_pkce();
        let url = build_authorize_url(
            &CLAUDE_OAUTH_CONFIG,
            &redirect_uri(53692, "/callback"),
            &pkce,
        );
        assert!(!url.contains("id_token_add_organizations"));
        assert!(!url.contains("codex_cli_simplified_flow"));
        assert!(!url.contains("originator"));
    }

    fn codex_mock_cfg(token_host: &'static str) -> OAuthConfig {
        OAuthConfig {
            token_host,
            ..kigi_models::CODEX_OAUTH_CONFIG
        }
    }

    /// Codex code→token exchange posts a FORM body carrying the grant + code +
    /// verifier + redirect_uri, and NOTABLY NO `state` field (the codex token
    /// endpoint does not expect it). Response materializes a `KimiAuth`.
    #[tokio::test]
    async fn codex_exchange_code_posts_form_without_state() {
        use wiremock::matchers::{body_string_contains, header, method, path};
        let server = MockServer::start().await;
        let host: &'static str = Box::leak(server.uri().into_boxed_str());
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=codex-auth-code"))
            .and(body_string_contains("code_verifier="))
            .and(body_string_contains("redirect_uri="))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "codex-access-jwt",
                "refresh_token": "codex-refresh",
                "expires_in": 3600,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let cfg = codex_mock_cfg(host);
        let pkce = generate_pkce_random_state();
        let auth = exchange_code_form(
            &cfg,
            "codex-auth-code",
            &pkce,
            "http://localhost:1455/auth/callback",
        )
        .await
        .unwrap();
        assert_eq!(auth.key, "codex-access-jwt");
        assert_eq!(auth.refresh_token.as_deref(), Some("codex-refresh"));
        assert_eq!(auth.expires_in, Some(3600));
    }
}
