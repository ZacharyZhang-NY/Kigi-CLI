//! Drives planned members through the backend at the pace
//! [`super::schedule::LaunchPacer`] allows, and collects their results.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

// One clock for the whole runner: `tokio::time::Instant` is the clock the
// sleeps below advance. Mixing it with `std::time::Instant` makes the retry
// windows and the timer disagree — under a paused clock they never converge
// and the loop spins forever.
use tokio::time::Instant;

use futures::stream::{FuturesUnordered, StreamExt};
use kigi_tool_types::{SwarmMemberOutcome, SwarmMemberResult};

use super::plan::MemberSpec;
use super::schedule::{
    LAUNCH_INTERVAL, LaunchDecision, LaunchPacer, MAX_RATE_LIMIT_RETRIES, MAX_SWARM_RUNTIME,
    retry_backoff,
};
use crate::implementations::kigi::task::backend::SubagentBackend;
use crate::implementations::kigi::task::types::{
    ModelOverrideProvenance, SubagentRequest, SubagentResult, SubagentRuntimeOverrides,
};

/// Everything the runner needs that is not per-member.
pub struct SwarmRunConfig {
    pub subagent_type: String,
    pub description: String,
    pub parent_session_id: String,
    pub parent_prompt_id: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub max_concurrency: Option<usize>,
}

/// A member waiting for its turn, plus how often the provider has refused it.
struct Pending {
    index: usize,
    spec: MemberSpec,
    attempts: u32,
    /// Earliest instant this member may be retried after a rate limit.
    not_before: Option<Instant>,
}

struct Finished {
    agent_id: Option<String>,
    result: SubagentResult,
}

/// Cancels members that are still running if the swarm's future goes away.
///
/// A send-now interrupt cancels the turn WITHOUT cancelling subagents, then
/// aborts the turn task — which drops this future and closes every member's
/// result channel. The coordinator reads a closed channel as "parent gone" and
/// re-attaches the child as a background task, so without this guard an
/// interrupt silently leaves up to `MAX_AGENT_SWARM_MEMBERS` agents editing the
/// caller's tree with no way to list them. `Drop` cannot await, so the cancels
/// are handed to the runtime; if there is no runtime left to hand them to,
/// nothing can be done and the ids are logged instead of lost silently.
struct InFlightGuard {
    backend: Arc<dyn SubagentBackend>,
    live: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
}

impl InFlightGuard {
    fn track(&self, id: &str) {
        self.live
            .lock()
            .expect("not poisoned")
            .insert(id.to_string());
    }

    fn release(&self, id: &str) {
        self.live.lock().expect("not poisoned").remove(id);
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let ids: Vec<String> = self
            .live
            .lock()
            .map(|live| live.iter().cloned().collect())
            .unwrap_or_default();
        if ids.is_empty() {
            return;
        }
        let backend = self.backend.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    for id in ids {
                        backend.cancel(&id).await;
                    }
                });
            }
            Err(_) => tracing::warn!(
                orphans = ?ids,
                "agent_swarm dropped with no runtime to cancel its members on"
            ),
        }
    }
}

/// Runs every member to a terminal state and returns their results in the
/// order they were planned.
pub async fn run_swarm(
    backend: Arc<dyn SubagentBackend>,
    specs: Vec<MemberSpec>,
    config: SwarmRunConfig,
) -> Vec<SwarmMemberResult> {
    let started_at = Instant::now();
    let total = specs.len();
    let labels: Vec<String> = specs.iter().map(|s| s.item.clone()).collect();
    let resumed: Vec<bool> = specs.iter().map(|s| s.resume_from.is_some()).collect();

    let mut queue: Vec<Pending> = specs
        .into_iter()
        .enumerate()
        .map(|(index, spec)| Pending {
            index,
            spec,
            attempts: 0,
            not_before: None,
        })
        .collect();
    queue.reverse(); // pop() takes the earliest-planned member first

    let mut pacer = LaunchPacer::new(total, config.max_concurrency);
    let mut in_flight = FuturesUnordered::new();
    let mut done: HashMap<usize, Finished> = HashMap::with_capacity(total);
    // The id each member was last launched under, so one abandoned at the wall
    // clock is still reportable as a live agent rather than an anonymous gap.
    let mut launched: Vec<Option<String>> = vec![None; total];
    let guard = InFlightGuard {
        backend: backend.clone(),
        live: Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new())),
    };

    while done.len() < total {
        let now = started_at.elapsed();
        if now >= MAX_SWARM_RUNTIME {
            tracing::warn!(
                elapsed_s = now.as_secs(),
                unfinished = total - done.len(),
                "agent_swarm hit its wall clock; reporting unfinished members"
            );
            break;
        }
        let decision = if queue.is_empty() {
            LaunchDecision::Drained
        } else {
            pacer.poll(now)
        };

        match decision {
            LaunchDecision::Launch => {
                // A member cooling off after a rate limit is not eligible yet;
                // rotate it behind one that is rather than idling the fleet.
                let Some(pending) = take_ready(&mut queue) else {
                    // Every queued member is cooling off. Whichever comes first
                    // — a slot freeing or the soonest retry opening — is the
                    // event worth waking for; awaiting only the in-flight side
                    // would park a member with a 3s backoff behind one with ten
                    // minutes left to run.
                    let wait = soonest_retry(&queue);
                    if in_flight.is_empty() {
                        match wait {
                            Some(wait) => tokio::time::sleep(wait).await,
                            // Unreachable: a member that is not ready has a
                            // `not_before`. Break rather than spin if it ever is.
                            None => break,
                        }
                        continue;
                    }
                    tokio::select! {
                        () = tokio::time::sleep(wait.unwrap_or(LAUNCH_INTERVAL)) => {}
                        Some(item) = in_flight.next() => {
                            settle(item, &guard, &mut pacer, &mut queue, &mut done, started_at);
                        }
                    }
                    continue;
                };
                let request = build_request(&pending, &config);
                let agent_id = request.id.clone();
                let index = pending.index;
                let attempts = pending.attempts;
                let spec = pending.spec;
                let backend = backend.clone();
                guard.track(&agent_id);
                launched[index] = Some(agent_id.clone());
                pacer.on_launched(now);
                in_flight.push(async move {
                    let result = backend.spawn(request).await;
                    (index, agent_id, attempts, spec, result)
                });
            }
            LaunchDecision::Wait(delay) => {
                if in_flight.is_empty() {
                    tokio::time::sleep(delay).await;
                } else {
                    // A finishing member frees a slot sooner than the timer.
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {}
                        Some(item) = in_flight.next() => {
                            settle(item, &guard, &mut pacer, &mut queue, &mut done, started_at);
                        }
                    }
                }
            }
            LaunchDecision::Drained => {
                if in_flight.is_empty() {
                    break;
                }
                collect_one(
                    &mut in_flight,
                    &guard,
                    &mut pacer,
                    &mut queue,
                    &mut done,
                    started_at,
                )
                .await;
            }
        }
    }

    (0..total)
        .map(|index| match done.remove(&index) {
            Some(finished) => to_member_result(
                labels[index].clone(),
                resumed[index],
                finished.agent_id,
                finished.result,
            ),
            // Reached when the swarm hit its wall clock: the member is alive
            // and unaccounted for, which is exactly `Backgrounded` — never
            // offer it for resume, and never invite a relaunch of its item.
            None => SwarmMemberResult {
                item: labels[index].clone(),
                agent_id: launched[index].clone(),
                resumed: resumed[index],
                outcome: SwarmMemberOutcome::Backgrounded,
                summary: "Still running when the swarm reached its time limit.".to_string(),
            },
        })
        .collect()
}

type InFlight = (
    usize,
    String,
    u32,
    MemberSpec,
    Result<SubagentResult, kigi_tool_runtime::ToolError>,
);

async fn collect_one(
    in_flight: &mut FuturesUnordered<impl Future<Output = InFlight>>,
    guard: &InFlightGuard,
    pacer: &mut LaunchPacer,
    queue: &mut Vec<Pending>,
    done: &mut HashMap<usize, Finished>,
    started_at: Instant,
) {
    if let Some(item) = in_flight.next().await {
        settle(item, guard, pacer, queue, done, started_at);
    }
}

/// Files one finished member: either terminal, or re-queued because the
/// provider — not the work — refused it.
fn settle(
    (index, agent_id, attempts, spec, outcome): InFlight,
    guard: &InFlightGuard,
    pacer: &mut LaunchPacer,
    queue: &mut Vec<Pending>,
    done: &mut HashMap<usize, Finished>,
    started_at: Instant,
) {
    guard.release(&agent_id);
    let now = started_at.elapsed();
    let mut transport_failed = false;
    let result = match outcome {
        Ok(result) => result,
        Err(err) => {
            transport_failed = true;
            SubagentResult {
                success: false,
                error: Some(err.to_string()),
                ..Default::default()
            }
        }
    };

    if result.rate_limited && attempts < MAX_RATE_LIMIT_RETRIES {
        pacer.on_rate_limited(now);
        queue.push(Pending {
            index,
            spec,
            attempts: attempts + 1,
            not_before: Some(Instant::now() + retry_backoff(attempts)),
        });
        return;
    }

    pacer.on_finished();
    // A transport failure means no child was ever created, so there is no id
    // to resume; prefer the coordinator's own id when it minted one.
    let agent_id = match (transport_failed, result.subagent_id.as_str()) {
        (true, _) => None,
        (false, "") => Some(agent_id),
        (false, minted) => Some(minted.to_string()),
    };
    done.insert(index, Finished { agent_id, result });
}

/// The earliest-planned member whose retry window has opened.
fn take_ready(queue: &mut Vec<Pending>) -> Option<Pending> {
    let now = Instant::now();
    let position = (0..queue.len())
        .rev()
        .find(|&i| queue[i].not_before.is_none_or(|at| at <= now))?;
    Some(queue.remove(position))
}

fn soonest_retry(queue: &[Pending]) -> Option<Duration> {
    let now = Instant::now();
    queue
        .iter()
        .filter_map(|p| p.not_before)
        .map(|at| at.saturating_duration_since(now))
        .min()
}

fn build_request(pending: &Pending, config: &SwarmRunConfig) -> SubagentRequest {
    let (result_tx, _) = tokio::sync::oneshot::channel();
    let resume_from = pending.spec.resume_from.clone();
    SubagentRequest {
        id: uuid::Uuid::now_v7().to_string(),
        prompt: pending.spec.prompt.clone(),
        description: config.description.clone(),
        subagent_type: config.subagent_type.clone(),
        parent_session_id: config.parent_session_id.clone(),
        parent_prompt_id: config.parent_prompt_id.clone(),
        cwd: config.cwd.clone(),
        runtime_overrides: SubagentRuntimeOverrides {
            // A resume inherits the source member's model, so an override here
            // would be dropped by the coordinator anyway.
            model: resume_from
                .is_none()
                .then(|| config.model.clone())
                .flatten(),
            model_override_provenance: ModelOverrideProvenance::Tool,
            reasoning_effort: None,
            persona: None,
            capability_mode: None,
            // Members share the caller's tree: 128 worktrees is not viable, and
            // the distinct-prompt rule is what keeps them off each other's files.
            isolation: None,
            harness_agent_type: None,
        },
        resume_from,
        // The swarm owns its members' lifetimes: it awaits every one of them
        // before returning, so none may outlive the tool call.
        run_in_background: false,
        surface_completion: true,
        fork_context: false,
        result_tx,
    }
}

fn to_member_result(
    item: String,
    resumed: bool,
    agent_id: Option<String>,
    result: SubagentResult,
) -> SwarmMemberResult {
    let outcome = if result.backgrounded {
        // Checked FIRST: the coordinator reports a detached member with
        // `success: false` and no output, which is indistinguishable from a
        // failure by every other field.
        SwarmMemberOutcome::Backgrounded
    } else if result.success {
        SwarmMemberOutcome::Completed
    } else if result.cancelled {
        SwarmMemberOutcome::Aborted
    } else {
        SwarmMemberOutcome::Failed
    };

    let output = result.output.trim();
    let summary = match (&result.error, outcome) {
        (_, SwarmMemberOutcome::Backgrounded) => format!(
            "Still running in the background; its result is not part of this call.{}",
            if output.is_empty() {
                String::new()
            } else {
                format!("\nProgress so far:\n{output}")
            }
        ),
        // Why it ended is the load-bearing half for anything that did not
        // complete — "max turns reached" must not be swallowed by whatever
        // text the member happened to emit last.
        (Some(error), SwarmMemberOutcome::Failed | SwarmMemberOutcome::Aborted) => {
            if output.is_empty() {
                error.clone()
            } else {
                format!("{error}\n{output}")
            }
        }
        _ if output.is_empty() => "Member produced no output.".to_string(),
        _ => output.to_string(),
    };

    SwarmMemberResult {
        item,
        agent_id,
        resumed,
        outcome,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::implementations::kigi::task::backend::SubagentBackend;
    use crate::implementations::kigi::task::types::{
        SubagentCancelOutcome, SubagentDescribeOutcome, SubagentSnapshot,
        SubagentValidateTypeOutcome,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records every prompt it is asked to run, and answers from a script
    /// keyed by prompt so a member can be refused once and then succeed.
    #[derive(Default)]
    struct FakeBackend {
        seen: Mutex<Vec<String>>,
        /// prompt -> remaining rate-limit rejections before it succeeds.
        refuse: Mutex<HashMap<String, u32>>,
        /// prompts that always fail outright.
        fail: Mutex<Vec<String>>,
        /// prompts the coordinator detaches instead of finishing.
        background: Mutex<Vec<String>>,
        /// prompt -> how long it occupies its slot, so members can overlap.
        duration_ms: Mutex<HashMap<String, u64>>,
        cancelled: Mutex<Vec<String>>,
        peak_in_flight: AtomicUsize,
        in_flight: AtomicUsize,
    }

    impl FakeBackend {
        fn prompts(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl SubagentBackend for FakeBackend {
        async fn spawn(
            &self,
            request: SubagentRequest,
        ) -> Result<SubagentResult, kigi_tool_runtime::ToolError> {
            let live = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_in_flight.fetch_max(live, Ordering::SeqCst);
            self.seen.lock().unwrap().push(request.prompt.clone());

            let refuse_now = {
                let mut refuse = self.refuse.lock().unwrap();
                match refuse.get_mut(&request.prompt) {
                    Some(remaining) if *remaining > 0 => {
                        *remaining -= 1;
                        true
                    }
                    _ => false,
                }
            };
            let fails = self.fail.lock().unwrap().contains(&request.prompt);
            let backgrounds = self.background.lock().unwrap().contains(&request.prompt);
            let hold = self
                .duration_ms
                .lock()
                .unwrap()
                .get(&request.prompt)
                .copied()
                .unwrap_or(0);
            // Yield for a real (virtual) interval so `FuturesUnordered` can
            // actually interleave: without an await point every member runs to
            // completion inside its own poll and nothing overlaps.
            tokio::time::sleep(Duration::from_millis(hold.max(1))).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);

            if backgrounds {
                return Ok(SubagentResult {
                    success: false,
                    backgrounded: true,
                    subagent_id: request.id.clone(),
                    child_session_id: "child".into(),
                    ..Default::default()
                });
            }

            if refuse_now {
                return Ok(SubagentResult {
                    success: false,
                    rate_limited: true,
                    child_session_id: "child".into(),
                    error: Some("Session error: Rate limited".into()),
                    ..Default::default()
                });
            }
            if fails {
                return Ok(SubagentResult {
                    success: false,
                    error: Some("it broke".into()),
                    ..Default::default()
                });
            }
            Ok(SubagentResult {
                success: true,
                output: Arc::from(format!("done: {}", request.prompt)),
                subagent_id: request.id.clone(),
                child_session_id: "child".into(),
                ..Default::default()
            })
        }

        async fn query(&self, _: &str, _: bool, _: Option<u64>) -> Option<SubagentSnapshot> {
            None
        }
        async fn cancel(&self, id: &str) -> SubagentCancelOutcome {
            self.cancelled.lock().unwrap().push(id.to_string());
            SubagentCancelOutcome::NotFound
        }
        async fn validate_type(&self, _: &str, _: &str) -> SubagentValidateTypeOutcome {
            SubagentValidateTypeOutcome::Ok
        }
        async fn describe_subagent_type(
            &self,
            _: &str,
            _: Option<&str>,
            _: &str,
        ) -> SubagentDescribeOutcome {
            SubagentDescribeOutcome::Unavailable
        }
    }

    fn specs(items: &[&str]) -> Vec<MemberSpec> {
        items
            .iter()
            .map(|item| MemberSpec {
                item: (*item).to_string(),
                prompt: format!("work on {item}"),
                resume_from: None,
            })
            .collect()
    }

    fn config(max_concurrency: Option<usize>) -> SwarmRunConfig {
        SwarmRunConfig {
            subagent_type: "general-purpose".into(),
            description: "test swarm".into(),
            parent_session_id: "parent".into(),
            parent_prompt_id: None,
            model: None,
            cwd: None,
            max_concurrency,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn every_member_runs_and_results_come_back_in_plan_order() {
        let backend = Arc::new(FakeBackend::default());
        // Finish in the exact reverse of plan order, so a runner that reported
        // completion order instead would fail this.
        for (item, ms) in [("a", 300), ("b", 200), ("c", 100)] {
            backend
                .duration_ms
                .lock()
                .unwrap()
                .insert(format!("work on {item}"), ms);
        }
        let results = run_swarm(backend.clone(), specs(&["a", "b", "c"]), config(None)).await;

        assert_eq!(
            results.iter().map(|r| r.item.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"],
            "results must be ordered as planned, not by completion"
        );
        assert!(
            results
                .iter()
                .all(|r| r.outcome == SwarmMemberOutcome::Completed)
        );
        assert_eq!(backend.prompts().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_member_does_not_sink_the_others() {
        let backend = Arc::new(FakeBackend::default());
        backend.fail.lock().unwrap().push("work on b".into());

        let results = run_swarm(backend, specs(&["a", "b", "c"]), config(None)).await;
        assert_eq!(results[0].outcome, SwarmMemberOutcome::Completed);
        assert_eq!(results[1].outcome, SwarmMemberOutcome::Failed);
        assert_eq!(results[2].outcome, SwarmMemberOutcome::Completed);
        assert!(results[1].summary.contains("it broke"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limited_member_is_retried_not_discarded() {
        let backend = Arc::new(FakeBackend::default());
        backend.refuse.lock().unwrap().insert("work on b".into(), 1);

        let results = run_swarm(backend.clone(), specs(&["a", "b"]), config(None)).await;
        assert!(
            results
                .iter()
                .all(|r| r.outcome == SwarmMemberOutcome::Completed),
            "the refused member must succeed on its retry: {results:?}"
        );
        assert_eq!(
            backend
                .prompts()
                .iter()
                .filter(|p| *p == "work on b")
                .count(),
            2,
            "the refused member must be attempted exactly twice"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_operator_cap_bounds_concurrency() {
        let hold_all = || {
            let backend = Arc::new(FakeBackend::default());
            for item in ["a", "b", "c", "d"] {
                backend
                    .duration_ms
                    .lock()
                    .unwrap()
                    .insert(format!("work on {item}"), 100);
            }
            backend
        };

        let capped = hold_all();
        run_swarm(
            capped.clone(),
            specs(&["a", "b", "c", "d"]),
            config(Some(2)),
        )
        .await;
        assert!(
            capped.peak_in_flight.load(Ordering::SeqCst) <= 2,
            "cap of 2 exceeded: peak was {}",
            capped.peak_in_flight.load(Ordering::SeqCst)
        );

        let uncapped = hold_all();
        run_swarm(uncapped.clone(), specs(&["a", "b", "c", "d"]), config(None)).await;
        assert!(
            uncapped.peak_in_flight.load(Ordering::SeqCst) > 2,
            "the fixture must be able to exceed the cap, or the assertion above proves nothing"
        );
    }

    /// A member the coordinator detached is still running: reporting it as a
    /// failure invites the model to relaunch its item, putting a second agent
    /// on the same files.
    #[tokio::test(start_paused = true)]
    async fn a_backgrounded_member_is_not_reported_as_failed_or_offered_for_resume() {
        let backend = Arc::new(FakeBackend::default());
        backend.background.lock().unwrap().push("work on b".into());

        let results = run_swarm(backend, specs(&["a", "b"]), config(None)).await;
        assert_eq!(results[1].outcome, SwarmMemberOutcome::Backgrounded);
        assert!(
            !results[1].is_resumable(),
            "a live member must never be offered for resume"
        );
        assert!(
            results[1].summary.contains("Still running"),
            "{}",
            results[1].summary
        );
    }

    /// Dropping the runner is what a send-now interrupt does; the members must
    /// be cancelled rather than silently detached onto the user's tree.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_swarm_cancels_its_live_members() {
        let backend = Arc::new(FakeBackend::default());
        for item in ["a", "b"] {
            backend
                .duration_ms
                .lock()
                .unwrap()
                .insert(format!("work on {item}"), 10_000);
        }

        tokio::select! {
            _ = run_swarm(backend.clone(), specs(&["a", "b"]), config(None)) => {
                panic!("members hold their slots for 10s; the swarm cannot finish first")
            }
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        // The guard hands its cancels to the runtime; let them run.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(
            !backend.cancelled.lock().unwrap().is_empty(),
            "a dropped swarm must cancel the members it started"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_permanently_refused_member_fails_instead_of_hanging_the_turn() {
        let backend = Arc::new(FakeBackend::default());
        // Refuses far more often than the retry bound allows.
        backend
            .refuse
            .lock()
            .unwrap()
            .insert("work on a".into(), 100);

        let results = run_swarm(backend.clone(), specs(&["a", "b"]), config(None)).await;
        assert_eq!(
            results[0].outcome,
            SwarmMemberOutcome::Failed,
            "the swarm blocks the caller's turn, so retries must be bounded"
        );
        assert_eq!(
            results[1].outcome,
            SwarmMemberOutcome::Completed,
            "one exhausted member must not sink its siblings"
        );
        assert_eq!(
            backend
                .prompts()
                .iter()
                .filter(|p| *p == "work on a")
                .count() as u32,
            MAX_RATE_LIMIT_RETRIES + 1,
            "the first attempt plus exactly the retry budget"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_resume_member_carries_its_source_id() {
        let backend = Arc::new(FakeBackend::default());
        let specs = vec![
            MemberSpec {
                item: "agent-7".into(),
                prompt: "keep going".into(),
                resume_from: Some("agent-7".into()),
            },
            MemberSpec {
                item: "fresh".into(),
                prompt: "start here".into(),
                resume_from: None,
            },
        ];
        let results = run_swarm(backend, specs, config(None)).await;
        assert!(results[0].resumed);
        assert!(!results[1].resumed);
    }
}
