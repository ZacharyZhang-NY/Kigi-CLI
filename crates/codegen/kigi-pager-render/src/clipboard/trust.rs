//! Trusted success / toast policy for clipboard writes.
//!
//! Writes still multi-fire every backend; this module decides whether we tell
//! the user it worked based on legs that actually reach the pasteboard they use.

use crate::host::{DisplayServer, HostOs};
use crate::terminal::TerminalName;

use super::{ClipboardToastKind, ClipboardWriteLegs};

/// True when native legs wrote the **local** OS clipboard (not SSH/container).
pub(crate) fn trusted_native(
    legs: &ClipboardWriteLegs,
    host_os: HostOs,
    display_server: DisplayServer,
    remote: bool,
    container: bool,
) -> bool {
    if remote || container || !legs.route_native {
        return false;
    }
    match host_os {
        HostOs::Linux => match display_server {
            // A verified wl-copy write, or an arboard write that went through
            // the compositor's data-control protocol (focus-free, no XWayland
            // bridge). Without data-control, arboard only reached the X11 side
            // and the Wayland paste may never see it.
            DisplayServer::Wayland => legs.wl_copy_ok || (legs.arboard_ok && legs.data_control),
            _ => legs.cli_ok || legs.arboard_ok,
        },
        HostOs::Macos | HostOs::Windows | HostOs::Other => legs.cli_ok || legs.arboard_ok,
    }
}

/// True when an OSC 52 write reaches the user's real clipboard.
///
/// Normally this requires the detected terminal brand to natively apply OSC 52
/// to the system pasteboard (fail closed). Two overrides widen the brand gate:
///
/// - `osc52_sink`: when `kigi wrap` is capturing this process's output (see
///   [`super::osc52_sink_active`]) the escape sequence is intercepted upstream
///   and copied to the *local* clipboard regardless of the (often misdetected,
///   e.g. over SSH) inner terminal brand, so the copy is trusted.
/// - `container` + `Unknown` brand: inside a container without a display server
///   (docker/podman), native legs *cannot* reach the user's pasteboard and the
///   container runtime does not forward brand env vars (`WT_SESSION`,
///   `TERM_PROGRAM`, …), so the brand is `Unknown` even when the outer terminal
///   (Windows Terminal, iTerm2, Ghostty, …) applies OSC 52 fine. Failing closed
///   here would mis-report *every* container copy as failed (GB report:
///   "Copy failed" toast in docker from Windows PowerShell while the copy
///   landed). The `CopiedOscContainer` toast already hedges with a fallback
///   instruction, so trust the emitted escape. A *detected* non-supporting
///   brand (env explicitly forwarded) stays fail-closed.
pub(crate) fn trusted_osc(
    legs: &ClipboardWriteLegs,
    brand: TerminalName,
    container: bool,
    osc52_sink: bool,
) -> bool {
    legs.osc52_ok
        && (brand.supports_osc52_clipboard()
            || osc52_sink
            || (container && brand == TerminalName::Unknown))
}

/// Toast from legs + env: native → OSC (incl. VS Code remote non-ASCII) → tmux → Failed.
// The arguments are independent environment inputs; bundling them into a struct
// would only move the same list to every call site.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_copy_toast(
    legs: &ClipboardWriteLegs,
    text: &str,
    brand: TerminalName,
    host_os: HostOs,
    display_server: DisplayServer,
    remote: bool,
    container: bool,
    osc52_sink: bool,
) -> ClipboardToastKind {
    if trusted_native(legs, host_os, display_server, remote, container) {
        return ClipboardToastKind::Copied;
    }
    if trusted_osc(legs, brand, container, osc52_sink) {
        if remote && brand.is_vscode_family() && !text.is_ascii() {
            return ClipboardToastKind::VsCodeSshNonAscii;
        }
        // A remote container reports the container toast: its fallback hint is
        // the actionable one.
        if container {
            return ClipboardToastKind::CopiedOscContainer;
        }
        if remote {
            return ClipboardToastKind::CopiedOscRemote;
        }
        return ClipboardToastKind::Copied;
    }
    if legs.tmux_ok {
        return ClipboardToastKind::CopiedTmux;
    }
    ClipboardToastKind::Failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::ClipboardWriteLegs;

    fn legs(
        route_native: bool,
        cli_ok: bool,
        arboard_ok: bool,
        tmux_ok: bool,
        osc52_ok: bool,
        cli_ok_tools: &str,
    ) -> ClipboardWriteLegs {
        ClipboardWriteLegs {
            route_native,
            cli_tools_tried: String::new(),
            cli_ok_tools: cli_ok_tools.into(),
            wl_copy_ok: cli_ok_tools.split('+').any(|t| t == "wl-copy"),
            cli_ok,
            arboard_ok,
            data_control: false,
            tmux_ok,
            osc52_ok,
        }
    }

    fn legs_data_control(
        route_native: bool,
        cli_ok: bool,
        arboard_ok: bool,
        tmux_ok: bool,
        osc52_ok: bool,
        cli_ok_tools: &str,
    ) -> ClipboardWriteLegs {
        ClipboardWriteLegs {
            data_control: true,
            ..legs(
                route_native,
                cli_ok,
                arboard_ok,
                tmux_ok,
                osc52_ok,
                cli_ok_tools,
            )
        }
    }

    fn resolve(
        legs: &ClipboardWriteLegs,
        brand: TerminalName,
        host_os: HostOs,
        display_server: DisplayServer,
        remote: bool,
        container: bool,
    ) -> ClipboardToastKind {
        resolve_copy_toast(
            legs,
            "hello",
            brand,
            host_os,
            display_server,
            remote,
            container,
            false,
        )
    }

    #[test]
    fn macos_local_native_ok() {
        let l = legs(true, true, false, false, false, "pbcopy");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Ghostty,
                HostOs::Macos,
                DisplayServer::Quartz,
                false,
                false
            ),
            ClipboardToastKind::Copied
        );
    }

    #[test]
    fn macos_apple_terminal_osc_only_fails() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::AppleTerminal,
                HostOs::Macos,
                DisplayServer::Quartz,
                false,
                false
            ),
            ClipboardToastKind::Failed
        );
        assert!(!TerminalName::AppleTerminal.supports_osc52_clipboard());
    }

    #[test]
    fn windows_local_native_ok() {
        let l = legs(true, false, true, false, false, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::WindowsTerminal,
                HostOs::Windows,
                DisplayServer::Win32,
                false,
                false
            ),
            ClipboardToastKind::Copied
        );
    }

    #[test]
    fn linux_x11_xclip_ok() {
        let l = legs(true, true, false, false, true, "xclip");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::X11,
                false,
                false
            ),
            ClipboardToastKind::Copied
        );
    }

    #[test]
    fn linux_wayland_arboard_only_fails() {
        let l = legs(true, false, true, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::Wayland,
                false,
                false
            ),
            ClipboardToastKind::Failed
        );
    }

    // Locked-down enterprise desktop: no clipboard CLI installed, but the
    // arboard write reached the compositor via data-control.
    #[test]
    fn linux_wayland_arboard_data_control_ok() {
        let l = legs_data_control(true, false, true, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::Wayland,
                false,
                false
            ),
            ClipboardToastKind::Copied
        );
    }

    // GNOME <= 47 or the kill-switch: no data-control protocol available.
    #[test]
    fn linux_wayland_arboard_without_data_control_still_fails() {
        let l = legs(true, false, true, false, false, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::Wayland,
                false,
                false
            ),
            ClipboardToastKind::Failed
        );
    }

    #[test]
    fn linux_wayland_data_control_without_arboard_fails() {
        let l = legs_data_control(true, false, false, false, false, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::Wayland,
                false,
                false
            ),
            ClipboardToastKind::Failed
        );
    }

    #[test]
    fn linux_wayland_wl_copy_ok() {
        let l = legs(true, true, false, false, true, "wl-copy");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::Wayland,
                false,
                false
            ),
            ClipboardToastKind::Copied
        );
    }

    #[test]
    fn linux_wayland_xclip_only_not_trusted_native() {
        let l = legs(true, true, true, false, true, "xclip");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::Wayland,
                false,
                false
            ),
            ClipboardToastKind::Failed
        );
    }

    #[test]
    fn linux_vte_osc_only_fails() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::X11,
                false,
                false
            ),
            ClipboardToastKind::Failed
        );
    }

    #[test]
    fn ssh_vte_osc_only_fails() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false
            ),
            ClipboardToastKind::Failed
        );
    }

    #[test]
    fn ssh_ghostty_osc_only_remote_toast() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Ghostty,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false
            ),
            ClipboardToastKind::CopiedOscRemote
        );
    }

    #[test]
    fn ssh_iterm2_osc_only_remote_toast() {
        // The remote toast only holds while Iterm2 is in the OSC-52 brand set.
        assert!(TerminalName::Iterm2.supports_osc52_clipboard());
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Iterm2,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false
            ),
            ClipboardToastKind::CopiedOscRemote
        );
    }

    #[test]
    fn local_ghostty_osc_only_copied() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Ghostty,
                HostOs::Linux,
                DisplayServer::X11,
                false,
                false
            ),
            ClipboardToastKind::Copied
        );
    }

    #[test]
    fn tmux_only_ok() {
        let l = legs(true, false, false, true, false, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::X11,
                false,
                false
            ),
            ClipboardToastKind::CopiedTmux
        );
    }

    #[test]
    fn vscode_ssh_ascii_trusted_osc_remote() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::VsCode,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false
            ),
            ClipboardToastKind::CopiedOscRemote
        );
    }

    #[test]
    fn vscode_ssh_non_ascii_trusted_osc() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve_copy_toast(
                &l,
                "café",
                TerminalName::VsCode,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false,
                false,
            ),
            ClipboardToastKind::VsCodeSshNonAscii
        );
    }

    #[test]
    fn vscode_ssh_non_ascii_untrusted_osc_fails() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve_copy_toast(
                &l,
                "café",
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false,
                false,
            ),
            ClipboardToastKind::Failed
        );
    }

    #[test]
    fn all_fail() {
        let l = legs(true, false, false, false, false, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Ghostty,
                HostOs::Linux,
                DisplayServer::X11,
                false,
                false
            ),
            ClipboardToastKind::Failed
        );
        assert!(!ClipboardToastKind::Failed.reported_success());
    }

    #[test]
    fn ssh_remote_native_not_trusted_without_osc() {
        let l = legs(true, true, true, false, false, "xclip");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::X11,
                true,
                false
            ),
            ClipboardToastKind::Failed
        );
    }

    #[test]
    fn container_ghostty_osc_container_toast() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Ghostty,
                HostOs::Linux,
                DisplayServer::Unknown,
                false,
                true
            ),
            ClipboardToastKind::CopiedOscContainer
        );
    }

    #[test]
    fn remote_and_container_prefers_container_toast() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Ghostty,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                true
            ),
            ClipboardToastKind::CopiedOscContainer
        );
    }

    // The common SSH case: the inner terminal is misdetected as Vte, which does
    // not natively support OSC 52, yet the `kigi wrap` sink upstream does.
    #[test]
    fn wrapped_ssh_vte_osc_trusted_via_sink() {
        let l = legs(true, false, false, false, true, "");
        // Trailing arg is the sink flag: without it, an untrusted brand over
        // SSH fails closed.
        assert_eq!(
            resolve_copy_toast(
                &l,
                "hello",
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false,
                false,
            ),
            ClipboardToastKind::Failed
        );
        // Same inputs with the sink active.
        assert_eq!(
            resolve_copy_toast(
                &l,
                "hello",
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false,
                true,
            ),
            ClipboardToastKind::CopiedOscRemote
        );
    }

    #[test]
    fn wrapped_sink_without_osc_write_still_fails() {
        let l = legs(true, false, false, false, false, "");
        assert!(!trusted_osc(&l, TerminalName::Vte, false, true));
        assert_eq!(
            resolve_copy_toast(
                &l,
                "hello",
                TerminalName::Vte,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false,
                true,
            ),
            ClipboardToastKind::Failed
        );
    }

    // Regression test for the false "Copy failed" toast in docker: the runtime
    // does not forward brand env vars, so the brand is Unknown even though the
    // outer terminal applies OSC 52 fine. See [`trusted_osc`].
    #[test]
    fn container_unknown_brand_osc_trusted() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Unknown,
                HostOs::Linux,
                DisplayServer::Unknown,
                false,
                true
            ),
            ClipboardToastKind::CopiedOscContainer
        );
    }

    #[test]
    fn container_unknown_brand_without_osc_write_fails() {
        let l = legs(true, false, false, false, false, "");
        assert!(!trusted_osc(&l, TerminalName::Unknown, true, false));
        assert_eq!(
            resolve(
                &l,
                TerminalName::Unknown,
                HostOs::Linux,
                DisplayServer::Unknown,
                false,
                true
            ),
            ClipboardToastKind::Failed
        );
    }

    // A brand that survived into the container means the env was explicitly
    // forwarded, so the detection is authoritative and stays fail-closed.
    #[test]
    fn container_detected_nonsupporting_brand_fails() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::AppleTerminal,
                HostOs::Linux,
                DisplayServer::Unknown,
                false,
                true
            ),
            ClipboardToastKind::Failed
        );
    }

    // The container override is deliberately narrow: plain SSH keeps failing
    // closed, since `kigi wrap` is the supported SSH path.
    #[test]
    fn ssh_unknown_brand_osc_only_still_fails() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve(
                &l,
                TerminalName::Unknown,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false
            ),
            ClipboardToastKind::Failed
        );
    }

    #[test]
    fn wrapped_container_osc_trusted_via_sink() {
        let l = legs(true, false, false, false, true, "");
        assert_eq!(
            resolve_copy_toast(
                &l,
                "hello",
                TerminalName::Unknown,
                HostOs::Linux,
                DisplayServer::Unknown,
                false,
                true,
                true,
            ),
            ClipboardToastKind::CopiedOscContainer
        );
    }
}
