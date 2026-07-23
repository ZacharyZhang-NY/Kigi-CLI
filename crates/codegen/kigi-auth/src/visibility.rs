/// Applies auth headers to outbound visibility requests. Implemented by
/// `kigi-shell::util::kigi_auth_credentials::KigiAuthCredentials`, keeping
/// credential construction owned by shell while data-collector builds the
/// request without reaching back into shell types.
pub trait HttpAuth: Send + Sync {
    fn apply(&self, builder: reqwest::RequestBuilder, base_url: &str) -> reqwest::RequestBuilder;
}
