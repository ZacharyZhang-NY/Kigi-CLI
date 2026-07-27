//! Launch pacing for a swarm, as pure state: no I/O, no clock, no tasks.
//!
//! A fan-out of N members hits ONE provider at once, so the launch order is
//! the difference between a swarm that runs and a swarm that 429s itself to
//! death. The runner asks [`LaunchPacer`] when it may start the next member
//! and reports rate limits back; everything here is decided arithmetically so
//! the policy is unit-testable without spawning an agent.

use std::time::Duration;

/// Members allowed to start with no wait at all.
pub const INITIAL_LAUNCH_BURST: usize = 5;

/// Spacing between launches once the burst is spent.
pub const LAUNCH_INTERVAL: Duration = Duration::from_millis(700);

/// First wait before re-attempting a rate-limited member.
pub const RETRY_MIN_BACKOFF: Duration = Duration::from_secs(3);

/// Cap on a single member's retry wait; a provider window outlasting this is
/// better spent letting other members through than sleeping longer.
pub const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(120);

/// Quiet period after which the fleet regains one lost capacity slot.
pub const CAPACITY_RECOVERY: Duration = Duration::from_secs(180);

/// Shortest gap between two capacity reductions, so one provider window that
/// rejects several members in a burst costs one slot, not all of them.
pub const CAPACITY_SHRINK_COOLDOWN: Duration = Duration::from_secs(2);

/// Env override for the concurrency ceiling; unset means the ramp is the only
/// brake. A value that does not parse as a positive integer is a hard error:
/// silently ignoring it would run an unbounded fan-out the operator forbade.
pub const MAX_CONCURRENCY_ENV: &str = "KIGI_AGENT_SWARM_MAX_CONCURRENCY";

/// Resolves [`MAX_CONCURRENCY_ENV`], failing loudly on a malformed value.
pub fn max_concurrency_from_env(raw: Option<&str>) -> Result<Option<usize>, String> {
    let Some(raw) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    match raw.parse::<usize>() {
        Ok(0) | Err(_) => Err(format!(
            "{MAX_CONCURRENCY_ENV} must be a positive integer, got {raw:?}"
        )),
        Ok(value) => Ok(Some(value)),
    }
}

/// What the runner should do right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchDecision {
    /// Start the next queued member immediately.
    Launch,
    /// Nothing may start yet; wait this long and ask again.
    Wait(Duration),
    /// Every member has been started.
    Drained,
}

/// The launch ramp plus the fleet's rate-limit response.
///
/// Time is injected as a monotonic `now` so tests drive it directly.
#[derive(Debug)]
pub struct LaunchPacer {
    queued: usize,
    in_flight: usize,
    started: usize,
    interval: Duration,
    hard_cap: Option<usize>,
    /// `None` until the first rate limit: before that the ramp alone paces us.
    capacity: Option<usize>,
    last_launch: Option<Duration>,
    last_shrink: Option<Duration>,
    last_rate_limit: Option<Duration>,
}

impl LaunchPacer {
    pub fn new(queued: usize, hard_cap: Option<usize>) -> Self {
        Self {
            queued,
            in_flight: 0,
            started: 0,
            interval: LAUNCH_INTERVAL,
            hard_cap,
            capacity: None,
            last_launch: None,
            last_shrink: None,
            last_rate_limit: None,
        }
    }

    /// The ceiling in force now: the operator's cap and the rate-limit-derived
    /// capacity both apply, whichever is lower.
    fn ceiling(&self) -> Option<usize> {
        match (self.hard_cap, self.capacity) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (only, None) | (None, only) => only,
        }
    }

    pub fn poll(&mut self, now: Duration) -> LaunchDecision {
        self.recover_capacity(now);

        if self.queued == 0 {
            return LaunchDecision::Drained;
        }
        if let Some(ceiling) = self.ceiling()
            && self.in_flight >= ceiling
        {
            // Held by capacity, not by the clock: only a member finishing (or
            // the recovery timer) can release this, so poll on the recovery
            // grain rather than spinning.
            return LaunchDecision::Wait(self.recovery_wait(now));
        }
        if self.started < INITIAL_LAUNCH_BURST {
            return LaunchDecision::Launch;
        }
        match self.last_launch {
            Some(last) if now.saturating_sub(last) < self.interval => {
                LaunchDecision::Wait(self.interval - now.saturating_sub(last))
            }
            _ => LaunchDecision::Launch,
        }
    }

    /// Record that the runner acted on a [`LaunchDecision::Launch`].
    pub fn on_launched(&mut self, now: Duration) {
        self.queued = self.queued.saturating_sub(1);
        self.in_flight += 1;
        self.started += 1;
        self.last_launch = Some(now);
    }

    /// Record that a member reached a terminal state.
    pub fn on_finished(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// Record that a member was rejected for rate limiting and re-queued.
    ///
    /// The fleet loses one slot (never below one) at most once per
    /// [`CAPACITY_SHRINK_COOLDOWN`].
    ///
    /// Upstream also doubles the launch interval when the rejected member never
    /// reached the provider. Kigi cannot observe that: every rejection arrives
    /// through a child session that DID start, so the branch would be dead code
    /// — and it ratchets one way, with no path back from a two-minute interval.
    pub fn on_rate_limited(&mut self, now: Duration) {
        self.queued += 1;
        self.in_flight = self.in_flight.saturating_sub(1);
        self.last_rate_limit = Some(now);

        let cooling = self
            .last_shrink
            .is_some_and(|last| now.saturating_sub(last) < CAPACITY_SHRINK_COOLDOWN);
        if !cooling {
            let current = self.capacity.unwrap_or(self.in_flight.max(1));
            self.capacity = Some(current.saturating_sub(1).max(1));
            self.last_shrink = Some(now);
        }
    }

    /// One slot back per quiet [`CAPACITY_RECOVERY`] window, until the cap is
    /// no longer the binding constraint.
    fn recover_capacity(&mut self, now: Duration) {
        let (Some(capacity), Some(last)) = (self.capacity, self.last_rate_limit) else {
            return;
        };
        if now.saturating_sub(last) < CAPACITY_RECOVERY {
            return;
        }
        self.last_rate_limit = Some(now);
        self.capacity = Some(capacity + 1);
    }

    fn recovery_wait(&self, now: Duration) -> Duration {
        let elapsed = self
            .last_rate_limit
            .map(|last| now.saturating_sub(last))
            .unwrap_or_default();
        CAPACITY_RECOVERY.saturating_sub(elapsed).max(self.interval)
    }
}

/// Ceiling on the whole swarm's wall clock.
///
/// Per-member bounds do not bound the fleet: a member may hold its slot for the
/// full foreground budget and then be re-queued, so a large swarm can otherwise
/// hold the caller's turn for hours. At the deadline the runner stops launching
/// and reports whatever has not finished, with ids, rather than waiting on.
pub const MAX_SWARM_RUNTIME: Duration = Duration::from_secs(30 * 60);

/// Rejections a single member may absorb before the runner gives up on it.
///
/// The swarm blocks the caller's turn, so every retry path needs a bound it
/// cannot argue its way past: a provider that refuses one member indefinitely
/// must surface as a failed member the caller can retry deliberately, never as
/// a turn that hangs.
pub const MAX_RATE_LIMIT_RETRIES: u32 = 5;

/// Per-member exponential backoff, capped. `attempt` counts prior rejections.
pub fn retry_backoff(attempt: u32) -> Duration {
    RETRY_MIN_BACKOFF
        .saturating_mul(2u32.saturating_pow(attempt.min(6)))
        .min(RETRY_MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn the_first_five_members_launch_without_waiting() {
        let mut pacer = LaunchPacer::new(10, None);
        for _ in 0..INITIAL_LAUNCH_BURST {
            assert_eq!(pacer.poll(ms(0)), LaunchDecision::Launch);
            pacer.on_launched(ms(0));
        }
        assert_eq!(
            pacer.poll(ms(0)),
            LaunchDecision::Wait(LAUNCH_INTERVAL),
            "the sixth member must wait out the ramp interval"
        );
    }

    #[test]
    fn the_ramp_admits_one_member_per_interval() {
        let mut pacer = LaunchPacer::new(10, None);
        for _ in 0..INITIAL_LAUNCH_BURST {
            pacer.on_launched(ms(0));
        }
        assert_eq!(pacer.poll(ms(699)), LaunchDecision::Wait(ms(1)));
        assert_eq!(pacer.poll(ms(700)), LaunchDecision::Launch);
    }

    #[test]
    fn an_operator_cap_binds_before_the_ramp() {
        let mut pacer = LaunchPacer::new(10, Some(2));
        pacer.on_launched(ms(0));
        pacer.on_launched(ms(0));
        assert!(
            matches!(pacer.poll(ms(0)), LaunchDecision::Wait(_)),
            "a cap of 2 must not admit a third member even inside the burst"
        );
        pacer.on_finished();
        assert_eq!(pacer.poll(ms(0)), LaunchDecision::Launch);
    }

    #[test]
    fn a_rate_limit_requeues_the_member_and_costs_one_slot() {
        let mut pacer = LaunchPacer::new(4, None);
        for _ in 0..4 {
            pacer.on_launched(ms(0));
        }
        pacer.on_rate_limited(ms(1_000));
        assert_eq!(pacer.capacity, Some(2), "3 in flight, minus the lost slot");
        assert!(
            matches!(pacer.poll(ms(1_000)), LaunchDecision::Wait(_)),
            "3 in flight against a capacity of 2 must not admit the requeued member"
        );
    }

    #[test]
    fn a_burst_of_rejections_costs_one_slot_not_all_of_them() {
        let mut pacer = LaunchPacer::new(6, None);
        for _ in 0..6 {
            pacer.on_launched(ms(0));
        }
        pacer.on_rate_limited(ms(1_000));
        let after_first = pacer.capacity;
        pacer.on_rate_limited(ms(1_500));
        assert_eq!(
            pacer.capacity, after_first,
            "a second rejection inside the cooldown must not shrink again"
        );
        pacer.on_rate_limited(ms(4_000));
        assert_eq!(
            pacer.capacity,
            after_first.map(|c| c - 1),
            "past the cooldown the fleet gives up another slot"
        );
    }

    #[test]
    fn capacity_never_reaches_zero() {
        let mut pacer = LaunchPacer::new(3, None);
        pacer.on_launched(ms(0));
        for i in 0..10 {
            pacer.on_rate_limited(Duration::from_secs(10 * (i + 1)));
        }
        assert_eq!(
            pacer.capacity,
            Some(1),
            "a fleet with no slots could never make progress"
        );
    }

    #[test]
    fn quiet_time_returns_a_lost_slot() {
        let mut pacer = LaunchPacer::new(4, None);
        for _ in 0..3 {
            pacer.on_launched(ms(0));
        }
        pacer.on_rate_limited(ms(1_000));
        let shrunk = pacer.capacity.expect("shrunk");
        pacer.poll(ms(1_000) + CAPACITY_RECOVERY);
        assert_eq!(pacer.capacity, Some(shrunk + 1));
    }

    #[test]
    fn the_retry_bound_is_reachable_within_the_backoff_cap() {
        // The bound must terminate in bounded time, not merely be finite.
        let worst: Duration = (0..MAX_RATE_LIMIT_RETRIES).map(retry_backoff).sum();
        assert!(
            worst <= RETRY_MAX_BACKOFF * MAX_RATE_LIMIT_RETRIES,
            "worst-case retry time {worst:?} must stay inside the per-attempt cap"
        );
    }

    #[test]
    fn draining_is_reported_once_every_member_has_started() {
        let mut pacer = LaunchPacer::new(1, None);
        pacer.on_launched(ms(0));
        assert_eq!(pacer.poll(ms(0)), LaunchDecision::Drained);
    }

    #[test]
    fn backoff_grows_then_stops_at_the_cap() {
        assert_eq!(retry_backoff(0), RETRY_MIN_BACKOFF);
        assert_eq!(retry_backoff(1), RETRY_MIN_BACKOFF * 2);
        assert_eq!(retry_backoff(2), RETRY_MIN_BACKOFF * 4);
        assert_eq!(retry_backoff(30), RETRY_MAX_BACKOFF);
    }

    #[test]
    fn a_malformed_concurrency_cap_is_refused_not_ignored() {
        assert_eq!(max_concurrency_from_env(None), Ok(None));
        assert_eq!(max_concurrency_from_env(Some("  ")), Ok(None));
        assert_eq!(max_concurrency_from_env(Some("4")), Ok(Some(4)));
        assert!(max_concurrency_from_env(Some("0")).is_err());
        assert!(max_concurrency_from_env(Some("many")).is_err());
        assert!(max_concurrency_from_env(Some("-1")).is_err());
    }
}
