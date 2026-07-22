pub mod event_id;
pub mod kigi_home;
pub mod secure_file;
pub mod tips;
pub mod uname;
pub use kigi_shared::clipboard;
pub use kigi_shared::stderr::{stderr_lock, with_locked_stderr};
/// Generate a pseudo-random f64 in [0.0, 1.0).
///
/// Uses `RandomState::new()` which is OS-seeded (via `getrandom`) on each
/// instantiation, producing a unique hasher state per call. A fixed sentinel
/// is hashed to extract the random bits — the entropy comes entirely from
/// the OS-seeded `RandomState`, not from any clock source.
///
/// # Precision
/// The result uses all 53 bits of `f64` mantissa for a uniform distribution
/// over `[0.0, 1.0)`. We shift the 64-bit hash right by 11 bits to get a
/// 53-bit integer, then divide by `2^53`. This avoids the subtle bias that
/// occurs when casting a full `u64` to `f64` (which has only 52 bits of
/// mantissa, causing multiple `u64` values to map to the same `f64` for
/// values > 2^52).
///
/// Not cryptographically secure — suitable for sampling and feature
/// rollouts, not for security-sensitive randomness.
pub fn random_f64() -> f64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let random_state = RandomState::new();
    let mut hasher = random_state.build_hasher();
    hasher.write_u64(0x517cc1b727220a95);
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}
/// Probabilistic sampling. Returns `true` with probability `rate` (0.0–1.0).
pub fn probabilistic_sample(rate: f64) -> bool {
    random_f64() < rate
}
/// True when `candidate` is `trusted_base` or a path below it, comparing
/// scheme, host and effective port exactly (so suffix attacks such as
/// `api.kimi.com.evil.example` never match).
///
/// Public because the credential chokepoint
/// ([`kigi_shell::auth::credential_authority`](../../../kigi_shell/auth/credential_authority/index.html))
/// must compare a request URL against the SESSION's effective endpoints
/// (`[endpoints] coding_api_base_url` from config.toml, `models_base_url`, a
/// platform's own registry host) — none of which the env-var-only predicates
/// below can see.
pub fn matches_trusted_base_url(candidate: &str, trusted_base: &str) -> bool {
    let Ok(candidate) = reqwest::Url::parse(candidate) else {
        return false;
    };
    let Ok(trusted) = reqwest::Url::parse(trusted_base) else {
        return false;
    };
    let trusted_path = trusted.path();
    let candidate_path = candidate.path();
    let path_matches = candidate_path == trusted_path
        || candidate_path
            .strip_prefix(trusted_path)
            .is_some_and(|suffix| suffix.starts_with('/'));
    candidate.scheme() == trusted.scheme()
        && candidate.host_str() == trusted.host_str()
        && candidate.port_or_known_default() == trusted.port_or_known_default()
        && path_matches
}
/// True for subscription coding-API URLs (the compiled production endpoint;
/// deliberately NOT the env-overridable [`kigi_env::coding_api_base_url`] so a
/// runtime override can't widen this trust set).
pub fn is_production_coding_api_url(url: &str) -> bool {
    matches_trusted_base_url(url, kigi_env::PRODUCTION_ENDPOINTS.coding_api_base_url)
}
/// True for URLs the idle model-metadata refresh may re-fetch from: the
/// *effective* subscription coding endpoint (the `KIGI_CODE_BASE_URL`
/// override when set, else the compiled production endpoint), plus loopback
/// hosts (local dev proxies and test mocks). Unlike [`is_production_coding_api_url`]
/// this honours the env override and loopback, so use it only to gate traffic
/// that already flows to the session's configured base URL (the refresh
/// re-fetches from the same host the session samples against); it must never
/// widen a security trust set.
pub fn is_effective_coding_endpoint_url(url: &str) -> bool {
    if is_production_coding_api_url(url) {
        return true;
    }
    if matches_trusted_base_url(url, &kigi_env::coding_api_base_url()) {
        return true;
    }
    reqwest::Url::parse(url)
        .ok()
        .is_some_and(|u| match u.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(host)) => host == "localhost",
            None => false,
        })
}
/// True for first-party endpoints: the Kimi subscription coding API. The
/// session-token 401-refresh gate only refreshes against these; other hosts
/// are BYOK and exempt. Safe against invalid URLs and suffix attacks.
pub fn is_first_party_url(url: &str) -> bool {
    is_production_coding_api_url(url)
}
/// Truncate a string to at most `max_chars` characters.
/// Slices at char boundaries so multi-byte UTF-8 never panics.
pub fn truncate(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        return s;
    }
    let end = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    &s[..end]
}
/// Check if a process is still alive.
///
/// - Unix: `kill(pid, 0)` via `nix`. True if the process exists (even
///   under a different UID); false only on ESRCH.
/// - Windows: `OpenProcess(SYNCHRONIZE)` + `WaitForSingleObject(0)`. True
///   while running; false on exit, absence, or open failure.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => true,
    }
}
#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return false;
    };
    let wait_result = unsafe { WaitForSingleObject(handle, 0) };
    let _ = unsafe { CloseHandle(handle) };
    wait_result == WAIT_TIMEOUT
}
/// Terminate a process by PID. Idempotent: already-dead is `Ok`.
///
/// - Unix: `SIGTERM` via `nix::sys::signal::kill`; ESRCH maps to `Ok`.
/// - Windows: `OpenProcess(PROCESS_TERMINATE)` + `TerminateProcess`;
///   ERROR_INVALID_PARAMETER (Windows' "no such process") maps to `Ok`.
pub fn kill_process_by_pid(pid: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        match kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(e) => Err(std::io::Error::from_raw_os_error(e as i32)),
        }
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
        use windows::core::HRESULT;
        let no_such_process = HRESULT::from_win32(ERROR_INVALID_PARAMETER.0);
        let handle = match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            Ok(h) => h,
            Err(e) if e.code() == no_such_process => return Ok(()),
            Err(e) => {
                return Err(std::io::Error::other(format!("OpenProcess({pid}): {e}")));
            }
        };
        let terminate = unsafe { TerminateProcess(handle, 0) };
        let _ = unsafe { CloseHandle(handle) };
        terminate.map_err(|e| std::io::Error::other(format!("TerminateProcess({pid}): {e}")))
    }
}
/// True if `pid` is a kigi process; pairs with [`kill_process_by_pid`] to avoid killing a recycled PID.
/// Best-effort on macOS/BSD (liveness-only via `kill -0`), exact on Linux (/proc cmdline) and Windows (image path).
pub fn is_kigi_process(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let cmdline_path = format!("/proc/{pid}/cmdline");
        match std::fs::read(&cmdline_path) {
            Ok(data) => String::from_utf8_lossy(&data).contains("kigi"),
            Err(_) => false,
        }
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        };
        use windows::core::PWSTR;
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return false;
        };
        let mut buf: Vec<u16> = vec![0; 1024];
        let mut size: u32 = buf.len() as u32;
        let result = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut size,
            )
        };
        let _ = unsafe { CloseHandle(handle) };
        if result.is_err() {
            return false;
        }
        String::from_utf16_lossy(&buf[..size as usize])
            .to_ascii_lowercase()
            .contains("kigi")
    }
    #[cfg(all(not(target_os = "linux"), not(windows)))]
    {
        let mut cmd = std::process::Command::new("kill");
        cmd.args(["-0", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        kigi_tty_utils::detach_std_command(&mut cmd);
        cmd.status().is_ok_and(|s| s.success())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_is_production_coding_api_url_accepts_proxy_subpath() {
        assert!(is_production_coding_api_url(
            "https://api.kimi.com/coding/v1/chat/completions"
        ));
    }
    #[test]
    fn test_is_production_coding_api_url_rejects_public_api() {
        assert!(!is_production_coding_api_url("https://byok.example/v1"));
    }
    #[test]
    fn test_is_production_coding_api_url_rejects_spoofed_hostname() {
        assert!(!is_production_coding_api_url(
            "https://api.kimi.com.evil.example/coding/v1"
        ));
    }
    #[test]
    fn test_is_effective_coding_endpoint_url_accepts_prod_and_loopback() {
        assert!(is_effective_coding_endpoint_url(
            "https://api.kimi.com/coding/v1"
        ));
        assert!(is_effective_coding_endpoint_url("http://127.0.0.1:8080/v1"));
        assert!(is_effective_coding_endpoint_url("http://localhost:8080/v1"));
        assert!(is_effective_coding_endpoint_url("http://[::1]:8080/v1"));
    }
    #[test]
    fn test_is_effective_coding_endpoint_url_rejects_remote_third_party() {
        assert!(!is_effective_coding_endpoint_url("https://byok.example/v1"));
        assert!(!is_effective_coding_endpoint_url(
            "https://localhost.evil.example/v1"
        ));
    }
    #[test]
    fn test_is_production_coding_api_url_rejects_v11_prefix_confusion() {
        assert!(!is_production_coding_api_url(
            "https://api.kimi.com/coding/v11/chat/completions"
        ));
    }
    #[test]
    fn test_is_first_party_url() {
        assert!(is_first_party_url(
            "https://api.kimi.com/coding/v1/chat/completions"
        ));
        assert!(!is_first_party_url("https://byok.example/v1"));
        assert!(!is_first_party_url("https://api.openai.com/v1"));
        assert!(!is_first_party_url("https://api.anthropic.com/v1"));
        assert!(!is_first_party_url("https://api.kimi.com.evil.example/v1"));
        assert!(!is_first_party_url("not-a-url"));
        assert!(!is_first_party_url(""));
    }
    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 5), "hello");
        assert_eq!(truncate("abc🎉🎉def", 5), "abc🎉🎉");
    }
    #[test]
    fn is_process_alive_current_process() {
        assert!(is_process_alive(std::process::id()));
    }
    #[test]
    fn is_process_alive_dead_pid() {
        assert!(!is_process_alive(4_000_000_000));
    }
    #[cfg(unix)]
    #[test]
    fn is_process_alive_init_process() {
        assert!(is_process_alive(1));
    }
    #[test]
    fn kill_process_by_pid_already_dead_is_ok() {
        assert!(kill_process_by_pid(4_000_000_000).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn kill_process_by_pid_terminates_live_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        kill_process_by_pid(pid).expect("kill should succeed");
        let status = child.wait().expect("wait child");
        assert!(
            !status.success(),
            "sleep was terminated, not exited cleanly"
        );
    }
    #[test]
    fn is_kigi_process_self_true_impossible_pid_false() {
        assert!(is_kigi_process(std::process::id()));
        assert!(!is_kigi_process(u32::MAX));
    }
}
