//! Process manager utilities.

pub use crate::computer::types::{KillOutcome, TaskSnapshot};

pub fn format_system_time_rfc3339(time: std::time::SystemTime) -> String {
    use chrono::{DateTime, Utc};
    let datetime: DateTime<Utc> = time.into();
    datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
