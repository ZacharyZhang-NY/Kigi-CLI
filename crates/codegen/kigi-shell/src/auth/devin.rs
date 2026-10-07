use std::sync::Arc;

use anyhow::Context;
use kigi_models::OAuthConfig;

use super::AuthManager;
use super::flow::{AuthChannels, AuthUrlInfo, AuthUrlMode};
use super::model::KimiAuth;
use super::oauth_pkce;

const DEVIN_CALLBACK_PORT: u16 = 59653;
const DEVIN_CALLBACK_PATH: &str = "/callback";
const DEVIN_LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

fn devin_redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}{DEVIN_CALLBACK_PATH}")
}

fn build_devin_authorize_url(
    cfg: &OAuthConfig,
    redirect_uri: &str,
    pkce: &oauth_pkce::PkceCodes,
) -> String {
    let base = format!("{}{}", cfg.auth_host.trim_end_matches('/'), cfg.device_path);
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", &pkce.state)
        .append_pair("prompt", "select_account")
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256");
    format!("{base}?{}", serializer.finish())
}

fn devin_token_client() -> anyhow::Result<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build devin token client")?;
    let _ = CLIENT.set(client.clone());
    Ok(client)
}

const DEVIN_TOKEN_BODY_CAP: usize = 64 * 1024;

#[derive(serde::Deserialize)]
struct DevinTokenResponse {
    token: Option<String>,
}

fn validate_session_token(token: &str) -> anyhow::Result<String> {
    if token.trim().is_empty() {
        anyhow::bail!("Devin token exchange returned an empty token");
    }
    if token.chars().any(|c| c.is_control()) {
        anyhow::bail!("Devin token exchange returned a malformed token");
    }
    Ok(token.to_string())
}

async fn exchange_devin_token(
    cfg: &OAuthConfig,
    code: &str,
    pkce: &oauth_pkce::PkceCodes,
) -> anyhow::Result<String> {
    let url = format!("{}{}", cfg.token_host.trim_end_matches('/'), cfg.token_path);
    post_devin_token(&url, code, pkce).await
}

async fn post_devin_token(
    url: &str,
    code: &str,
    pkce: &oauth_pkce::PkceCodes,
) -> anyhow::Result<String> {
    use futures_util::StreamExt;
    tracing::info!("auth: exchanging code for session token (devin)");
    let resp = devin_token_client()?
        .post(url)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": pkce.verifier,
        }))
        .send()
        .await
        .context("devin token exchange request failed")?;
    let status = resp.status();
    if !status.is_success() {
        drop(resp);
        tracing::warn!(%status, "auth: devin token exchange failed");
        anyhow::bail!("Devin token exchange failed (HTTP {status})");
    }
    let mut body = Vec::new();
    let mut bytes = resp.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.context("devin token response read failed")?;
        if body.len() + chunk.len() > DEVIN_TOKEN_BODY_CAP {
            anyhow::bail!("devin token response exceeds the size cap");
        }
        body.extend_from_slice(&chunk);
    }
    let parsed: DevinTokenResponse = serde_json::from_slice(&body)
        .map_err(|_| anyhow::anyhow!("malformed devin token payload"))?;
    tracing::info!("auth: devin token exchange succeeded");
    validate_session_token(parsed.token.as_deref().unwrap_or_default())
}

fn jwt_expiry(token: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let exp = value.get("exp")?.as_i64()?;
    chrono::DateTime::from_timestamp(exp, 0)
}

pub(crate) async fn run_devin_login(
    cfg: &'static OAuthConfig,
    auth_manager: &Arc<AuthManager>,
    channels: &mut Option<AuthChannels>,
) -> anyhow::Result<(KimiAuth, bool)> {
    let pkce = oauth_pkce::generate_pkce_random_state();

    let listener = match bind_devin_listener(DEVIN_CALLBACK_PORT).await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::info!(
                "auth: devin callback port {DEVIN_CALLBACK_PORT} in use; binding ephemeral"
            );
            bind_devin_listener(0)
                .await
                .context("could not bind devin loopback")?
        }
        Err(e) => return Err(e).context("could not bind devin loopback"),
    };
    let port = listener
        .local_addr()
        .context("devin loopback local_addr failed")?
        .port();
    let redirect_uri = devin_redirect_uri(port);
    let authorize_url = build_devin_authorize_url(cfg, &redirect_uri, &pkce);

    let mut chans = channels.take();
    if let Some(tx) = chans.as_mut().and_then(|c| c.url_tx.take()) {
        let _ = tx.send(AuthUrlInfo {
            url: authorize_url.clone(),
            mode: AuthUrlMode::Device,
        });
        super::device_code::open_browser_detached(&authorize_url).await;
    } else {
        eprintln!();
        eprintln!("To sign in, open this URL in your browser:");
        eprintln!();
        eprintln!("  {authorize_url}");
        eprintln!();
        if !super::device_code::open_browser_detached(&authorize_url).await {
            eprintln!("  (Could not open the browser automatically — open the URL above.)");
            eprintln!();
        }
        eprintln!("Waiting for the sign-in to complete...");
    }

    let code = await_devin_code(&listener, &redirect_uri, &pkce, chans.as_mut()).await?;
    let token = exchange_devin_token(cfg, &code, &pkce).await?;
    let expires_at = jwt_expiry(&token);
    let expires_in = expires_at
        .map(|at| (at - chrono::Utc::now()).num_seconds())
        .filter(|secs| *secs > 0);
    let auth = auth_manager
        .update(KimiAuth {
            key: token,
            refresh_token: None,
            expires_at,
            expires_in,
            ..KimiAuth::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to save credentials: {e}"))?;
    Ok((auth, true))
}

async fn bind_devin_listener(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(("127.0.0.1", port)).await
}

fn parse_devin_callback(
    input: &str,
    redirect_uri: &str,
    expected_state: &str,
) -> anyhow::Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty paste");
    }
    let expected = url::Url::parse(redirect_uri).context("redirect uri is not a URL")?;
    let pasted =
        url::Url::parse(trimmed).context("pasted value is not this login's callback URL")?;
    if pasted.scheme() != expected.scheme()
        || pasted.host_str() != expected.host_str()
        || pasted.port_or_known_default() != expected.port_or_known_default()
        || pasted.path() != expected.path()
        || !pasted.username().is_empty()
        || pasted.password().is_some()
        || pasted.fragment().is_some()
    {
        anyhow::bail!("pasted value is not this login's callback URL");
    }
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    for (key, value) in pasted.query_pairs() {
        match key.as_ref() {
            "code" => {
                if code.is_some() || value.is_empty() {
                    anyhow::bail!("pasted callback carries a duplicate or empty code");
                }
                code = Some(value.into_owned());
            }
            "state" => {
                if state.is_some() || value.is_empty() {
                    anyhow::bail!("pasted callback carries a duplicate or empty state");
                }
                state = Some(value.into_owned());
            }
            "error" => anyhow::bail!("pasted callback reported an OAuth error"),
            _ => anyhow::bail!("pasted callback carries an unexpected parameter"),
        }
    }
    let code = code.ok_or_else(|| anyhow::anyhow!("pasted callback carried no code"))?;
    match state.as_deref() {
        Some(s) if s == expected_state => Ok(code),
        _ => anyhow::bail!("OAuth state mismatch — rejecting callback (CSRF guard)"),
    }
}

async fn await_devin_code(
    listener: &tokio::net::TcpListener,
    redirect_uri: &str,
    pkce: &oauth_pkce::PkceCodes,
    channels: Option<&mut AuthChannels>,
) -> anyhow::Result<String> {
    let outcome = tokio::time::timeout(DEVIN_LOGIN_TIMEOUT, async {
        match channels {
            Some(ch) => {
                tokio::select! {
                    code = oauth_pkce::await_loopback_code_on(
                        listener,
                        DEVIN_CALLBACK_PATH,
                        &pkce.state,
                    ) => code,
                    pasted = ch.code_rx.recv() => {
                        let pasted = pasted.ok_or_else(|| {
                            anyhow::anyhow!("auth code channel closed before a code arrived")
                        })?;
                        parse_devin_callback(&pasted, redirect_uri, &pkce.state)
                    }
                }
            }
            None => {
                oauth_pkce::await_loopback_code_on(listener, DEVIN_CALLBACK_PATH, &pkce.state).await
            }
        }
    })
    .await;
    match outcome {
        Ok(result) => result,
        Err(_elapsed) => anyhow::bail!("Devin sign-in timed out — run the login again"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_has_exact_devin_query() {
        let pkce = oauth_pkce::generate_pkce_random_state();
        let url = build_devin_authorize_url(
            &kigi_models::DEVIN_OAUTH_CONFIG,
            "http://127.0.0.1:59653/callback",
            &pkce,
        );
        let (base, query) = url.split_once('?').expect("query");
        assert_eq!(base, "https://app.devin.ai/auth/cli/continue");
        let params: std::collections::BTreeMap<String, String> =
            url::form_urlencoded::parse(query.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
        assert_eq!(
            params.keys().cloned().collect::<Vec<_>>(),
            vec![
                "code_challenge",
                "code_challenge_method",
                "prompt",
                "redirect_uri",
                "state",
            ],
            "exactly five params, nothing else"
        );
        assert_eq!(params["redirect_uri"], "http://127.0.0.1:59653/callback");
        assert_eq!(params["state"], pkce.state);
        assert_eq!(params["prompt"], "select_account");
        assert_eq!(params["code_challenge"], pkce.challenge);
        assert_eq!(params["code_challenge_method"], "S256");
        assert!(!url.contains(&pkce.verifier));
        assert!(!url.contains("code_verifier"));
        assert!(!url.contains("client_id"));
        assert!(!url.contains("response_type"));
        assert!(!url.contains("scope="));
    }

    #[test]
    fn pkce_s256_and_random_distinct_state() {
        use base64::Engine;
        use sha2::Digest;
        let a = oauth_pkce::generate_pkce_random_state();
        let b = oauth_pkce::generate_pkce_random_state();
        assert_ne!(a.state, b.state, "states must be fresh-random");
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.state, a.verifier, "state never doubles as verifier");
        let digest = sha2::Sha256::digest(a.verifier.as_bytes());
        assert_eq!(
            a.challenge,
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest),
            "challenge is S256 of the verifier"
        );
    }

    #[tokio::test]
    async fn listener_prebinds_ephemeral_port() {
        let listener = bind_devin_listener(0).await.expect("bind ephemeral");
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 0, "the bound port is real");
        assert_ne!(port, DEVIN_CALLBACK_PORT, "ephemeral is not the default");
    }

    #[test]
    fn manual_paste_requires_exact_callback_url() {
        let pkce = oauth_pkce::generate_pkce_random_state();
        let redirect = "http://127.0.0.1:59653/callback";
        let state = &pkce.state;

        let good = format!("{redirect}?code=the-code&state={state}");
        assert_eq!(
            parse_devin_callback(&good, redirect, state).expect("valid callback"),
            "the-code"
        );
        let pct = format!("{redirect}?code=abc%2Fdef&state={state}");
        assert_eq!(
            parse_devin_callback(&pct, redirect, state).expect("percent-decoded"),
            "abc/def"
        );

        for bad in [
            "".to_string(),
            "the-code".to_string(),
            format!("the-code#{state}"),
            format!("{redirect}?code=the-code&state=wrong"),
            format!("{redirect}?code=the-code"),
            format!("{redirect}?state={state}"),
            format!("{redirect}?code=&state={state}"),
            format!("{redirect}?code=c&state="),
            format!("{redirect}?code=c&code=d&state={state}"),
            format!("{redirect}?code=c&state={state}&state={state}"),
            format!("{redirect}?code=c&state={state}&error=access_denied"),
            format!("{redirect}?code=c&state={state}&extra=1"),
            format!("https://127.0.0.1:59653/callback?code=c&state={state}"),
            format!("http://localhost:59653/callback?code=c&state={state}"),
            format!("http://127.0.0.1:1111/callback?code=c&state={state}"),
            format!("http://127.0.0.1:59653/other?code=c&state={state}"),
            format!("http://user@127.0.0.1:59653/callback?code=c&state={state}"),
            format!("{redirect}?code=c&state={state}#frag"),
            "not a url".to_string(),
        ] {
            assert!(
                parse_devin_callback(&bad, redirect, state).is_err(),
                "must reject: {bad}"
            );
        }
    }

    #[tokio::test]
    async fn loopback_callback_strict() {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("client");
        let listener = bind_devin_listener(0).await.expect("bind");
        let port = listener.local_addr().unwrap().port();
        let pkce = oauth_pkce::generate_pkce_random_state();
        let state = pkce.state.clone();
        let code_fut = oauth_pkce::await_loopback_code_on(&listener, "/callback", &state);
        tokio::pin!(code_fut);

        tokio::select! {
            res = &mut code_fut => panic!("wrong path must not resolve: {res:?}"),
            resp = client.get(format!("http://127.0.0.1:{port}/nope")).send() => {
                assert_eq!(resp.expect("req").status(), 404);
            }
        }

        let (code_res, _) = tokio::join!(&mut code_fut, async {
            let resp = client
                .get(format!(
                    "http://127.0.0.1:{port}/callback?code=x&state=wrong"
                ))
                .send()
                .await
                .expect("req");
            assert_eq!(resp.status(), 400);
        });
        let err = code_res.expect_err("a wrong state must be rejected");
        assert!(err.to_string().contains("state"), "{err}");

        let listener = bind_devin_listener(0).await.expect("bind");
        let port = listener.local_addr().unwrap().port();
        let (code_res, _) = tokio::join!(
            oauth_pkce::await_loopback_code_on(&listener, "/callback", &state),
            async {
                let resp = client
                    .get(format!(
                        "http://127.0.0.1:{port}/callback?code=the-code&state={state}"
                    ))
                    .send()
                    .await
                    .expect("req");
                assert_eq!(resp.status(), 200);
            }
        );
        assert_eq!(code_res.expect("code"), "the-code");
    }

    #[test]
    fn jwt_expiry_parsing() {
        use base64::Engine;
        let exp = chrono::Utc::now().timestamp() + 3600;
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{{\"exp\":{exp}}}"));
        let jwt = format!("header.{payload}.sig");
        let at = jwt_expiry(&jwt).expect("exp parsed");
        assert_eq!(at.timestamp(), exp);
        assert!(jwt_expiry("opaque-session-token").is_none());
        assert!(jwt_expiry("a.b").is_none());
        let no_exp = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        assert!(jwt_expiry(&format!("h.{no_exp}.s")).is_none());
    }

    #[tokio::test]
    async fn token_exchange_json_and_no_secret_leak() {
        use axum::routing::post;
        use axum::{Json, Router};
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let seen_clone = seen.clone();
        let app = Router::new().route(
            "/token",
            post(move |Json(body): Json<serde_json::Value>| {
                let seen = seen_clone.clone();
                async move {
                    *seen.lock().unwrap() = Some(body);
                    Json(serde_json::json!({"token": "sess-xyz"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let pkce = oauth_pkce::PkceCodes {
            verifier: "verifier-secret".into(),
            challenge: "ch".into(),
            state: "st".into(),
        };
        let token = post_devin_token(&format!("http://{addr}/token"), "the-code", &pkce)
            .await
            .expect("exchange");
        assert_eq!(token, "sess-xyz");
        let body = seen.lock().unwrap().take().expect("request seen");
        let mut keys: Vec<String> = body.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["code", "code_verifier"]);
        assert_eq!(body["code"], "the-code");
        assert_eq!(body["code_verifier"], "verifier-secret");
    }

    #[tokio::test]
    async fn token_exchange_never_follows_redirects() {
        use axum::Router;
        use axum::response::Redirect;
        use axum::routing::post;
        let app = Router::new().route(
            "/token",
            post(|| async { Redirect::temporary("http://off-host.invalid/") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let pkce = oauth_pkce::PkceCodes {
            verifier: "v".into(),
            challenge: "c".into(),
            state: "s".into(),
        };
        let err = post_devin_token(&format!("http://{addr}/token"), "code", &pkce)
            .await
            .expect_err("302 is a failure, not a redirect to follow");
        assert!(!err.to_string().contains("verifier"));
        assert!(!err.to_string().contains("code"));
    }

    #[tokio::test]
    async fn token_exchange_errors_hide_secrets() {
        use axum::Json;
        use axum::Router;
        use axum::routing::post;
        let app = Router::new()
            .route(
                "/fail",
                post(|| async {
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error":"bad","echo":"the-code"})),
                    )
                }),
            )
            .route("/empty", post(|| async { Json(serde_json::json!({})) }))
            .route(
                "/malformed",
                post(|| async { "{\"token\":123456789987654321}" }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let pkce = oauth_pkce::PkceCodes {
            verifier: "verifier-secret".into(),
            challenge: "c".into(),
            state: "s".into(),
        };
        let err = post_devin_token(&format!("http://{addr}/fail"), "the-code", &pkce)
            .await
            .expect_err("400 fails");
        let msg = err.to_string();
        assert!(msg.contains("400"));
        assert!(!msg.contains("the-code"), "reflected code leaked: {msg}");
        assert!(!msg.contains("verifier-secret"), "verifier leaked: {msg}");
        let err = post_devin_token(&format!("http://{addr}/empty"), "the-code", &pkce)
            .await
            .expect_err("empty token fails");
        assert!(err.to_string().contains("empty token"));
        let err = post_devin_token(&format!("http://{addr}/malformed"), "the-code", &pkce)
            .await
            .expect_err("non-string token fails decode");
        let rendered = err.to_string();
        let chained = format!("{err:#}");
        for secret in ["123456789987654321", "the-code", "verifier-secret"] {
            assert!(!rendered.contains(secret), "decode error echoed {secret}");
            assert!(!chained.contains(secret), "error chain echoed {secret}");
        }
    }

    #[test]
    fn session_token_validation() {
        assert!(validate_session_token("").is_err());
        assert!(validate_session_token("   ").is_err());
        assert!(validate_session_token("ok-token").is_ok());
        assert!(validate_session_token("bad\ntoken").is_err());
        assert!(validate_session_token("bad\u{7}token").is_err());
    }
}
