//! Session-scoped subagent spawn limits, enforced by the shell's spawn path.

use crate::util::env::parse_positive_env;

pub const DEFAULT_MAX_CONCURRENT: usize = 32;

/// What happens to a spawn that arrives at the concurrent limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LimitBehavior {
    #[default]
    Queue,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentLimits {
    /// Subagents one session may run at once. Zero does not disable the
    /// limit: [`Self::effective_max_concurrent`] clamps it to 1.
    pub max_concurrent: usize,
    pub behavior: LimitBehavior,
}

impl Default for SubagentLimits {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            behavior: LimitBehavior::Queue,
        }
    }
}

impl SubagentLimits {
    /// Read once at the composition root; inject everywhere else.
    pub fn from_env() -> Self {
        Self::from_lookup(|var| std::env::var(var).ok())
    }

    /// A limit can be adjusted but never disabled.
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let default = Self::default();
        let max_concurrent = parse_positive_env(
            "KIGI_MAX_CONCURRENT_SUBAGENTS",
            lookup("KIGI_MAX_CONCURRENT_SUBAGENTS"),
        )
        .unwrap_or(default.max_concurrent);
        let behavior = match lookup("KIGI_SUBAGENT_LIMIT_BEHAVIOR") {
            None => LimitBehavior::Queue,
            Some(value) if value.eq_ignore_ascii_case("fail") => LimitBehavior::Fail,
            Some(value) if value.eq_ignore_ascii_case("queue") => LimitBehavior::Queue,
            Some(value) => {
                tracing::warn!(
                    %value,
                    "KIGI_SUBAGENT_LIMIT_BEHAVIOR is neither `queue` nor `fail`; keeping `queue`"
                );
                LimitBehavior::Queue
            }
        };
        Self {
            max_concurrent,
            behavior,
        }
    }

    /// The semaphore size: a zero limit must still admit one child, or
    /// queued spawns starve.
    pub fn effective_max_concurrent(&self) -> usize {
        self.max_concurrent.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::{LimitBehavior, SubagentLimits};

    #[test]
    fn limits_accept_plain_positive_digits_and_ignore_everything_else() {
        let from = |concurrent: Option<&str>, behavior: Option<&str>| {
            SubagentLimits::from_lookup(|var| {
                match var {
                    "KIGI_MAX_CONCURRENT_SUBAGENTS" => concurrent,
                    "KIGI_SUBAGENT_LIMIT_BEHAVIOR" => behavior,
                    other => panic!("unexpected lookup: {other}"),
                }
                .map(str::to_owned)
            })
        };

        assert_eq!(from(None, None), SubagentLimits::default());
        assert_eq!(
            from(Some("5"), Some("FAIL")),
            SubagentLimits {
                max_concurrent: 5,
                behavior: LimitBehavior::Fail,
            }
        );
        // Adjustable but never disabled: zero, negatives, non-digits,
        // scientific notation, digit separators, and out-of-range values all
        // keep defaults.
        for ignored in ["0", "-1", "abc", "1e3", "20_000", "18446744073709551616"] {
            assert_eq!(
                from(Some(ignored), Some("nonsense")),
                SubagentLimits::default(),
                "value {ignored:?} should fall back to the defaults"
            );
        }
    }

    #[test]
    fn a_zero_limit_is_clamped_to_one_not_disabled() {
        let limits = SubagentLimits {
            max_concurrent: 0,
            behavior: LimitBehavior::Queue,
        };
        assert_eq!(limits.effective_max_concurrent(), 1);
        assert_eq!(SubagentLimits::default().effective_max_concurrent(), 32);
    }
}
