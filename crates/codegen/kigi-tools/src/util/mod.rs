pub mod base64_images;
pub mod binary;
pub mod command_display;
pub mod env;
pub mod fs;
pub mod git_detect;
pub mod hash;
pub mod image_compress;
pub mod image_validate;
pub mod kigi_home;
pub mod mcp_truncate;
pub mod path_suggestions;
pub(crate) mod query_tools;
pub mod remap;
pub mod serde_base64;
pub mod spawn;
pub mod truncate;
pub mod unicode_confusables;

pub use command_display::strip_redundant_session_cd;
#[cfg(unix)]
pub use env::detach_from_tty;
pub use env::substitute_plugin_tokens;
pub use env::{KIGI_AGENT_ENV, KIGI_AGENT_ENV_VALUE, apply_kigi_agent_marker, pager_env};
pub use fs::{UnicodePathMatch, canonicalize_with_timeout, try_resolve_unicode_filename};
pub use kigi_home::{kigi_application, kigi_home};
pub use kigi_tty_utils::detach_std_command;
pub use path_suggestions::format_not_found_error;
pub use remap::{remap_json_keys, remap_schema_properties, reverse_map};
pub use spawn::{
    ProcessGroup, ProcessScope, detach_command, global_process_scope, new_process_group,
};
pub use truncate::{
    DEFAULT_SOFT_WRAP_WIDTH, ceil_char_boundary, estimate_tokens, floor_char_boundary,
    soft_wrap_line, soft_wrap_lines, truncate_line, truncate_str, truncate_str_with_marker,
};
