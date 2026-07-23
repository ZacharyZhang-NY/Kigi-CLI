//! Backpressure signal emitted by the event bus when a consumer falls behind.
//!
//! `EventLag` is a wire-format payload — it appears in the
//! `EventEnvelope.payload.lag` oneof over the gRPC `Events` stream — so the
//! canonical Rust definition belongs in this crate. The runtime
//! `EventStream<T>` wrapper in the workspace crate surfaces it to consumers as
//! `Result<T, EventLag>`, which is why it implements `Error`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Backpressure signal: the event-bus subscriber lagged behind the producer
/// and events were dropped.
///
/// `tag = "type"` matches the crate-wide convention that every wire enum is
/// tagged on a `type` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventLag {
    /// Count of events dropped between the previous successful receive and this one.
    #[error("lagged by {0} events")]
    Lagged(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let lag = EventLag::Lagged(42);
        let json = serde_json::to_string(&lag).unwrap();
        let back: EventLag = serde_json::from_str(&json).unwrap();
        assert_eq!(lag, back);
    }

    #[test]
    fn display_renders_count() {
        assert_eq!(EventLag::Lagged(7).to_string(), "lagged by 7 events");
    }

    #[test]
    fn json_shape_uses_type_tag_with_data_payload() {
        let lag = EventLag::Lagged(3);
        let json = serde_json::to_string(&lag).unwrap();
        assert_eq!(json, r#"{"type":"lagged","data":3}"#);
    }
}
