//! Shim — see `kigi_log::instrumentation` for the implementation.
//!
//! Two pieces stay here:
//! - The [`instrumentation_timer!`] macro, because it's `#[macro_export]`-ed
//!   from this crate and call sites spell it `crate::instrumentation_timer!`
//!   (i.e. `kigi_shell::instrumentation_timer!`). Keeping the macro here
//!   means downstream callers don't need to be edited.
//! - [`finalize_and_exit`], because shell needs to log a terminal exit event
//!   and flush instrumentation before the process exits.

pub use kigi_log::instrumentation::{
    ChromeTraceOptions, InstrumentationFinalizer, InstrumentationMode, InstrumentationTimer,
    TARGET, current_mode, finalize, finalizer, generate_chrome_trace, install_panic_hook, layer,
    timer,
};

/// Final cleanup before terminating the process.
///
/// Logs an exit event, flushes instrumentation guards, and exits with `code`.
///
/// Stays in shell so callers can keep calling `kigi_shell::instrumentation::finalize_and_exit`.
pub fn finalize_and_exit(code: i32) -> ! {
    let signal_name = match code {
        130 => "SIGINT",
        143 => "SIGTERM",
        _ => "other",
    };
    tracing::info!(
        event_type = "process_exit",
        signal = signal_name,
        exit_code = code,
        "Exiting process"
    );
    let _ = finalize();
    // Flush the --debug firehose; this exits via process::exit, bypassing main's flush.
    kigi_log::debug_log::flush();
    std::process::exit(code);
}

/// Time a block under the instrumentation target.
///
/// Macro stays in shell so `$crate` continues to resolve to `kigi_shell`
/// for the 12+ existing call sites that spell it as
/// `crate::instrumentation_timer!(...)` or `kigi_shell::instrumentation_timer!(...)`.
/// The macro body delegates to types and functions in
/// `kigi_log::instrumentation`.
#[macro_export]
macro_rules! instrumentation_timer {
    ($name:literal) => {{
        let mode = $crate::instrumentation::current_mode();
        match mode {
            $crate::instrumentation::InstrumentationMode::Chrome => {
                let span = tracing::info_span!(target: $crate::instrumentation::TARGET, $name);
                $crate::instrumentation::InstrumentationTimer::new_with_span(
                    $name,
                    mode,
                    Some(span.entered()),
                )
            }
            _ => $crate::instrumentation::InstrumentationTimer::new($name),
        }
    }};
}
