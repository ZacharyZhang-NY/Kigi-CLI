//! Presigned-URL helpers for S3-compatible endpoints.
//!
//! Used by tools that hand a user-configured bucket a short-lived PUT/GET URL
//! (e.g. the ZDR video-generation output path). No upload client lives here.

use anyhow::Context;

/// Parse credential content (JSON or INI format) into AWS SDK credentials.
fn parse_aws_credentials(content: &str) -> anyhow::Result<aws_sdk_s3::config::Credentials> {
    #[derive(serde::Deserialize)]
    struct JsonCreds {
        aws_access_key_id: String,
        aws_secret_access_key: String,
        #[serde(default)]
        aws_session_token: Option<String>,
    }

    if let Ok(parsed) = serde_json::from_str::<JsonCreds>(content) {
        return Ok(aws_sdk_s3::config::Credentials::new(
            &parsed.aws_access_key_id,
            &parsed.aws_secret_access_key,
            parsed.aws_session_token,
            None,
            "kigi-shell-trace-upload",
        ));
    }

    let strip_comment = |v: &str| {
        v.split_once('#')
            .map_or(v, |(before, _)| before)
            .trim()
            .to_owned()
    };
    let mut key_id = None;
    let mut secret = None;
    let mut token = None;
    for line in content.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "aws_access_key_id" => key_id = Some(strip_comment(v)),
                "aws_secret_access_key" => secret = Some(strip_comment(v)),
                "aws_session_token" => token = Some(strip_comment(v)),
                _ => {}
            }
        }
    }

    match (key_id, secret) {
        (Some(k), Some(s)) => Ok(aws_sdk_s3::config::Credentials::new(
            &k,
            &s,
            token,
            None,
            "kigi-shell-trace-upload",
        )),
        _ => anyhow::bail!(
            "AWS credentials are neither valid JSON \
             nor contain aws_access_key_id and aws_secret_access_key"
        ),
    }
}

/// Build an S3 client. Uses path-style addressing when `endpoint_url` is set.
///
/// Reads `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY` environment
/// variables so that S3 traffic can route through a corporate HTTP proxy when
/// the S3-compatible endpoint is not directly reachable.
pub(crate) async fn build_s3_client(
    region: &str,
    credentials_content: Option<&str>,
    credentials_file: Option<&str>,
    endpoint_url: Option<&str>,
) -> anyhow::Result<aws_sdk_s3::Client> {
    let proxy_config = aws_smithy_http_client::proxy::ProxyConfig::from_env();
    let http_client = aws_smithy_http_client::Builder::new().build_with_connector_fn(
        move |settings, _runtime_components| {
            let mut builder =
                aws_smithy_http_client::Connector::builder().proxy_config(proxy_config.clone());
            if let Some(s) = settings {
                builder.set_connector_settings(Some(s.clone()));
            }
            builder
                .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
                    aws_smithy_http_client::tls::rustls_provider::CryptoMode::Ring,
                ))
                .build()
        },
    );

    let mut config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .http_client(http_client)
        .region(aws_config::Region::new(region.to_owned()));

    let resolved_content = match (credentials_content, credentials_file) {
        (Some(inline), _) => Some(inline.to_owned()),
        (None, Some(path)) => Some(
            tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("Failed to read AWS credentials file: {path}"))?,
        ),
        (None, None) => None,
    };

    if let Some(ref content) = resolved_content {
        config_loader = config_loader.credentials_provider(parse_aws_credentials(content)?);
    } else if endpoint_url.is_some() {
        config_loader = config_loader.credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test",
            "test",
            None,
            None,
            "kigi-shell-test",
        ));
    }

    let sdk_config = config_loader.load().await;
    let mut builder =
        aws_sdk_s3::config::Builder::from(&sdk_config).force_path_style(endpoint_url.is_some());
    if let Some(url) = endpoint_url {
        builder = builder.endpoint_url(url);
    }
    Ok(aws_sdk_s3::Client::from_conf(builder.build()))
}

/// Static access-key credentials for presigning S3 URLs.
///
/// `Debug` is intentionally redacted — the struct holds plaintext secrets.
#[derive(Clone)]
pub struct S3StaticCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl std::fmt::Debug for S3StaticCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3StaticCredentials")
            .field("access_key_id", &"[redacted]")
            .field("secret_access_key", &"[redacted]")
            .finish()
    }
}

impl S3StaticCredentials {
    fn to_credentials_content(&self) -> String {
        serde_json::json!({
            "aws_access_key_id": self.access_key_id,
            "aws_secret_access_key": self.secret_access_key,
        })
        .to_string()
    }
}

pub async fn presign_put_url(
    region: &str,
    endpoint_url: Option<&str>,
    creds: &S3StaticCredentials,
    bucket: &str,
    key: &str,
    content_type: &str,
    expires_in: std::time::Duration,
) -> anyhow::Result<String> {
    let content = creds.to_credentials_content();
    let client = build_s3_client(region, Some(&content), None, endpoint_url).await?;
    let presigning_config = aws_sdk_s3::presigning::PresigningConfig::expires_in(expires_in)?;
    let presigned = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .content_type(content_type)
        .presigned(presigning_config)
        .await?;
    Ok(presigned.uri().to_string())
}

pub async fn presign_get_url(
    region: &str,
    endpoint_url: Option<&str>,
    creds: &S3StaticCredentials,
    bucket: &str,
    key: &str,
    expires_in: std::time::Duration,
) -> anyhow::Result<String> {
    let content = creds.to_credentials_content();
    let client = build_s3_client(region, Some(&content), None, endpoint_url).await?;
    let presigning_config = aws_sdk_s3::presigning::PresigningConfig::expires_in(expires_in)?;
    let presigned = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .presigned(presigning_config)
        .await?;
    Ok(presigned.uri().to_string())
}
