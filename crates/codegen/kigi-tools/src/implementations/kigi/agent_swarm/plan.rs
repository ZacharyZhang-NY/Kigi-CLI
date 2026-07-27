//! Turning `agent_swarm` input into member specs, and member results back
//! into one tool result. Pure: no spawning, no I/O.

use kigi_tool_types::{
    AgentSwarmToolInput, MAX_AGENT_SWARM_MEMBERS, PROMPT_TEMPLATE_PLACEHOLDER, SwarmMemberOutcome,
    SwarmMemberResult,
};

/// One member's launch instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSpec {
    /// The `items` entry, or the agent id for a resume — the label the caller
    /// sees in the result and reuses to retry this exact member.
    pub item: String,
    pub prompt: String,
    /// Set for a resume; `None` spawns a fresh member.
    pub resume_from: Option<String>,
}

/// Everything wrong with the input is reported before anything is spawned:
/// a half-launched swarm is far more expensive to recover from than a refused
/// tool call.
pub fn plan_members(input: &AgentSwarmToolInput) -> Result<Vec<MemberSpec>, String> {
    let resumes = input.resume_agent_ids.len();

    if input.items.is_empty() && resumes == 0 {
        return Err(
            "agent_swarm needs `items` (with `prompt_template`) or `resume_agent_ids`; \
             use the task tool for a single subagent"
                .to_string(),
        );
    }
    if resumes == 0 && input.items.len() < 2 {
        return Err(
            "agent_swarm runs 2 or more members; use the task tool for a single subagent"
                .to_string(),
        );
    }
    if input.items.len() + resumes > MAX_AGENT_SWARM_MEMBERS {
        return Err(format!(
            "agent_swarm runs at most {MAX_AGENT_SWARM_MEMBERS} members, got {}",
            input.items.len() + resumes
        ));
    }

    // Models emit `""`/`"null"`/`"none"` where they mean "no id"; accepting one
    // spawns a resume that can only die inside the coordinator.
    if let Some(bad) = input
        .resume_agent_ids
        .keys()
        .find(|id| !crate::implementations::kigi::task::types::is_valid_resume_id(id))
    {
        return Err(format!(
            "`resume_agent_ids` key {bad:?} is not a subagent id; use the agent_id \
             values from a previous agent_swarm result"
        ));
    }

    // Resumes first: they already hold context, so they reach the provider
    // before the fresh members compete for the same rate-limit budget.
    let mut specs: Vec<MemberSpec> = input
        .resume_agent_ids
        .iter()
        .map(|(agent_id, prompt)| MemberSpec {
            item: agent_id.clone(),
            prompt: prompt.clone(),
            resume_from: Some(agent_id.clone()),
        })
        .collect();

    if !input.items.is_empty() {
        let template = input
            .prompt_template
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or("agent_swarm requires `prompt_template` whenever `items` is given")?;
        if !template.contains(PROMPT_TEMPLATE_PLACEHOLDER) {
            return Err(format!(
                "`prompt_template` must contain the literal {PROMPT_TEMPLATE_PLACEHOLDER}, \
                 which is replaced by each entry of `items`"
            ));
        }
        for item in &input.items {
            if item.trim().is_empty() {
                return Err("`items` entries must be non-empty".to_string());
            }
            specs.push(MemberSpec {
                item: item.clone(),
                prompt: template.replace(PROMPT_TEMPLATE_PLACEHOLDER, item),
                resume_from: None,
            });
        }
    }

    // Two members handed the same prompt do the same work twice and, if it
    // writes, race each other over the same files.
    let mut seen = std::collections::HashSet::with_capacity(specs.len());
    for spec in &specs {
        if !seen.insert(spec.prompt.as_str()) {
            return Err(format!(
                "two members would run an identical prompt (from item {:?}); \
                 give every member a distinct scope",
                spec.item
            ));
        }
    }

    Ok(specs)
}

/// Per-member ceiling on rendered summary text.
///
/// 128 members' full outputs concatenated can exceed the context window at
/// exactly the moment the results matter. Native tool output is not truncated
/// anywhere downstream — only the MCP dispatcher does that — so the budget has
/// to live here. Each member keeps its head and its tail: the head says what it
/// did, the tail usually holds the verdict.
const MEMBER_SUMMARY_BUDGET: usize = 4_000;

/// Trims to [`MEMBER_SUMMARY_BUDGET`] on a char boundary, keeping both ends and
/// saying plainly how much was dropped.
fn clamp_summary(summary: &str) -> String {
    if summary.len() <= MEMBER_SUMMARY_BUDGET {
        return summary.to_string();
    }
    let keep = MEMBER_SUMMARY_BUDGET / 2;
    let head_end = (0..=keep)
        .rev()
        .find(|i| summary.is_char_boundary(*i))
        .unwrap_or(0);
    let tail_start = (summary.len() - keep..summary.len())
        .find(|i| summary.is_char_boundary(*i))
        .unwrap_or(summary.len());
    format!(
        "{}\n… {} bytes omitted; read this member's full output with the task-output tool …\n{}",
        &summary[..head_end],
        summary.len() - head_end - (summary.len() - tail_start),
        &summary[tail_start..]
    )
}

/// Renders the fleet's results as one tool result.
///
/// A failed member is reported inside the aggregate rather than failing the
/// call: the caller needs the members that DID succeed, and needs to know
/// precisely which ones to retry.
pub fn render_results(results: &[SwarmMemberResult]) -> String {
    let count = |wanted: SwarmMemberOutcome| results.iter().filter(|r| r.outcome == wanted).count();
    let completed = count(SwarmMemberOutcome::Completed);
    let failed = count(SwarmMemberOutcome::Failed);
    let aborted = count(SwarmMemberOutcome::Aborted);
    let backgrounded = count(SwarmMemberOutcome::Backgrounded);

    let mut out = String::from("<agent_swarm_result>\n");
    out.push_str(&format!(
        "<summary>completed: {completed}, failed: {failed}, aborted: {aborted}, \
         still running: {backgrounded}</summary>\n"
    ));
    if backgrounded > 0 {
        out.push_str(
            "<still_running>Some members outlived the foreground budget and are still \
             working. Do NOT re-launch their items — a second agent on the same files \
             corrupts both. Poll them with the task-output tool instead.</still_running>\n",
        );
    }

    if results.iter().any(SwarmMemberResult::is_resumable) {
        out.push_str(
            "<resume_hint>Call agent_swarm again with resume_agent_ids mapping the agent_id \
             values below to a follow-up prompt to continue unfinished work.</resume_hint>\n",
        );
    }

    for result in results {
        out.push_str("<member");
        if let Some(id) = &result.agent_id {
            out.push_str(&format!(" agent_id=\"{}\"", escape_attr(id)));
        }
        out.push_str(&format!(
            " item=\"{}\" state=\"{}\" outcome=\"{}\"",
            escape_attr(&result.item),
            if result.started() {
                "started"
            } else {
                "not_started"
            },
            result.outcome.as_str()
        ));
        if result.resumed {
            out.push_str(" mode=\"resume\"");
        }
        out.push_str(">\n");
        out.push_str(clamp_summary(result.summary.trim()).trim());
        out.push_str("\n</member>\n");
    }

    out.push_str("</agent_swarm_result>");
    out
}

/// Attribute-safe: an item is a model-supplied string and routinely contains
/// quotes or angle brackets (file paths, globs, shell fragments).
fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(items: &[&str], template: Option<&str>) -> AgentSwarmToolInput {
        AgentSwarmToolInput {
            description: "test swarm".into(),
            subagent_type: "general-purpose".into(),
            prompt_template: template.map(str::to_string),
            items: items.iter().map(|s| s.to_string()).collect(),
            resume_agent_ids: Default::default(),
            model: None,
        }
    }

    #[test]
    fn each_item_becomes_one_member_with_the_placeholder_substituted() {
        let specs = plan_members(&input(&["a.rs", "b.rs"], Some("Review {{item}} for bugs")))
            .expect("valid input");
        assert_eq!(
            specs,
            vec![
                MemberSpec {
                    item: "a.rs".into(),
                    prompt: "Review a.rs for bugs".into(),
                    resume_from: None,
                },
                MemberSpec {
                    item: "b.rs".into(),
                    prompt: "Review b.rs for bugs".into(),
                    resume_from: None,
                },
            ]
        );
    }

    #[test]
    fn a_single_item_is_refused_in_favour_of_the_task_tool() {
        let err = plan_members(&input(&["only.rs"], Some("Do {{item}}"))).unwrap_err();
        assert!(err.contains("2 or more"), "{err}");
    }

    #[test]
    fn a_template_without_the_placeholder_is_refused() {
        let err = plan_members(&input(&["a", "b"], Some("Do the work"))).unwrap_err();
        assert!(err.contains(PROMPT_TEMPLATE_PLACEHOLDER), "{err}");
    }

    #[test]
    fn items_without_a_template_are_refused() {
        let err = plan_members(&input(&["a", "b"], None)).unwrap_err();
        assert!(err.contains("prompt_template"), "{err}");
    }

    #[test]
    fn duplicate_expansions_are_refused_before_anything_spawns() {
        let err = plan_members(&input(&["same", "same"], Some("Do {{item}}"))).unwrap_err();
        assert!(err.contains("identical prompt"), "{err}");
    }

    #[test]
    fn more_members_than_the_ceiling_are_refused() {
        let items: Vec<String> = (0..=MAX_AGENT_SWARM_MEMBERS)
            .map(|i| format!("item-{i}"))
            .collect();
        let mut spec = input(&[], Some("Do {{item}}"));
        spec.items = items;
        let err = plan_members(&spec).unwrap_err();
        assert!(err.contains(&MAX_AGENT_SWARM_MEMBERS.to_string()), "{err}");
    }

    #[test]
    fn resumes_are_planned_before_fresh_members() {
        let mut spec = input(&["fresh.rs"], Some("Do {{item}}"));
        spec.resume_agent_ids
            .insert("agent-1".into(), "keep going".into());
        let specs = plan_members(&spec).expect("a resume lifts the two-item floor");
        assert_eq!(specs[0].resume_from.as_deref(), Some("agent-1"));
        assert_eq!(specs[0].prompt, "keep going");
        assert_eq!(specs[1].item, "fresh.rs");
    }

    #[test]
    fn an_empty_call_is_refused() {
        let err = plan_members(&input(&[], None)).unwrap_err();
        assert!(err.contains("resume_agent_ids"), "{err}");
    }

    fn member(item: &str, outcome: SwarmMemberOutcome, id: Option<&str>) -> SwarmMemberResult {
        SwarmMemberResult {
            item: item.into(),
            agent_id: id.map(str::to_string),
            resumed: false,
            outcome,
            summary: format!("summary for {item}"),
        }
    }

    #[test]
    fn the_aggregate_counts_outcomes_and_labels_every_member() {
        let rendered = render_results(&[
            member("a.rs", SwarmMemberOutcome::Completed, Some("id-a")),
            member("b.rs", SwarmMemberOutcome::Failed, Some("id-b")),
        ]);
        assert!(rendered.contains("completed: 1, failed: 1, aborted: 0, still running: 0"));
        assert!(
            rendered.contains(r#"agent_id="id-a" item="a.rs" state="started" outcome="completed""#)
        );
        assert!(rendered.contains(r#"outcome="failed""#));
        assert!(rendered.contains("summary for b.rs"));
    }

    #[test]
    fn the_resume_hint_appears_only_when_something_resumable_is_unfinished() {
        let all_done = render_results(&[member("a", SwarmMemberOutcome::Completed, Some("id-a"))]);
        assert!(!all_done.contains("resume_hint"));

        let never_started = render_results(&[member("a", SwarmMemberOutcome::Aborted, None)]);
        assert!(
            !never_started.contains("resume_hint"),
            "a member with no agent_id has nothing to resume"
        );

        let retryable = render_results(&[member("a", SwarmMemberOutcome::Failed, Some("id-a"))]);
        assert!(retryable.contains("resume_hint"));
    }

    #[test]
    fn an_item_containing_markup_cannot_break_out_of_its_attribute() {
        let rendered = render_results(&[member(
            r#"a" onload="x"#,
            SwarmMemberOutcome::Completed,
            None,
        )]);
        assert!(!rendered.contains(r#"item="a" onload="#), "{rendered}");
        assert!(rendered.contains("&quot;"), "{rendered}");
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn a_huge_member_output_is_clamped_but_keeps_both_ends() {
        let summary = format!("HEAD{}TAIL", "x".repeat(MEMBER_SUMMARY_BUDGET * 2));
        let clamped = clamp_summary(&summary);
        assert!(clamped.len() < summary.len(), "must shrink");
        assert!(clamped.starts_with("HEAD"), "the head says what it did");
        assert!(clamped.ends_with("TAIL"), "the tail holds the verdict");
        assert!(clamped.contains("bytes omitted"), "the loss must be stated");
    }

    #[test]
    fn a_summary_inside_the_budget_is_untouched() {
        assert_eq!(clamp_summary("short"), "short");
    }

    #[test]
    fn clamping_never_splits_a_character() {
        // Multi-byte throughout, so a naive byte slice would panic.
        let summary = "café ".repeat(MEMBER_SUMMARY_BUDGET);
        let clamped = clamp_summary(&summary);
        assert!(clamped.len() < summary.len());
    }
}
