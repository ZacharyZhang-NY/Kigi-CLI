//! Count tracing events whose `message` starts with a known prefix.
//!
//! Producers should export exact prefixes as `pub const`s next to the
//! `tracing::debug!` site (e.g. `REFRESH_SCAN_LOG_PREFIX`) so tests never
//! duplicate the strings.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Pulls the formatted `message` field off one event.
#[derive(Default)]
struct MessageVisitor(String);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

/// Layer that counts, per registered prefix, events whose `message` starts
/// with it. Clones share the same counters.
#[derive(Clone)]
pub struct MessagePrefixCounter {
    counters: Arc<Vec<(&'static str, AtomicUsize)>>,
}

impl MessagePrefixCounter {
    pub fn new(prefixes: &[&'static str]) -> Self {
        Self {
            counters: Arc::new(prefixes.iter().map(|p| (*p, AtomicUsize::new(0))).collect()),
        }
    }

    /// Count for `prefix`. Panics if `prefix` was never registered.
    pub fn count(&self, prefix: &str) -> usize {
        self.counters
            .iter()
            .find(|(p, _)| *p == prefix)
            .unwrap_or_else(|| panic!("prefix not registered with this counter: {prefix:?}"))
            .1
            .load(Ordering::Relaxed)
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for MessagePrefixCounter {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        for (prefix, count) in self.counters.iter() {
            if visitor.0.starts_with(prefix) {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Thread-scoped default subscriber counting `prefixes`. Hold the guard for
/// the test lifetime. Only sees events on the current thread — use a
/// current-thread runtime for the subject under test.
pub fn install_prefix_counter_thread(
    prefixes: &[&'static str],
) -> (tracing::subscriber::DefaultGuard, MessagePrefixCounter) {
    use tracing_subscriber::layer::SubscriberExt as _;
    let counter = MessagePrefixCounter::new(prefixes);
    let subscriber = tracing_subscriber::registry().with(counter.clone());
    (tracing::subscriber::set_default(subscriber), counter)
}

/// Process-global subscriber counting `prefixes` — for subjects that spawn
/// their own threads/runtimes. Panics if a global subscriber already exists.
///
/// `stderr_env_filter` optionally tees matching formatted logs to stderr.
pub fn install_prefix_counter_global(
    prefixes: &[&'static str],
    stderr_env_filter: Option<&str>,
) -> MessagePrefixCounter {
    use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
    use tracing_subscriber::util::SubscriberInitExt as _;
    let counter = MessagePrefixCounter::new(prefixes);
    let fmt = stderr_env_filter.map(|filter| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_filter(tracing_subscriber::EnvFilter::new(filter))
    });
    tracing_subscriber::registry()
        .with(counter.clone())
        .with(fmt)
        .try_init()
        .expect("this test binary must own the global subscriber");
    counter
}
