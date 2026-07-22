pub(crate) mod attribution;
mod config;
pub mod credential_provider;
pub(crate) mod device;
pub mod device_code;
pub mod error;
mod flow;
pub(crate) mod kimi_oauth;
pub(crate) mod manager;
mod model;
pub(crate) mod oauth_device;
pub(crate) mod oauth_pkce;
pub(crate) mod oauth_registry;
pub(crate) mod recovery;
pub(crate) mod refresh;
mod storage;
pub(crate) mod token_type;
pub use config::{KIMI_CODE_OAUTH_SCOPE, KimiCodeConfig};
pub(crate) use flow::try_ensure_session_noninteractive;
pub use flow::{
    AuthChannels, AuthUrlInfo, AuthUrlMode, LogoutResult, ensure_authenticated,
    ensure_authenticated_or_noninteractive, perform_logout, run_auth_flow,
    run_auth_flow_with_stderr_bridge, run_cli_login, run_cli_logout, run_oauth_provider_flow,
    try_ensure_fresh_auth,
};
mod meta;
pub use device::device_headers;
pub use error::{AuthError, RefreshTokenError, RefreshTokenFailedReason};
pub use manager::{AuthManager, shared_api_key_provider};
pub use meta::AuthMeta;
pub use model::{AuthMode, KimiAuth, lookup_auth};
pub(crate) use model::{TOKEN_TTL, is_expired, token_suffix};
pub use storage::{
    clear_api_key, read_api_key, read_auth_json, read_platform_api_key, read_token_by_scope,
    store_api_key, store_platform_api_key,
};
