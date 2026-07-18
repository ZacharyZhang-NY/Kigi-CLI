#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    #[test]
    fn settings_update_clearing_group_tool_verbs_reverts_to_default() {
        // Expected values come from the same chain the handler resolves, so the
        // test holds regardless of host config/env (a local `[ui]` or env
        // override legitimately beats the remote tier on both legs).
        let requirements = kigi_shell::config::load_merged_requirements();
        let user_config = kigi_shell::config::load_from_disk().ok();
        let managed_config = kigi_shell::config::load_managed_config().ok();
        let resolve = |remote_val: Option<bool>| {
            let remote = kigi_shell::util::config::RemoteSettings {
                group_tool_verbs: remote_val,
                ..Default::default()
            };
            kigi_shell::util::config::resolve_group_tool_verbs(
                requirements.as_ref(),
                user_config.as_ref(),
                managed_config.as_ref(),
                Some(&remote),
            )
            .value
        };
        let expect_on = resolve(Some(true));
        let expect_cleared = resolve(None);
        let mut app = make_app_with_agent("sess-1");

        // Remote enable arrives (redundant with the on-default, still latched).
        assert!(handle_ext_notification(
            &group_tool_verbs_settings_update(Some(true)),
            &mut app
        ));
        assert_eq!(
            crate::appearance::cache::load_group_tool_verbs(),
            expect_on,
            "remote Some(true) must re-resolve into the cache"
        );

        // remote settings clears the remote tier (field absent → None). Seed the
        // cache opposite to the expected outcome — the latched remote enable —
        // so only a real re-resolve can pass; the update must revert it to the
        // local/default resolution instead of skipping the field. An old
        // payload without the field takes this same path.
        crate::appearance::cache::set_group_tool_verbs(!expect_cleared);
        assert!(handle_ext_notification(
            &group_tool_verbs_settings_update(None),
            &mut app
        ));
        assert_eq!(
            crate::appearance::cache::load_group_tool_verbs(),
            expect_cleared,
            "cleared remote tier must re-resolve the full chain, not stay latched"
        );
        // Restore default (on) for other tests that share the process cache.
        crate::appearance::cache::set_group_tool_verbs(true);
    }

    #[test]
    fn settings_update_clearing_collapsed_edit_blocks_reverts_to_default() {
        // Expected values come from the same chain the handler resolves, so the
        // test holds regardless of host config/env (a local `[ui]` or env
        // override legitimately beats the remote tier on both legs).
        let requirements = kigi_shell::config::load_merged_requirements();
        let user_config = kigi_shell::config::load_from_disk().ok();
        let managed_config = kigi_shell::config::load_managed_config().ok();
        let resolve = |remote_val: Option<bool>| {
            let remote = kigi_shell::util::config::RemoteSettings {
                collapsed_edit_blocks: remote_val,
                ..Default::default()
            };
            kigi_shell::util::config::resolve_collapsed_edit_blocks(
                requirements.as_ref(),
                user_config.as_ref(),
                managed_config.as_ref(),
                Some(&remote),
            )
            .value
        };
        let expect_on = resolve(Some(true));
        let expect_cleared = resolve(None);
        let mut app = make_app_with_agent("sess-1");

        // remote settings enable arrives (the team rollout path).
        assert!(handle_ext_notification(
            &collapsed_edit_blocks_settings_update(Some(true)),
            &mut app
        ));
        assert_eq!(
            crate::appearance::cache::load_collapsed_edit_blocks(),
            expect_on,
            "remote Some(true) must re-resolve into the cache"
        );

        // remote settings clears the remote tier (field absent → None). Seed the
        // cache opposite to the expected outcome — the latched remote enable —
        // so only a real re-resolve can pass; the update must revert it to the
        // local/default resolution instead of skipping the field. An old
        // payload without the field takes this same path.
        crate::appearance::cache::set_collapsed_edit_blocks(!expect_cleared);
        assert!(handle_ext_notification(
            &collapsed_edit_blocks_settings_update(None),
            &mut app
        ));
        assert_eq!(
            crate::appearance::cache::load_collapsed_edit_blocks(),
            expect_cleared,
            "cleared remote tier must re-resolve the full chain, not stay latched"
        );
        // Restore default (off) for other tests that share the process cache.
        crate::appearance::cache::set_collapsed_edit_blocks(false);
    }

    /// A remote collapsed_edit_blocks flip re-materializes on-default Edit
    /// rows in the live transcript (the same policy the settings toggle
    /// applies via `apply_collapsed_edit_blocks_flip`).
    #[test]
    fn settings_update_collapsed_edit_blocks_flip_refolds_live_edits() {
        use crate::scrollback::types::DisplayMode;

        crate::appearance::cache::set_collapsed_edit_blocks(false);
        let mut app = make_app_with_agent("sess-1");
        let id = {
            let sb = &mut app.agents.get_mut(&AgentId(0)).unwrap().scrollback;
            sb.push_block(crate::scrollback::block::RenderBlock::ToolCall(
                crate::scrollback::blocks::tool::ToolCallBlock::Edit(
                    crate::scrollback::blocks::tool::EditToolCallBlock::new("f.rs", vec![]),
                ),
            ))
        };
        assert_eq!(
            app.agents[&AgentId(0)].scrollback.get_by_id(id).unwrap().display_mode,
            DisplayMode::Expanded,
            "flag off materializes expanded"
        );

        assert!(handle_ext_notification(
            &collapsed_edit_blocks_settings_update(Some(true)),
            &mut app
        ));
        if !crate::appearance::cache::load_collapsed_edit_blocks() {
            // A host-level env/config override outranked the remote value, so
            // no real flip occurred and the re-fold didn't run — nothing to
            // assert on this machine (CI runs with clean layers).
            return;
        }
        assert_eq!(
            app.agents[&AgentId(0)].scrollback.get_by_id(id).unwrap().display_mode,
            DisplayMode::Collapsed,
            "remote enable must collapse the on-default Edit row"
        );
        // Restore default (off) for other tests that share the process cache.
        crate::appearance::cache::set_collapsed_edit_blocks(false);
    }

    /// The live-refresh flip mirrors `set_group_tool_verbs_inner`'s stale
    /// group-expansion cleanup: a previously expanded verb slot must not
    /// survive a remote flip as an expanded header.
    #[test]
    fn settings_update_flip_resets_stale_group_expansion() {
        crate::appearance::cache::set_group_tool_verbs(true);
        let mut app = make_app_with_agent("sess-1");
        {
            let sb = &mut app.agents.get_mut(&AgentId(0)).unwrap().scrollback;
            for i in 0..3 {
                sb.push_block(crate::scrollback::block::RenderBlock::read(
                    format!("f{i}.rs"),
                    None,
                ));
            }
            sb.prepare_layout(80, 40);
            sb.set_selected(Some(0));
            assert!(sb.toggle_group_expansion());
            sb.prepare_layout(80, 40);
            let info = sb.get_cached_entry_layouts().unwrap()[0];
            assert!(info.group_collapse_header, "expanded verb slot armed");
        }

        assert!(handle_ext_notification(
            &group_tool_verbs_settings_update(Some(false)),
            &mut app
        ));
        if crate::appearance::cache::load_group_tool_verbs() {
            // A host-level env/config override outranked the remote value, so
            // no real flip occurred and the cleanup path didn't run — nothing
            // to assert on this machine (CI runs with clean layers).
            return;
        }
        let sb = &mut app.agents.get_mut(&AgentId(0)).unwrap().scrollback;
        sb.prepare_layout(80, 40);
        let info = sb.get_cached_entry_layouts().unwrap()[0];
        assert!(
            !info.group_collapse_header,
            "remote flip must drop the stale expansion"
        );
        assert!(
            sb.get_cached_entry_height(1).unwrap_or(0) > 0,
            "rows render individually after the flip"
        );
    }

    #[test]
    fn auto_gate_killswitch_clears_all_agents_regardless_of_active_mirror() {
        // Two agents both in auto; the active tab's global mirror reads "ask"
        // (a tab switch / Shift+Tab re-anchored it away from auto). A
        // mid-session gate kill-switch (`auto_permission_mode_enabled=false`)
        // must clear the per-session auto flag on BOTH agents. The old code
        // gated this fan-out on `current_ui.permission_mode == "auto"`, so it
        // skipped background agents and left stale `auto_mode` that
        // `switch_to_agent` could re-anchor back to "auto" on return.
        let mut app = make_app_two_agents();
        app.auto_mode_gate = true;
        for agent in app.agents.values_mut() {
            agent.session.auto_mode = true;
        }
        // Active tab's mirror is NOT "auto" — the old bug's skip condition.
        app.current_ui.permission_mode = Some("ask".into());

        let killswitch = acp::ExtNotification::new(
            "kigi/settings/update",
            serde_json::value::to_raw_value(
                &serde_json::json!({ "auto_permission_mode_enabled": false }),
            )
            .unwrap()
            .into(),
        );
        let _ = handle_ext_notification(&killswitch, &mut app);

        assert!(!app.auto_mode_gate, "gate must be off after kill-switch");
        for (id, agent) in &app.agents {
            assert!(
                !agent.session.auto_mode,
                "agent {id:?} auto_mode must be cleared by the kill-switch"
            );
        }
    }

    #[test]
    fn auto_gate_killswitch_notifies_agents_to_leave_auto() {
        // The kill-switch must tell live sessions to leave Auto, else the agent
        // keeps classifier-approving while the UI shows "Ask". The notification is
        // CLIENT-scoped, so exactly ONE fires regardless of how many tabs were in
        // auto; it omits `yolo_mode` so a sibling always-approve tab is preserved.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = AppView::new(tx, ModelState::default(), Vec::new());
        // Two auto agents + one always-approve sibling, all with live sessions.
        app.agents.insert(AgentId(0), make_agent(Some("sess-0")));
        app.agents.insert(AgentId(1), make_agent(Some("sess-1")));
        app.agents.insert(AgentId(2), make_agent(Some("sess-yolo")));
        app.auto_mode_gate = true;
        app.agents.get_mut(&AgentId(0)).unwrap().session.auto_mode = true;
        app.agents.get_mut(&AgentId(1)).unwrap().session.auto_mode = true;
        app.agents.get_mut(&AgentId(2)).unwrap().session.yolo_mode = true;

        let killswitch = acp::ExtNotification::new(
            "kigi/settings/update",
            serde_json::value::to_raw_value(
                &serde_json::json!({ "auto_permission_mode_enabled": false }),
            )
            .unwrap()
            .into(),
        );
        let _ = handle_ext_notification(&killswitch, &mut app);

        assert!(!app.auto_mode_gate, "gate must be off after kill-switch");
        // Sibling always-approve is untouched — the kill-switch clears only auto.
        assert!(
            app.agents[&AgentId(2)].session.is_yolo(),
            "sibling always-approve must stay yolo after the auto kill-switch"
        );

        let mut leave_auto_notifs = 0;
        while let Ok(msg) = rx.try_recv() {
            if let kigi_acp_lib::AcpAgentMessage::ExtNotification(args) = msg {
                if args.request.method.as_ref() != "kigi/yolo_mode_changed" {
                    continue;
                }
                let params: serde_json::Value =
                    serde_json::from_str(args.request.params.get()).unwrap();
                assert_eq!(params["auto_mode"], serde_json::json!(false));
                assert_eq!(params["permission_mode"], serde_json::json!("ask"));
                assert!(
                    params.get("yolo_mode").is_none(),
                    "yolo_mode must be omitted so a sibling always-approve session is preserved"
                );
                leave_auto_notifs += 1;
            }
        }
        assert_eq!(
            leave_auto_notifs, 1,
            "exactly one client-scoped leave-auto notification, regardless of agent count"
        );
    }

    /// User-owned mode must not re-arm default_yolo or rewrite UI from remote.
    #[test]
    fn permission_mode_user_claim_blocks_default_yolo_rearm() {
        let mut app = make_app_with_agent("sess-user-claim");
        app.auto_mode_gate = true;
        app.permission_mode_from_soft_default = false;
        app.current_ui.permission_mode = Some("ask".into());
        app.default_yolo = false;

        let apply_yolo = acp::ExtNotification::new(
            "kigi/settings/update",
            serde_json::value::to_raw_value(&serde_json::json!({
                "permission_mode": "always-approve",
            }))
            .unwrap()
            .into(),
        );
        let _ = handle_ext_notification(&apply_yolo, &mut app);
        assert!(
            !app.default_yolo,
            "user-claimed mode must not re-arm default_yolo from remote always-approve"
        );
        assert_eq!(
            app.current_ui.permission_mode.as_deref(),
            Some("ask"),
            "user-claimed UI must not be rewritten by remote soft-default"
        );
        assert!(
            !app.permission_mode_from_soft_default,
            "user claim origin stays false"
        );
    }

    #[test]
    fn permission_mode_omitted_does_not_clear_soft_default() {
        let mut app = make_app_with_agent("sess-omit-pm");
        app.permission_mode_from_soft_default = true;
        app.current_ui.permission_mode = Some("auto".into());
        app.default_yolo = false;
        app.auto_mode_gate = true;

        let unrelated = acp::ExtNotification::new(
            "kigi/settings/update",
            serde_json::value::to_raw_value(&serde_json::json!({
                "show_resolved_model": true,
            }))
            .unwrap()
            .into(),
        );
        let _ = handle_ext_notification(&unrelated, &mut app);
        assert_eq!(
            app.current_ui.permission_mode.as_deref(),
            Some("auto"),
            "omitted permission_mode must not clear soft-applied UI mode"
        );
        assert!(
            app.permission_mode_from_soft_default,
            "origin must stay SoftDefault when field is omitted"
        );
        assert!(
            !app.default_yolo,
            "omitted permission_mode must not recompute default_yolo"
        );
    }

    /// Positive wiring: a permission_mode-bearing push with the latch held
    /// must reach the applier through the real handler. The handler's ambient
    /// effective-config read decides WHICH mode wins (exact outcomes are
    /// pinned on the applier with injected TOML), so this asserts the
    /// applier's host-independent signature instead: the non-canonical
    /// sentinel display is rewritten to a canonical mode, latch preserved.
    #[test]
    fn permission_mode_soft_default_push_reaches_applier() {
        let mut app = make_app_with_agent("sess-wire-pm");
        app.auto_mode_gate = true;
        app.permission_mode_from_soft_default = true;
        // Outside the applier's output alphabet — only the applier rewrites it.
        app.current_ui.permission_mode = Some("sentinel-not-a-mode".into());

        let push = acp::ExtNotification::new(
            "kigi/settings/update",
            serde_json::value::to_raw_value(&serde_json::json!({
                "permission_mode": "always-approve",
            }))
            .unwrap()
            .into(),
        );
        let _ = handle_ext_notification(&push, &mut app);
        let display = app
            .current_ui
            .permission_mode
            .as_deref()
            .expect("applier always writes a display mode");
        assert!(
            matches!(display, "ask" | "auto" | "always-approve" | "default"),
            "soft push must rewrite the sentinel display via the applier, got {display:?}"
        );
        assert!(
            app.permission_mode_from_soft_default,
            "a soft re-arm must keep SoftDefault origin"
        );
    }

    /// Soft-origin recompute with injected TOML (deterministic — no host
    /// config): remote always-approve arms default_yolo + UI, keeps the soft
    /// latch, and persists nothing.
    #[test]
    fn permission_mode_soft_default_applies_remote_always_approve() {
        let mut app = make_app_with_agent("sess-pm");
        app.auto_mode_gate = true;
        app.permission_mode_from_soft_default = true;
        app.current_ui.permission_mode = None;
        app.default_yolo = false;

        super::super::settings::apply_soft_default_permission_mode(
            &mut app,
            None,
            Some("always-approve"),
        );
        assert!(app.default_yolo, "remote always-approve must arm default_yolo");
        assert_eq!(
            app.current_ui.permission_mode.as_deref(),
            Some("always-approve"),
        );
        assert!(
            app.permission_mode_from_soft_default,
            "a soft re-arm must keep SoftDefault origin"
        );
        assert!(
            app.pending_effects.is_empty(),
            "a soft default must never be persisted to disk"
        );
    }

    /// Explicit `null` recomputes with remote=None (unlike field omission):
    /// with no TOML permission key the soft always-approve drops back to Ask.
    #[test]
    fn permission_mode_explicit_null_clears_soft_always_approve() {
        let mut app = make_app_with_agent("sess-null-pm");
        app.auto_mode_gate = true;
        app.permission_mode_from_soft_default = true;
        app.current_ui.permission_mode = Some("always-approve".into());
        app.default_yolo = true;

        super::super::settings::apply_soft_default_permission_mode(&mut app, None, None);
        assert!(!app.default_yolo, "remote null must disarm a soft always-approve");
        assert_eq!(app.current_ui.permission_mode.as_deref(), Some("ask"));
        assert!(app.permission_mode_from_soft_default);
        assert!(
            app.pending_effects.is_empty(),
            "a soft default must never be persisted to disk"
        );
    }

    /// Policy pin and auto gate clamp a soft re-arm to Ask enforcement/display.
    #[test]
    fn permission_mode_soft_default_respects_pin_and_gate() {
        let mut app = make_app_with_agent("sess-pin-pm");
        app.permission_mode_from_soft_default = true;
        app.yolo_policy_block = Some("pinned");
        app.default_yolo = false;
        super::super::settings::apply_soft_default_permission_mode(
            &mut app,
            None,
            Some("always-approve"),
        );
        assert!(!app.default_yolo, "policy pin must block a remote always-approve");
        assert_eq!(app.current_ui.permission_mode.as_deref(), Some("ask"));

        let mut app = make_app_with_agent("sess-gate-pm");
        app.permission_mode_from_soft_default = true;
        app.auto_mode_gate = false;
        super::super::settings::apply_soft_default_permission_mode(&mut app, None, Some("auto"));
        assert!(!app.default_yolo);
        assert_eq!(
            app.current_ui.permission_mode.as_deref(),
            Some("ask"),
            "gated-off Auto must display as Ask"
        );
    }
