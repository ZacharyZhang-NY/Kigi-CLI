//! Prompt-suggestion gate and follow-up chips: the tab-autocomplete ghost
//! gate plus the follow-up chip lifecycle.

use super::{AgentView, FollowUps, MAX_PENDING_FOLLOW_UPS};

impl AgentView {
    /// Refresh the gate for the predicted-next-prompt ghost (tab
    /// autocomplete): it only shows on an idle session's normal prompt.
    /// Called before key dispatch and before each draw so a turn starting
    /// or an input-mode switch hides the ghost immediately. Also re-reads
    /// the enabled state so a `/settings` toggle applies live.
    pub(crate) fn refresh_prompt_suggestion_gate(&mut self) {
        self.prompt.prompt_suggestion.enabled = crate::views::prompt_suggestion::resolve_enabled();
        self.prompt.prompt_suggestion_active = self.prompt_input_mode
            == super::PromptInputMode::Normal
            && matches!(self.prompt_mode, super::PromptMode::Normal)
            && !self.session.state.is_busy();
    }

    /// Notify the suggestion controller that the prompt text changed.
    /// Returns an Effect to dispatch if the controller wants a debounce.
    ///
    /// Shell suggestions are a bash-mode (`!`) feature: outside it the
    /// pipeline never fires (no shell-history ghosts over natural-language
    /// chat text) and any leftover ghost/dropdown is torn down.
    pub(crate) fn notify_suggestion_text_changed(&mut self) -> Option<super::actions::Effect> {
        use crate::views::suggestion_controller::SuggestionAction;

        if self.prompt_input_mode != super::PromptInputMode::Bash {
            self.prompt.suggestions.clear_ghost();
            return None;
        }

        let snap = self.prompt.slash_state.snapshot();
        let slash_active = snap.active;
        let has_inline_ghost = snap.inline_ghost.is_some();
        // Copy text before passing to text_changed to satisfy the borrow checker.
        let text = self.prompt.text().to_owned();
        let action = self
            .prompt
            .suggestions
            .text_changed(&text, slash_active, has_inline_ghost)?;

        match action {
            SuggestionAction::Matched => None,
            SuggestionAction::Debounce { generation } => {
                Some(super::actions::Effect::DebounceSuggestions {
                    agent_id: self.session.id,
                    generation,
                })
            }
        }
    }

    /// Apply an `kigi/follow_ups` notification, keyed by `response_id`
    /// (newest-response-wins).
    ///
    /// Monotonic accept-the-newer: a never-seen `response_id` is strictly newer
    /// than any previously accepted one, so it supersedes the shown chips; a
    /// re-delivery of an already-accepted (hence older) response is ignored, so
    /// a buffer-replay or duplicate cannot clobber the newest chips on any
    /// turn-boundary path, with no reliance on a clear being wired there and no
    /// eviction window that could let a stale id pass as new. A re-delivery of
    /// the currently-shown response refreshes it in place (no-op when
    /// identical); empty `suggestions` retracts that response's chips. Returns
    /// `true` when the displayed chips changed (a redraw is warranted).
    /// Backward-compatible shim used by tests that don't exercise the turn
    /// identity: equivalent to a follow_ups notification with no stamped
    /// `promptId` (the older-shell / replay path). Production always routes
    /// through [`apply_follow_ups_with_prompt`] from `handle_follow_ups`.
    #[cfg(test)]
    pub(crate) fn apply_follow_ups(
        &mut self,
        response_id: String,
        suggestions: Vec<String>,
    ) -> bool {
        self.apply_follow_ups_with_prompt(response_id, None, suggestions)
    }

    /// `apply_follow_ups` with the turn identity (`prompt_id`) the shell stamps
    /// on each `kigi/follow_ups` notification (the same `promptId` it stamps on
    /// every `session/update`). The identity makes viewer-adoption dedup
    /// DETERMINISTIC:
    ///
    /// - A re-delivery of the CURRENTLY-ADOPTED turn's follow-ups (its
    ///   `prompt_id` equals `session.current_prompt_id`) re-renders even when its
    ///   chips were cleared by turn adoption — so chips that were applied then
    ///   cleared reappear instead of being lost until reload.
    /// - A buffer-replayed `kigi/follow_ups` for a PRIOR turn's `response_id`
    ///   stays rejected by the seen-ring (its `prompt_id` is not the active one),
    ///   so stale chips are never revived on the new turn.
    ///
    /// `prompt_id == None` (older shells, or a replay path that lacks it) is
    /// treated as "not provably the current turn" → it falls back to the
    /// monotonic newest-wins seen-ring and NEVER revives a cleared prior turn.
    pub(crate) fn apply_follow_ups_with_prompt(
        &mut self,
        response_id: String,
        prompt_id: Option<&str>,
        suggestions: Vec<String>,
    ) -> bool {
        // Re-delivery of the currently-shown response: refresh in place.
        if self
            .follow_ups
            .as_ref()
            .is_some_and(|c| c.response_id == response_id)
        {
            if self
                .follow_ups
                .as_ref()
                .is_some_and(|c| c.suggestions == suggestions)
            {
                return false;
            }
            self.follow_up_chips.clear();
            self.hovered_follow_up_chip = None;
            if suggestions.is_empty() {
                // Empty retraction of the currently-shown chips: drop this id
                // from the seen-ring so a later NON-empty delivery for the SAME
                // response can be re-accepted and re-rendered. Otherwise the id
                // (recorded when first accepted) would make the re-delivery hit
                // the `follow_up_seen` reject below and never display. This only
                // ever affects the currently-shown (newest) id — a genuinely
                // older/superseded id is never the shown one, so it never
                // reaches this branch and stays rejected (newest-wins intact).
                self.follow_up_seen.remove(&response_id);
                self.follow_ups = None;
                self.follow_up_shown_prompt_id = None;
            } else {
                self.follow_ups = Some(FollowUps {
                    response_id,
                    suggestions,
                });
                self.follow_up_shown_prompt_id = prompt_id.map(str::to_owned);
            }
            return true;
        }

        // Does this notification belong to the turn the client has currently
        // adopted? Deterministic when the shell stamped the `promptId`; `false`
        // for older shells / replay paths without one (those rely on the
        // newest-wins seen-ring below and never revive a prior turn).
        let current_prompt_id = self.session.current_prompt_id.as_deref();
        let is_current_turn =
            matches!((prompt_id, current_prompt_id), (Some(pid), Some(cur)) if pid == cur);
        // A stamped `promptId` that names a DIFFERENT turn than the one
        // currently adopted: this is a non-current turn's follow_ups (a PRIOR
        // turn's late first-time arrival, or a not-yet-adopted turn). It must
        // never render — as a re-delivery OR as "newest" — while another turn is
        // active, or its chips would appear over the running turn.
        //
        // Guarded on `current == Some`: a `None` `promptId` (older shells) has
        // no turn identity → newest-wins fallback; and `current == None` (e.g. a
        // just-finished turn whose trailing follow_ups arrive after
        // `current_prompt_id` was cleared) is NOT a mismatch, so those chips
        // still render.
        let names_other_active_turn =
            matches!((prompt_id, current_prompt_id), (Some(pid), Some(cur)) if pid != cur);

        if self.follow_up_seen.contains_key(&response_id) {
            // Already accepted. Normally this is an older, superseded response →
            // reject (newest-wins; a stale prior-turn buffer-replay must NOT
            // revive chips). EXCEPTION: if this IS the currently-adopted turn
            // (its `prompt_id` matches the active turn) and it carries chips, a
            // re-delivery whose chips were cleared by turn adoption must
            // re-render — scoped deterministically to the active turn so a prior
            // turn is never revived.
            if is_current_turn && !suggestions.is_empty() {
                self.follow_up_chips.clear();
                self.hovered_follow_up_chip = None;
                self.follow_ups = Some(FollowUps {
                    response_id,
                    suggestions,
                });
                self.follow_up_shown_prompt_id = prompt_id.map(str::to_owned);
                return true;
            }
            return false;
        }

        // First-time (never-seen) arrival for a turn that is NOT the active one.
        // It must not render NOW (it would draw over the running turn), but it
        // may be a not-yet-adopted FUTURE turn whose follow_ups raced ahead of
        // the `session/update` that adopts it. Dropping it would lose the chips
        // forever if it is the only delivery. Instead BUFFER it keyed by its
        // `promptId`; [`flush_pending_follow_ups`] renders it if/when that turn
        // becomes current. A genuinely prior turn's `promptId` never becomes
        // current again, so its buffered entry is never flushed (no stale
        // revival) and is eventually FIFO-evicted by the cap.
        if names_other_active_turn {
            if let Some(pid) = prompt_id
                && !suggestions.is_empty()
            {
                self.buffer_pending_follow_ups(pid.to_owned(), response_id, suggestions);
            }
            return false;
        }

        // Strictly newer response: supersede the prior chips (already recorded
        // in `follow_up_seen` at its own acceptance, so no re-record needed).
        let had_chips = self.follow_ups.take().is_some();
        self.follow_up_shown_prompt_id = None;
        self.follow_up_chips.clear();
        self.hovered_follow_up_chip = None;
        if suggestions.is_empty() {
            // An empty payload for a never-seen response is a no-op retraction
            // and is deliberately NOT recorded, so a later non-empty delivery
            // for the same response still renders.
            return had_chips;
        }
        self.follow_up_seen
            .insert(response_id.clone(), self.follow_up_next_gen);
        self.follow_up_next_gen += 1;
        self.follow_ups = Some(FollowUps {
            response_id,
            suggestions,
        });
        self.follow_up_shown_prompt_id = prompt_id.map(str::to_owned);
        true
    }

    /// Buffer a stamped `kigi/follow_ups` for a turn that is not yet current,
    /// keyed by its `promptId`. A newer delivery for the same `promptId`
    /// overwrites the earlier one (keep the latest); the FIFO order list bounds
    /// the map to [`MAX_PENDING_FOLLOW_UPS`], evicting only the oldest entry.
    fn buffer_pending_follow_ups(
        &mut self,
        prompt_id: String,
        response_id: String,
        suggestions: Vec<String>,
    ) {
        let is_new_key = self
            .follow_up_pending
            .insert(
                prompt_id.clone(),
                FollowUps {
                    response_id,
                    suggestions,
                },
            )
            .is_none();
        if is_new_key {
            self.follow_up_pending_order.push_back(prompt_id);
            if self.follow_up_pending_order.len() > MAX_PENDING_FOLLOW_UPS
                && let Some(evicted) = self.follow_up_pending_order.pop_front()
            {
                self.follow_up_pending.remove(&evicted);
            }
        }
    }

    /// Flush a buffered `kigi/follow_ups` for `prompt_id` (a turn that has just
    /// become current). Renders the chips through [`apply_follow_ups_with_prompt`]
    /// — now that `current_prompt_id == prompt_id`, the stamped delivery is
    /// accepted as the active turn's. Returns whether chips were rendered. A
    /// no-op when nothing is buffered for `prompt_id`. Callers invoke this AFTER
    /// setting `current_prompt_id` to `prompt_id` at every turn-adoption site.
    pub(crate) fn flush_pending_follow_ups(&mut self, prompt_id: &str) -> bool {
        let Some(pending) = self.follow_up_pending.remove(prompt_id) else {
            return false;
        };
        if let Some(pos) = self
            .follow_up_pending_order
            .iter()
            .position(|p| p == prompt_id)
        {
            self.follow_up_pending_order.remove(pos);
        }
        self.apply_follow_ups_with_prompt(pending.response_id, Some(prompt_id), pending.suggestions)
    }

    /// Drop the shown follow-up chips at a turn start (UX: they belong to the
    /// previous response). The response stays recorded in `follow_up_seen`, so a
    /// stale re-delivery stays rejected; the active turn's own re-delivery still
    /// re-renders via the `prompt_id` match in [`apply_follow_ups_with_prompt`],
    /// so this is used for BOTH viewer-adoption and self-driven turn starts.
    pub(crate) fn clear_follow_ups(&mut self) {
        self.follow_ups = None;
        self.follow_up_shown_prompt_id = None;
        self.follow_up_chips.clear();
        self.hovered_follow_up_chip = None;
    }

    /// Full follow-up reset for a session reload. Unlike [`clear_follow_ups`]
    /// (turn boundary — keeps `follow_up_seen` so a stale re-delivery stays
    /// rejected), a reload starts a fresh streaming session: follow-ups never
    /// persist, so the prior session's seen ids must also be dropped or they
    /// would suppress chips streamed after the reload.
    pub(crate) fn reset_follow_ups_for_reload(&mut self) {
        self.reset_follow_ups_for_reload_preserving(None);
    }

    /// Reload reset that PRESERVES the running turn's follow-ups for
    /// `keep_prompt_id` (the turn the load is about to adopt). On `SessionLoaded`
    /// the running turn's `kigi/follow_ups` arrive on the ext channel DURING
    /// `loading_replay`; an unconditional reset would drop them before adoption
    /// could re-render them, so the chips would never appear unless the server
    /// resent them. The running turn's chips live in ONE of two places at reset
    /// time:
    ///
    /// * [`follow_up_pending`](Self::follow_up_pending) — buffered, never
    ///   displayed (the turn was not current when the chips arrived); OR
    /// * [`follow_ups`](Self::follow_ups) — already ON SCREEN, because
    ///   `current_prompt_id` was unset or already equalled the running turn, so
    ///   the delivery took the newest-wins / current-turn render path instead
    ///   of the buffer.
    ///
    /// Both are preserved (the on-screen copy is the live, latest state, so it
    /// wins) by re-buffering the survivor into `follow_up_pending` keyed by
    /// `keep_prompt_id`; [`adopt_running_prompt`](Self::adopt_running_prompt)
    /// then flushes it. All other state — every OTHER turn's buffer, the seen
    /// ring, on-screen chips of any other turn — is still cleared, so a reload
    /// never leaves stale chips behind. `None` is a full reset (the
    /// reconnect-reload finalize path, which has no running turn to adopt).
    pub(crate) fn reset_follow_ups_for_reload_preserving(&mut self, keep_prompt_id: Option<&str>) {
        // Capture the running turn's follow_ups BEFORE wiping state. Prefer the
        // on-screen copy (it rendered, so it is the latest accepted delivery);
        // fall back to the pending buffer.
        let kept = keep_prompt_id.and_then(|keep| {
            let displayed = self
                .follow_up_shown_prompt_id
                .as_deref()
                .filter(|shown| *shown == keep)
                .and_then(|_| self.follow_ups.clone());
            displayed
                .or_else(|| self.follow_up_pending.get(keep).cloned())
                .map(|entry| (keep.to_owned(), entry))
        });

        self.follow_ups = None;
        self.follow_up_shown_prompt_id = None;
        self.follow_up_chips.clear();
        self.hovered_follow_up_chip = None;
        self.follow_up_seen.clear();
        self.follow_up_next_gen = 0;
        self.follow_up_pending.clear();
        self.follow_up_pending_order.clear();
        if let Some((pid, entry)) = kept {
            self.follow_up_pending.insert(pid.clone(), entry);
            self.follow_up_pending_order.push_back(pid);
        }
    }

    /// Index of the follow-up chip under a screen position, if any. Used by
    /// the mouse handler to submit the clicked suggestion as a literal prompt.
    pub(crate) fn follow_up_chip_at(&self, col: u16, row: u16) -> Option<usize> {
        self.follow_up_chips
            .iter()
            .position(|r| r.contains((col, row).into()))
    }

    /// Update hover highlight for follow-up chips. Returns true if the hover
    /// index changed (caller should re-render).
    pub(crate) fn set_hovered_follow_up_chip(&mut self, idx: Option<usize>) -> bool {
        if self.hovered_follow_up_chip == idx {
            return false;
        }
        self.hovered_follow_up_chip = idx;
        true
    }
}
