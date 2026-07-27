//! Detects a model stuck repeating one identical tool call.

/// Consecutive identical batches after which the turn is halted.
pub(crate) const MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS: u32 = 16;

/// Nudge threshold; half the budget remains after it.
pub(crate) const NUDGE_AFTER_IDENTICAL_TOOL_CALLS: u32 = 8;

/// Below the nudge on purpose: no-op runs get none.
pub(crate) const MAX_CONSECUTIVE_TRUE_NOOPS: u32 = 4;

const _: () = assert!(NUDGE_AFTER_IDENTICAL_TOOL_CALLS < MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS);
const _: () = assert!(MAX_CONSECUTIVE_TRUE_NOOPS < NUDGE_AFTER_IDENTICAL_TOOL_CALLS);

/// A shell command that does nothing whatsoever.
pub(crate) fn command_is_true(cmd: &str) -> bool {
    cmd.trim().eq_ignore_ascii_case("true")
}

/// Hashes name+args per call; separators prevent concatenation collisions.
pub(crate) fn hash_batch<'a>(calls: impl IntoIterator<Item = (&'a str, &'a str)>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (name, args) in calls {
        name.hash(&mut hasher);
        '\u{1f}'.hash(&mut hasher);
        args.hash(&mut hasher);
        '\u{1e}'.hash(&mut hasher);
    }
    hasher.finish()
}

/// One identity, so re-spelling a no-op cannot reset it.
const TRUE_NOOP_HASH: u64 = u64::MAX;

/// How many times the current batch has repeated unchanged.
#[derive(Default)]
pub(crate) struct IdenticalToolCallRun {
    /// Hash only: signatures hold raw tool arguments.
    last_hash: Option<u64>,
    tool_name: String,
    run_len: u32,
    is_true_noop_run: bool,
}

impl IdenticalToolCallRun {
    /// Records one batch and returns the length of the run it belongs to.
    pub(crate) fn observe(&mut self, batch_hash: u64, tool_name: &str, is_true_noop: bool) -> u32 {
        let hash = if is_true_noop {
            TRUE_NOOP_HASH
        } else {
            batch_hash
        };
        if self.last_hash == Some(hash) {
            self.run_len += 1;
        } else {
            self.run_len = 1;
            self.last_hash = Some(hash);
            self.is_true_noop_run = is_true_noop;
        }
        self.tool_name = tool_name.to_string();
        self.run_len
    }

    /// The run length at which this turn must be halted.
    pub(crate) fn hard_stop_threshold(&self) -> u32 {
        if self.is_true_noop_run {
            MAX_CONSECUTIVE_TRUE_NOOPS
        } else {
            MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS
        }
    }

    pub(crate) fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub(crate) fn is_true_noop_run(&self) -> bool {
        self.is_true_noop_run
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &str) -> u64 {
        hash_batch([(name, args)])
    }

    #[test]
    fn an_unchanged_batch_accumulates_a_run() {
        let mut run = IdenticalToolCallRun::default();
        let h = call("read_file", "a.rs");
        assert_eq!(run.observe(h, "read_file", false), 1);
        assert_eq!(run.observe(h, "read_file", false), 2);
        assert_eq!(run.observe(h, "read_file", false), 3);
    }

    #[test]
    fn any_change_in_the_arguments_restarts_the_run() {
        let mut run = IdenticalToolCallRun::default();
        run.observe(call("read_file", "a.rs"), "read_file", false);
        run.observe(call("read_file", "a.rs"), "read_file", false);
        assert_eq!(
            run.observe(call("read_file", "b.rs"), "read_file", false),
            1,
            "a different argument is a different action"
        );
    }

    #[test]
    fn two_batches_cannot_collide_by_concatenation() {
        // Unseparated these would hash identically.
        assert_ne!(
            hash_batch([("ab", "cd")]),
            hash_batch([("a", "b"), ("c", "d")])
        );
    }

    #[test]
    fn no_ops_share_one_run_however_they_are_spelled() {
        let mut run = IdenticalToolCallRun::default();
        assert_eq!(run.observe(call("bash", "true"), "bash", true), 1);
        assert_eq!(
            run.observe(call("bash", "  TRUE  "), "bash", true),
            2,
            "re-spelling a no-op must not reset the tighter ceiling"
        );
        assert_eq!(run.hard_stop_threshold(), MAX_CONSECUTIVE_TRUE_NOOPS);
    }

    #[test]
    fn a_real_call_after_a_noop_run_restores_the_ordinary_ceiling() {
        let mut run = IdenticalToolCallRun::default();
        run.observe(call("bash", "true"), "bash", true);
        assert_eq!(run.hard_stop_threshold(), MAX_CONSECUTIVE_TRUE_NOOPS);
        run.observe(call("read_file", "a.rs"), "read_file", false);
        assert_eq!(
            run.hard_stop_threshold(),
            MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS
        );
    }

    #[test]
    fn an_ordinary_repeated_call_is_halted_at_its_ceiling_and_not_before() {
        let mut run = IdenticalToolCallRun::default();
        let h = call("read_file", "a.rs");
        for expected in 1..MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS {
            let len = run.observe(h, "read_file", false);
            assert_eq!(len, expected);
            assert!(len < run.hard_stop_threshold(), "must not halt early");
        }
        assert_eq!(
            run.observe(h, "read_file", false),
            run.hard_stop_threshold(),
            "the turn halts on the 16th identical call"
        );
    }

    #[test]
    fn a_repeated_no_op_is_halted_far_sooner_and_without_a_nudge() {
        let mut run = IdenticalToolCallRun::default();
        let h = call("bash", "true");
        let mut len = 0;
        while len < run.hard_stop_threshold() {
            len = run.observe(h, "bash", true);
        }
        assert_eq!(len, MAX_CONSECUTIVE_TRUE_NOOPS);
        assert!(
            len < NUDGE_AFTER_IDENTICAL_TOOL_CALLS,
            "documented: a no-op run is halted before any nudge could fire"
        );
    }

    #[test]
    fn the_nudge_lands_with_budget_left_to_act_on_it() {
        let mut run = IdenticalToolCallRun::default();
        let h = call("read_file", "a.rs");
        let mut len = 0;
        while len < NUDGE_AFTER_IDENTICAL_TOOL_CALLS {
            len = run.observe(h, "read_file", false);
        }
        assert!(
            len < run.hard_stop_threshold(),
            "a warning the model cannot act on is not a warning"
        );
    }

    #[test]
    fn work_interleaved_with_repeats_is_never_halted() {
        let mut run = IdenticalToolCallRun::default();
        for i in 0..MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS * 2 {
            let h = if i % 2 == 0 {
                call("read_file", "a.rs")
            } else {
                call("read_file", "b.rs")
            };
            let len = run.observe(h, "read_file", false);
            assert!(
                len < run.hard_stop_threshold(),
                "alternating calls are progress, not a loop"
            );
        }
    }

    #[test]
    fn only_a_bare_true_counts_as_a_no_op() {
        assert!(command_is_true("true"));
        assert!(command_is_true("  true  "));
        assert!(command_is_true("TRUE"));
        assert!(!command_is_true("true && make"));
        assert!(!command_is_true("truely"));
        assert!(!command_is_true(""));
    }
}
