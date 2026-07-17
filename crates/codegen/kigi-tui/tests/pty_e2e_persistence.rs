//! Session resume, persistence, storage recovery, background-task cleanup, and
//! parked/wake lifecycle PTY coverage.
//!
//! All cases are ignored for ordinary Cargo runs; Bazel opts in and caps this
//! process-heavy family at four concurrent libtest workers.

// Shared support intentionally serves all PTY family crates.
#[allow(dead_code, unused_imports)]
#[path = "pty_e2e/common.rs"]
mod common;

#[path = "pty_e2e/background_task_reaped_on_quit.rs"]
mod background_task_reaped_on_quit;
#[path = "pty_e2e/continue_resumes_session_with_history.rs"]
mod continue_resumes_session_with_history;
#[path = "pty_e2e/endline_park_two_static_markers.rs"]
mod endline_park_two_static_markers;
#[path = "pty_e2e/endline_wake_markers_close_each_wakeup.rs"]
mod endline_wake_markers_close_each_wakeup;
#[path = "pty_e2e/rename_title_shows_in_prompt_border.rs"]
mod rename_title_shows_in_prompt_border;
#[path = "pty_e2e/reparked_wait_repushes_buried_marker.rs"]
mod reparked_wait_repushes_buried_marker;
#[path = "pty_e2e/spinner_reappears_after_wait_resumes.rs"]
mod spinner_reappears_after_wait_resumes;
