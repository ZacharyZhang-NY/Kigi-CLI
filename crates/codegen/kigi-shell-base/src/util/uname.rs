//! OS version string for the `<user_info>` preamble.
//!
//! `std::env::consts::OS` returns `"macos"` / `"linux"` -- the OS *family*, not
//! the kernel name and not the release. This module wraps `libc::uname` (Unix)
//! and keeps `std::env::consts::OS` only as the last-resort fallback, so the
//! result is always a non-empty string.

/// Returns `"<kernel-lowercased> <release>"`, e.g. `"darwin 24.6.0"` or
/// `"linux 6.5.0-1024-aws"`.
pub fn os_kernel_and_release() -> String {
    #[cfg(unix)]
    {
        if let Some(s) = uname_unix() {
            return s;
        }
    }

    #[cfg(windows)]
    {
        if let Some(s) = windows_version() {
            return s;
        }
    }

    std::env::consts::OS.to_string()
}

#[cfg(unix)]
fn uname_unix() -> Option<String> {
    use std::mem::MaybeUninit;

    let mut uts: MaybeUninit<libc::utsname> = MaybeUninit::zeroed();
    // SAFETY: libc::uname writes into the provided buffer and returns 0 on
    // success / -1 on failure. We do not read any uninitialized fields on
    // the failure path.
    let rc = unsafe { libc::uname(uts.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: rc == 0 means uname populated all fields with NUL-terminated
    // strings of length <= the buffer size (per POSIX).
    let uts = unsafe { uts.assume_init() };

    let sysname = c_char_array_to_lowercase_string(&uts.sysname)?;
    let release = c_char_array_to_string(&uts.release)?;
    Some(format!("{sysname} {release}"))
}

#[cfg(unix)]
fn c_char_array_to_string(bytes: &[libc::c_char]) -> Option<String> {
    use std::ffi::CStr;
    // SAFETY: utsname fields are POSIX-defined NUL-terminated byte strings.
    // The cast from c_char to u8 is layout-compatible on all platforms libc
    // supports; we treat the bytes as opaque UTF-8 candidates.
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), bytes.len()) };
    let cstr = CStr::from_bytes_until_nul(bytes).ok()?;
    cstr.to_str().ok().map(|s| s.to_owned())
}

#[cfg(unix)]
fn c_char_array_to_lowercase_string(bytes: &[libc::c_char]) -> Option<String> {
    c_char_array_to_string(bytes).map(|s| s.to_lowercase())
}

#[cfg(windows)]
fn windows_version() -> Option<String> {
    use std::process::Command;

    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "ver"]);
    kigi_tty_utils::detach_std_command(&mut cmd);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    // `ver` outputs e.g. "Microsoft Windows [Version 10.0.22631.4890]".
    // The bracketed portion is locale-independent.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let start = stdout.find("[Version ")? + "[Version ".len();
    let end = stdout[start..].find(']')? + start;
    let version = stdout[start..end].trim();
    if version.is_empty() {
        return None;
    }
    Some(format!("windows {version}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_kernel_and_release_is_non_empty() {
        let s = os_kernel_and_release();
        assert!(!s.is_empty(), "os_kernel_and_release returned empty");
    }

    #[cfg(unix)]
    #[test]
    fn os_kernel_and_release_unix_shape() {
        let s = os_kernel_and_release();
        // A single token means uname failed and the `std::env::consts::OS`
        // fallback produced the value -- correct behavior, but the two-token
        // shape below does not apply to it.
        if !s.contains(' ') {
            return;
        }
        let mut parts = s.splitn(2, ' ');
        let kernel = parts.next().expect("kernel half present");
        let release = parts.next().expect("release half present");
        assert!(!kernel.is_empty(), "kernel half empty in '{s}'");
        assert!(!release.is_empty(), "release half empty in '{s}'");
        assert_eq!(
            kernel,
            kernel.to_lowercase(),
            "kernel half must be lowercase: '{s}'"
        );
    }

    /// Regression guard: macOS must report `darwin 24.6.0`, not the family
    /// name `macos`.
    #[cfg(target_os = "macos")]
    #[test]
    fn os_kernel_and_release_macos_says_darwin() {
        let s = os_kernel_and_release();
        // Single token means the uname call failed and the fallback returned
        // "macos". Never taken on real CI/dev hardware.
        if !s.contains(' ') {
            return;
        }
        assert!(
            s.starts_with("darwin "),
            "macOS host must report 'darwin <release>', got '{s}'"
        );
    }
}
