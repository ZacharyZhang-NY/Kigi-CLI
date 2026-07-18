//! `kigi/billing` extension handler — Kimi Code usage/quota.
//!
//! Port of kimi-cli's `/usage` command (kimi-cli `src/kimi_cli/ui/shell/usage.py`):
//! `GET {coding_api_base_url}/usages` with the OAuth Bearer token, parsed into
//! display rows (`{usage: {...}, limits: [{detail, window, ...}]}` payload
//! shape). The TUI renders the rows as label + remaining-quota bar +
//! reset hint. The xAI credits/auto-topup surface this file used to serve is
//! gone with the xAI proxy.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};

use super::{ExtResult, to_raw_response};
use crate::agent::MvpAgent;

/// One usage row: a named quota with `used`/`limit` and an optional
/// human-readable reset hint (e.g. "resets in 2h 5m").
///
/// `Deserialize` because the TUI parses this back out of the
/// `kigi/billing` ext response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    pub label: String,
    pub used: i64,
    pub limit: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_hint: Option<String>,
}

/// Response for `kigi/billing`: the parsed usage rows, in display order
/// (summary row first when the payload carries one).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageResponse {
    pub rows: Vec<UsageRow>,
}

/// Error from the usages fetch, mapped to the same user-facing messages
/// kimi-cli shows (usage.py error handling).
#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("Authorization failed. Please check your credentials.")]
    Unauthorized,
    #[error("Usage endpoint not available. Try Kimi for Coding.")]
    NotFound,
    #[error("Failed to fetch usage (HTTP {status}).")]
    Http { status: u16 },
    #[error("Failed to fetch usage: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Failed to parse usage response: {0}")]
    Parse(#[from] serde_json::Error),
}

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "kigi/billing" => {
            tracing::info!("handling usage request");
            handle_get_usage(agent).await
        }
        _ => Err(acp::Error::method_not_found()),
    }
}

async fn handle_get_usage(agent: &MvpAgent) -> ExtResult {
    let auth = super::auth_gate::require_xai_auth(
        &agent.auth_manager,
        "Authentication required to fetch usage data",
        "Usage data requires a Kimi Code subscription session. Run `kigi login` to authenticate.",
    )?;

    let base = agent.cfg.borrow().endpoints.proxy_url();
    let usage = fetch_usage(&crate::http::shared_client(), &base, &auth.key)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "usage fetch failed");
            kigi_log::unified_log::warn(
                "usage: fetch failed",
                None,
                Some(serde_json::json!({ "error": e.to_string() })),
            );
            acp::Error::internal_error().data(e.to_string())
        })?;

    kigi_log::unified_log::info(
        "usage: fetched quota rows",
        None,
        serde_json::to_value(&usage).ok(),
    );

    to_raw_response(&usage)
}

/// `GET {base}/usages` with a Bearer token, parsed per kimi-cli usage.py.
pub(crate) async fn fetch_usage(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<UsageResponse, UsageError> {
    let url = format!("{}/usages", base_url.trim_end_matches('/'));
    let response = http
        .get(&url)
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    match response.status().as_u16() {
        200..=299 => {}
        401 => return Err(UsageError::Unauthorized),
        404 => return Err(UsageError::NotFound),
        status => return Err(UsageError::Http { status }),
    }
    let payload: serde_json::Value = serde_json::from_str(&response.text().await?)?;
    Ok(parse_usage_payload(&payload))
}

/// Port of usage.py `_parse_usage_payload`: `usage` (summary) + `limits[]`.
fn parse_usage_payload(payload: &serde_json::Value) -> UsageResponse {
    let mut rows = Vec::new();

    if let Some(usage) = payload.get("usage").filter(|v| v.is_object())
        && let Some(row) = to_usage_row(usage, "Weekly limit")
    {
        rows.push(row);
    }

    if let Some(limits) = payload.get("limits").and_then(|v| v.as_array()) {
        for (idx, item) in limits.iter().enumerate() {
            if !item.is_object() {
                continue;
            }
            let detail = match item.get("detail") {
                Some(d) if d.is_object() => d,
                _ => item,
            };
            let empty = serde_json::json!({});
            let window = match item.get("window") {
                Some(w) if w.is_object() => w,
                _ => &empty,
            };
            let label = limit_label(item, detail, window, idx);
            if let Some(row) = to_usage_row(detail, &label) {
                rows.push(row);
            }
        }
    }

    UsageResponse { rows }
}

/// Port of usage.py `_to_usage_row`: `used`/`limit`, with
/// `used = limit - remaining` fallback; row dropped when both absent.
fn to_usage_row(data: &serde_json::Value, default_label: &str) -> Option<UsageRow> {
    let limit = to_int(data.get("limit"));
    let used = to_int(data.get("used")).or_else(|| match (to_int(data.get("remaining")), limit) {
        (Some(remaining), Some(limit)) => Some(limit - remaining),
        _ => None,
    });
    if used.is_none() && limit.is_none() {
        return None;
    }
    let label = data
        .get("name")
        .and_then(non_empty_str)
        .or_else(|| data.get("title").and_then(non_empty_str))
        .map(str::to_owned)
        .unwrap_or_else(|| default_label.to_owned());
    Some(UsageRow {
        label,
        used: used.unwrap_or(0),
        limit: limit.unwrap_or(0),
        reset_hint: reset_hint(data),
    })
}

/// Port of usage.py `_limit_label`: name/title/scope, else the window
/// duration ("5h limit"), else "Limit #N".
fn limit_label(
    item: &serde_json::Value,
    detail: &serde_json::Value,
    window: &serde_json::Value,
    idx: usize,
) -> String {
    for key in ["name", "title", "scope"] {
        if let Some(val) = item
            .get(key)
            .and_then(non_empty_str)
            .or_else(|| detail.get(key).and_then(non_empty_str))
        {
            return val.to_owned();
        }
    }

    let duration = to_int(window.get("duration"))
        .or_else(|| to_int(item.get("duration")))
        .or_else(|| to_int(detail.get("duration")));
    let time_unit = window
        .get("timeUnit")
        .and_then(non_empty_str)
        .or_else(|| item.get("timeUnit").and_then(non_empty_str))
        .or_else(|| detail.get("timeUnit").and_then(non_empty_str))
        .unwrap_or("");
    if let Some(duration) = duration.filter(|&d| d != 0) {
        if time_unit.contains("MINUTE") {
            if duration >= 60 && duration % 60 == 0 {
                return format!("{}h limit", duration / 60);
            }
            return format!("{duration}m limit");
        }
        if time_unit.contains("HOUR") {
            return format!("{duration}h limit");
        }
        if time_unit.contains("DAY") {
            return format!("{duration}d limit");
        }
        return format!("{duration}s limit");
    }

    format!("Limit #{}", idx + 1)
}

/// Port of usage.py `_reset_hint`: absolute reset keys first, then
/// seconds-until keys.
fn reset_hint(data: &serde_json::Value) -> Option<String> {
    for key in ["reset_at", "resetAt", "reset_time", "resetTime"] {
        if let Some(val) = data.get(key).and_then(non_empty_str) {
            return Some(format_reset_time(val));
        }
    }
    for key in ["reset_in", "resetIn", "ttl", "window"] {
        if let Some(seconds) = to_int(data.get(key)).filter(|&s| s != 0) {
            return Some(format!(
                "resets in {}",
                format_duration(seconds.max(0) as u64)
            ));
        }
    }
    None
}

/// Port of usage.py `_format_reset_time`: ISO timestamp → "resets in …" /
/// "reset" (already past) / "resets at <raw>" when unparseable.
fn format_reset_time(val: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(val) {
        Ok(dt) => {
            let delta = dt.with_timezone(&chrono::Utc) - chrono::Utc::now();
            let seconds = delta.num_seconds();
            if seconds <= 0 {
                "reset".to_owned()
            } else {
                format!("resets in {}", format_duration(seconds as u64))
            }
        }
        Err(_) => format!("resets at {val}"),
    }
}

/// Port of kimi-cli `utils/datetime.py` `format_duration`: short units,
/// seconds shown only for sub-minute durations.
fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if secs > 0 && parts.is_empty() {
        parts.push(format!("{secs}s"));
    }
    if parts.is_empty() {
        "0s".to_owned()
    } else {
        parts.join(" ")
    }
}

fn non_empty_str(v: &serde_json::Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

/// Port of usage.py `_to_int`: ints and int-shaped floats/strings; anything
/// else is `None`.
fn to_int(value: Option<&serde_json::Value>) -> Option<i64> {
    let value = value?;
    if let Some(i) = value.as_i64() {
        return Some(i);
    }
    if let Some(f) = value.as_f64() {
        return Some(f as i64);
    }
    value.as_str()?.trim().parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{bearer_token, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Happy path: GET /usages with Bearer, kimi payload shape → rows with
    /// summary first, remaining-derived `used`, and window-derived labels.
    #[tokio::test]
    async fn fetch_usage_parses_kimi_payload() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/usages"))
            .and(bearer_token("tok-42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "usage": { "limit": 1000, "used": 250, "reset_at": "2099-01-01T00:00:00Z" },
                "limits": [
                    {
                        "window": { "duration": 300, "timeUnit": "TIME_UNIT_MINUTE" },
                        "detail": { "limit": 50, "remaining": 30, "resetIn": 1800 }
                    },
                    { "name": "RPM", "limit": 60, "used": 12 }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let usage = fetch_usage(&reqwest::Client::new(), &server.uri(), "tok-42")
            .await
            .expect("usage fetch should succeed");

        assert_eq!(usage.rows.len(), 3);
        assert_eq!(usage.rows[0].label, "Weekly limit");
        assert_eq!(usage.rows[0].used, 250);
        assert_eq!(usage.rows[0].limit, 1000);
        assert!(
            usage.rows[0]
                .reset_hint
                .as_deref()
                .is_some_and(|h| h.starts_with("resets in")),
            "absolute reset_at renders a relative hint: {:?}",
            usage.rows[0].reset_hint
        );
        // 300 minutes → "5h limit"; used derived from remaining (50-30=20).
        assert_eq!(usage.rows[1].label, "5h limit");
        assert_eq!(usage.rows[1].used, 20);
        assert_eq!(usage.rows[1].limit, 50);
        assert_eq!(usage.rows[1].reset_hint.as_deref(), Some("resets in 30m"));
        // Item-level fields when there is no `detail` object.
        assert_eq!(usage.rows[2].label, "RPM");
        assert_eq!(usage.rows[2].used, 12);
        assert_eq!(usage.rows[2].limit, 60);
    }

    /// Auth failure: 401 maps to the typed `Unauthorized` error (kimi-cli's
    /// "Authorization failed" path).
    #[tokio::test]
    async fn fetch_usage_maps_401_to_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/usages"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let err = fetch_usage(&reqwest::Client::new(), &server.uri(), "bad")
            .await
            .expect_err("401 must fail");
        assert!(matches!(err, UsageError::Unauthorized));
    }

    /// 404 maps to the "endpoint not available" error (kimi-cli parity).
    #[tokio::test]
    async fn fetch_usage_maps_404_to_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/usages"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let err = fetch_usage(&reqwest::Client::new(), &server.uri(), "tok")
            .await
            .expect_err("404 must fail");
        assert!(matches!(err, UsageError::NotFound));
    }

    /// Empty payload parses to zero rows (TUI shows "No usage data").
    #[test]
    fn parse_usage_payload_empty_object_yields_no_rows() {
        let usage = parse_usage_payload(&serde_json::json!({}));
        assert!(usage.rows.is_empty());
    }

    #[test]
    fn format_duration_matches_kimi_semantics() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(90), "1m");
        assert_eq!(format_duration(3_661), "1h 1m");
        assert_eq!(format_duration(90_000), "1d 1h");
    }
}
