//! Device identity headers for the Kimi Code OAuth endpoints.
//!
//! Every OAuth call (device authorization, token poll, refresh) carries three
//! headers identifying this installation (PRD F1):
//!
//! - `X-Msh-Device-Name` — the local hostname
//! - `X-Msh-Device-Model` — an honest local OS/arch string (e.g.
//!   "macOS 15.5 arm64"), ported from kimi-cli's `_device_model()`
//! - `X-Msh-Device-Id` — a uuid4 hex persisted at `~/.kigi/device_id`
//!   (owner-only), created on first use
//!
//! All values are ASCII-sanitized (ported from kimi-cli's
//! `_ascii_header_value`) since HTTP header values must be ASCII.

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context as _;

/// Sanitize a header value to ASCII: non-ASCII bytes are dropped; an empty
/// result falls back to `"unknown"`. Port of kimi-cli `_ascii_header_value`.
pub(crate) fn ascii_header_value(value: &str) -> String {
    let sanitized: String = value.chars().filter(char::is_ascii).collect();
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The three device-identity headers sent on every OAuth call.
///
/// Errors when the persistent device id cannot be created (e.g. read-only
/// `~/.kigi`): the OAuth endpoints require `X-Msh-Device-Id`, so login cannot
/// proceed without it.
pub(crate) fn device_headers() -> anyhow::Result<[(&'static str, String); 3]> {
    Ok([
        ("X-Msh-Device-Name", ascii_header_value(&device_name())),
        ("X-Msh-Device-Model", ascii_header_value(device_model())),
        ("X-Msh-Device-Id", ascii_header_value(&device_id()?)),
    ])
}

/// Local hostname (kimi-cli: `platform.node() or socket.gethostname()`).
fn device_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: buf is a valid writable buffer of the passed length.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).into_owned();
            if !name.trim().is_empty() {
                return name;
            }
        }
        "unknown".to_owned()
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_owned())
    }
    #[cfg(not(any(unix, windows)))]
    {
        "unknown".to_owned()
    }
}

/// Honest local device-model string, computed once per process. Port of
/// kimi-cli `_device_model()`:
/// - macOS → `macOS {product_version} {arch}` (e.g. "macOS 15.5 arm64")
/// - Windows → `Windows {10|11} {arch}` (build ≥ 22000 reports 11)
/// - other → `{sysname} {kernel_release} {machine}`
pub(crate) fn device_model() -> &'static str {
    static MODEL: OnceLock<String> = OnceLock::new();
    MODEL.get_or_init(compute_device_model)
}

fn compute_device_model() -> String {
    #[cfg(target_os = "macos")]
    {
        // Match Python's platform.machine() spelling on macOS.
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            other => other,
        };
        match macos_product_version() {
            Some(version) => format!("macOS {version} {arch}"),
            None => format!("macOS {arch}"),
        }
    }
    #[cfg(windows)]
    {
        let arch = std::env::consts::ARCH;
        match windows_release() {
            Some(release) => format!("Windows {release} {arch}"),
            None => format!("Windows {arch}"),
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let (sysname, release, machine) = uname_fields();
        match (release, machine) {
            (Some(r), Some(m)) => format!("{sysname} {r} {m}"),
            (Some(r), None) => format!("{sysname} {r}"),
            (None, Some(m)) => format!("{sysname} {m}"),
            (None, None) => sysname,
        }
    }
}

/// macOS product version (e.g. "15.5") from the SystemVersion plist — the
/// same source Python's `platform.mac_ver()` reads.
#[cfg(target_os = "macos")]
fn macos_product_version() -> Option<String> {
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    plist_string_value(&plist, "ProductVersion")
}

/// Extract `<key>{key}</key><string>value</string>` from a plist XML body.
#[cfg(target_os = "macos")]
fn plist_string_value(plist: &str, key: &str) -> Option<String> {
    let key_tag = format!("<key>{key}</key>");
    let after_key = &plist[plist.find(&key_tag)? + key_tag.len()..];
    let start = after_key.find("<string>")? + "<string>".len();
    let end = after_key.find("</string>")?;
    (start <= end).then(|| after_key[start..end].trim().to_owned())
}

/// Windows major release ("10" or "11"), from the build number reported by
/// `cmd /c ver` (kimi-cli: `sys.getwindowsversion().build >= 22000` → 11).
#[cfg(windows)]
fn windows_release() -> Option<String> {
    let output = std::process::Command::new("cmd")
        .args(["/c", "ver"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // "Microsoft Windows [Version 10.0.22631.3155]"
    let version = text.split("Version").nth(1)?.trim();
    let mut parts = version.trim_end_matches(']').split('.');
    let major = parts.next()?.trim().to_owned();
    let _minor = parts.next()?;
    let build: u32 = parts.next()?.trim().parse().ok()?;
    if major == "10" && build >= 22000 {
        Some("11".to_owned())
    } else {
        Some(major)
    }
}

/// `uname(2)` sysname / release / machine for Linux and other Unix.
#[cfg(all(unix, not(target_os = "macos")))]
fn uname_fields() -> (String, Option<String>, Option<String>) {
    // SAFETY: utsname is a plain-old-data struct; uname fills it in.
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uts) } != 0 {
        return (std::env::consts::OS.to_owned(), None, None);
    }
    fn field(raw: &[libc::c_char]) -> Option<String> {
        let bytes: Vec<u8> = raw
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        let s = String::from_utf8_lossy(&bytes).trim().to_owned();
        (!s.is_empty()).then_some(s)
    }
    (
        field(&uts.sysname).unwrap_or_else(|| std::env::consts::OS.to_owned()),
        field(&uts.release),
        field(&uts.machine),
    )
}

#[cfg(not(unix))]
#[cfg(not(windows))]
fn uname_fields() -> (String, Option<String>, Option<String>) {
    (std::env::consts::OS.to_owned(), None, None)
}

/// Path of the persistent device id: `{kigi_home}/device_id`.
fn device_id_path() -> PathBuf {
    kigi_config::kigi_home().join("device_id")
}

/// Persistent uuid4-hex device id, created (owner-only, 0o600) on first use
/// and cached for the process lifetime.
pub(crate) fn device_id() -> anyhow::Result<String> {
    static DEVICE_ID: OnceLock<String> = OnceLock::new();
    if let Some(id) = DEVICE_ID.get() {
        return Ok(id.clone());
    }
    let id = load_or_create_device_id(&device_id_path())?;
    Ok(DEVICE_ID.get_or_init(|| id).clone())
}

/// Read `path`, or mint a uuid4 hex and persist it owner-only.
fn load_or_create_device_id(path: &std::path::Path) -> anyhow::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_owned());
        }
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {} for device_id", parent.display()))?;
    }
    std::fs::write(path, &id)
        .with_context(|| format!("writing device id to {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    tracing::info!(path = %path.display(), "auth: created persistent device id");
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_header_value_passes_ascii_through() {
        assert_eq!(ascii_header_value("macOS 15.5 arm64"), "macOS 15.5 arm64");
        assert_eq!(ascii_header_value("  padded  "), "padded");
    }

    #[test]
    fn ascii_header_value_strips_non_ascii() {
        assert_eq!(ascii_header_value("café-host"), "caf-host");
        assert_eq!(ascii_header_value("机器"), "unknown");
        assert_eq!(ascii_header_value("  "), "unknown");
    }

    #[test]
    fn device_model_is_nonempty_ascii() {
        let model = device_model();
        assert!(!model.is_empty());
        assert!(model.is_ascii(), "device model must be ASCII: {model:?}");
        // The honest local OS name must lead the string.
        #[cfg(target_os = "macos")]
        assert!(model.starts_with("macOS "), "got {model:?}");
        #[cfg(windows)]
        assert!(model.starts_with("Windows"), "got {model:?}");
    }

    #[test]
    fn load_or_create_device_id_roundtrips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device_id");
        let created = load_or_create_device_id(&path).unwrap();
        assert_eq!(created.len(), 32, "uuid4 hex is 32 chars: {created:?}");
        assert!(created.chars().all(|c| c.is_ascii_hexdigit()));
        // Second call reads the same id back.
        let reread = load_or_create_device_id(&path).unwrap();
        assert_eq!(created, reread);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "device_id must be owner-only");
        }
    }

    #[test]
    fn load_or_create_device_id_ignores_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device_id");
        std::fs::write(&path, "  \n").unwrap();
        let created = load_or_create_device_id(&path).unwrap();
        assert_eq!(created.len(), 32);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_string_value_extracts_product_version() {
        let plist = r#"<?xml version="1.0"?>
<dict>
    <key>ProductBuildVersion</key>
    <string>24F74</string>
    <key>ProductVersion</key>
    <string>15.5</string>
</dict>"#;
        assert_eq!(
            plist_string_value(plist, "ProductVersion").as_deref(),
            Some("15.5")
        );
        assert_eq!(plist_string_value(plist, "Missing"), None);
    }
}
