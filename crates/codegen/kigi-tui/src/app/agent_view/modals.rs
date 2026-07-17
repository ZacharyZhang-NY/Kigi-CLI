//! Modal input handlers: agents/persona modals and the extensions modal
//! (hooks, plugins, skills, MCP servers) with its actions.

use super::AgentView;
#[cfg(test)]
use super::test_fixtures;
use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crate::views::file_search::line_viewer::LineViewerState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl AgentView {
    // -- Agents modal input handling --

    pub(super) fn handle_agents_modal_key(
        &mut self,
        key: &crossterm::event::KeyEvent,
    ) -> InputOutcome {
        let Some(ref mut state) = self.agents_modal else {
            return InputOutcome::Unchanged;
        };
        match crate::views::agents_modal::handle_agents_key(state, key) {
            crate::views::agents_modal::AgentsModalOutcome::Close => {
                self.agents_modal = None;
                InputOutcome::Changed
            }
            crate::views::agents_modal::AgentsModalOutcome::ViewAgent {
                title,
                source_path,
                content,
            } => {
                // Open the agent definition in the line viewer on top of the
                // agents modal.  The line viewer has higher input priority
                // (0a1) so it takes focus; when the user presses Esc the
                // viewer closes and the agents modal is still there.
                let viewer = if let Some(ref path) = source_path {
                    LineViewerState::open_markdown(path, None)
                } else if let Some(content) = content {
                    LineViewerState::open_markdown_content(&title, content, None)
                } else {
                    None
                };
                if let Some(mut v) = viewer {
                    v.title_override = Some(title);
                    self.line_viewer = Some(v);
                }
                InputOutcome::Changed
            }
            crate::views::agents_modal::AgentsModalOutcome::OpenPersonaDetail {
                name,
                source_path,
                editable,
                scope_label,
            } => {
                use crate::views::persona_detail::PersonaDetailState;
                let detail = if let Some(ref path) = source_path {
                    PersonaDetailState::from_toml_file(path, editable, &scope_label)
                } else {
                    Some(PersonaDetailState::from_name_only(&name))
                };
                if detail.is_none()
                    && let Some(ref mut modal) = self.agents_modal
                {
                    modal.message = Some(crate::views::agents_modal::AgentsModalMessage::error(
                        format!("Failed to load persona '{name}'"),
                    ));
                }
                self.persona_detail = detail;
                InputOutcome::Changed
            }
            crate::views::agents_modal::AgentsModalOutcome::EditInEditor { path, tab } => {
                InputOutcome::Action(Action::SuspendForEditor {
                    path,
                    refresh_agents_modal: Some(tab),
                })
            }
            crate::views::agents_modal::AgentsModalOutcome::Changed => InputOutcome::Changed,
            crate::views::agents_modal::AgentsModalOutcome::Unchanged => InputOutcome::Unchanged,
        }
    }

    pub(super) fn handle_agents_modal_mouse(
        &mut self,
        mouse: &crossterm::event::MouseEvent,
    ) -> InputOutcome {
        let Some(ref mut state) = self.agents_modal else {
            return InputOutcome::Unchanged;
        };
        match crate::views::agents_modal::handle_agents_mouse(state, mouse) {
            crate::views::agents_modal::AgentsModalOutcome::Close => {
                self.agents_modal = None;
                InputOutcome::Changed
            }
            crate::views::agents_modal::AgentsModalOutcome::ViewAgent { .. }
            | crate::views::agents_modal::AgentsModalOutcome::OpenPersonaDetail { .. }
            | crate::views::agents_modal::AgentsModalOutcome::EditInEditor { .. } => {
                // Mouse interactions don't trigger view/edit — ignore.
                InputOutcome::Unchanged
            }
            crate::views::agents_modal::AgentsModalOutcome::Changed => InputOutcome::Changed,
            crate::views::agents_modal::AgentsModalOutcome::Unchanged => InputOutcome::Unchanged,
        }
    }

    // -- Persona detail modal input handling --

    pub(super) fn handle_persona_detail_key(
        &mut self,
        key: &crossterm::event::KeyEvent,
    ) -> InputOutcome {
        let Some(ref mut detail) = self.persona_detail else {
            return InputOutcome::Unchanged;
        };
        use crate::views::persona_detail::{PersonaDetailOutcome, handle_persona_detail_key};
        match handle_persona_detail_key(detail, key) {
            PersonaDetailOutcome::Close => {
                self.persona_detail = None;
                // Refresh the personas list in case edits were made.
                if let Some(ref mut modal) = self.agents_modal {
                    modal.refresh_personas();
                }
                InputOutcome::Changed
            }
            PersonaDetailOutcome::EditInEditor { path } => {
                self.persona_detail = None;
                InputOutcome::Action(Action::SuspendForEditor {
                    path,
                    refresh_agents_modal: Some(crate::views::agents_modal::AgentsTab::Personas),
                })
            }
            PersonaDetailOutcome::Changed => InputOutcome::Changed,
            PersonaDetailOutcome::Unchanged => InputOutcome::Unchanged,
        }
    }

    pub(super) fn handle_persona_detail_mouse(
        &mut self,
        mouse: &crossterm::event::MouseEvent,
    ) -> InputOutcome {
        let Some(ref mut detail) = self.persona_detail else {
            return InputOutcome::Unchanged;
        };
        use crate::views::persona_detail::{PersonaDetailOutcome, handle_persona_detail_mouse};
        match handle_persona_detail_mouse(detail, mouse) {
            PersonaDetailOutcome::Close => {
                self.persona_detail = None;
                if let Some(ref mut modal) = self.agents_modal {
                    modal.refresh_personas();
                }
                InputOutcome::Changed
            }
            PersonaDetailOutcome::Changed => InputOutcome::Changed,
            PersonaDetailOutcome::EditInEditor { .. } | PersonaDetailOutcome::Unchanged => {
                InputOutcome::Unchanged
            }
        }
    }

    // -- Hooks/plugins modal input handling --

    pub(super) fn handle_extensions_modal_key(
        &mut self,
        key: &crossterm::event::KeyEvent,
    ) -> InputOutcome {
        // Handle modal messages (errors and confirmations) FIRST, before
        // the pending_action guard. Some error paths (e.g. structured
        // OutcomeStatus::ValidationError) leave pending_action set when
        // they raise the error, so the in-flight guard would otherwise
        // swallow every key and prevent the user from dismissing the
        // error.
        if self
            .extensions_modal
            .as_ref()
            .is_some_and(|s| s.modal_message.is_some())
        {
            if let Some(ref mut state) = self.extensions_modal {
                use crate::views::extensions_modal::ModalMessage;
                match (&state.modal_message, key.code) {
                    // Confirmation: y confirms, anything else dismisses.
                    (Some(ModalMessage::Confirmation { action, .. }), KeyCode::Char('y')) => {
                        let action = action.clone();
                        state.modal_message = None;
                        return self.execute_modal_button_action(
                            crate::views::extensions_modal::ButtonAction::PluginsAction(action),
                        );
                    }
                    _ => {
                        // Dismissing the error/confirmation also clears
                        // the in-flight "[processing]" badge — the action
                        // is done and the user has acknowledged.
                        state.modal_message = None;
                        state.pending_action = None;
                        state.pending_entry_index = None;
                    }
                }
            }
            return InputOutcome::Changed;
        }

        // Block all action keys while an action is in-flight (no error
        // overlay is showing — that case is handled above). Esc clears
        // the pending indicator (auth continues in background) but keeps
        // the modal open so the user can navigate or retry.
        if self
            .extensions_modal
            .as_ref()
            .is_some_and(|s| s.pending_action.is_some())
        {
            return match key.code {
                KeyCode::Esc => {
                    if let Some(ref mut state) = self.extensions_modal {
                        state.pending_action = None;
                        state.pending_entry_index = None;
                    }
                    InputOutcome::Changed
                }
                _ => InputOutcome::Changed,
            };
        }

        // If in input mode, route to input handler.
        if self
            .extensions_modal
            .as_ref()
            .is_some_and(|s| s.input.is_some())
        {
            return self.handle_modal_input_key(key);
        }

        // Ctrl+O opens grok.com connectors on the MCP servers tab.
        if key.code == KeyCode::Char('o')
            && key.modifiers == KeyModifiers::CONTROL
            && self.extensions_modal.as_ref().is_some_and(|s| {
                s.active_tab == crate::views::extensions_modal::ExtensionsTab::McpServers
                    && !s.picker_state.search_active
            })
        {
            return self.execute_modal_button_action(
                crate::views::extensions_modal::ButtonAction::OpenManagedConnectors,
            );
        }

        // Route chrome keys through ModalWindow first (mirrors the mouse path).
        // Handles Esc -> CloseRequested and h/l (or L/R when not tabs-focused)
        // -> fold outcomes when FoldInfo provided.
        // When the tab bar has been focused via Up/Down (`window.tabs_focused`),
        // Left/Right are left as Unhandled here so they reach picker input
        // which cycles tabs only while the tab list is selected.
        // This restores default L/R = expand/collapse on the selected item
        // unless the user has explicitly navigated focus to the tabs with arrows.
        {
            let state = self.extensions_modal.as_mut().unwrap();
            let labels: Vec<&str> = crate::views::extensions_modal::ExtensionsTab::ALL
                .iter()
                .map(|t| t.label())
                .collect();
            // Build FoldInfo from the focused entry's state. When search is
            // active or the tab bar is focused via Up/Down, fold_info is None
            // so h/l/L/R return Unhandled and fall through (picker handles
            // tabs or search cursor for arrows; L/R on content do expand/collapse).
            let fold_info = if state.picker_state.search_active || state.window.tabs_focused {
                None
            } else {
                let sel = state.picker_state.selected;
                if state
                    .entry_non_selectable
                    .get(sel)
                    .copied()
                    .unwrap_or(false)
                {
                    None
                } else {
                    let group_key = state
                        .entry_group_keys
                        .get(sel)
                        .and_then(|k| k.as_ref())
                        .cloned();
                    if let Some(ref gk) = group_key {
                        let is_expanded = state.is_group_expanded(sel, gk);
                        Some(crate::views::modal_window::FoldInfo {
                            collapsible: true,
                            expanded: is_expanded,
                            has_details: false,
                            details_expanded: false,
                            // Group headers are top-level in the extensions
                            // modal (no nesting).
                            parent_index: None,
                        })
                    } else {
                        // Leaf item: can have expandable detail fields.
                        let details_expanded = state.picker_state.expanded.contains(&sel);
                        let parent = (0..sel).rev().find(|&i| {
                            state
                                .entry_group_keys
                                .get(i)
                                .and_then(|k| k.as_ref())
                                .is_some()
                        });
                        Some(crate::views::modal_window::FoldInfo {
                            collapsible: false,
                            expanded: false,
                            has_details: true,
                            details_expanded,
                            parent_index: parent,
                        })
                    }
                }
            };
            let config = crate::views::modal_window::ModalWindowConfig {
                // Empty title — matches the renderer in extensions_modal.rs
                // which uses the tab bar to identify the modal contents.
                // Keep these in sync so future changes to handle_modal_key
                // that read `title` (e.g. for accessibility announcements)
                // see the same value the user sees.
                title: "",
                tabs: Some(&labels),
                shortcuts: &[],
                sizing: crate::views::modal_window::ModalSizing::default(),
                fold_info,
            };
            let outcome =
                crate::views::modal_window::handle_modal_key(&mut state.window, key, &config);
            match outcome {
                crate::views::modal_window::ModalWindowOutcome::CloseRequested => {
                    if state.picker_state.query.is_empty() && !state.picker_state.search_active {
                        self.extensions_modal = None;
                        return InputOutcome::Changed;
                    }
                }
                crate::views::modal_window::ModalWindowOutcome::CollapseGroup => {
                    let sel = state.picker_state.selected;
                    if let Some(gk) = state
                        .entry_group_keys
                        .get(sel)
                        .and_then(|k| k.as_ref())
                        .cloned()
                    {
                        self.extensions_modal_set_collapsed(sel, &gk, true);
                    }
                    return InputOutcome::Changed;
                }
                crate::views::modal_window::ModalWindowOutcome::ExpandGroup => {
                    let sel = state.picker_state.selected;
                    if let Some(gk) = state
                        .entry_group_keys
                        .get(sel)
                        .and_then(|k| k.as_ref())
                        .cloned()
                    {
                        if state.mcp_auth_intercept_on_expand() {
                            return self.execute_modal_button_action(
                                crate::views::extensions_modal::ButtonAction::McpAuthTrigger,
                            );
                        }
                        self.extensions_modal_set_collapsed(sel, &gk, false);
                    }
                    return InputOutcome::Changed;
                }
                crate::views::modal_window::ModalWindowOutcome::CollapseDetails => {
                    let sel = state.picker_state.selected;
                    state.picker_state.expanded.remove(&sel);
                    state.picker_state.scroll_offset = None;
                    return InputOutcome::Changed;
                }
                crate::views::modal_window::ModalWindowOutcome::ExpandDetails => {
                    let sel = state.picker_state.selected;
                    state.picker_state.expanded.insert(sel);
                    state.picker_state.scroll_offset = None;
                    return InputOutcome::Changed;
                }
                crate::views::modal_window::ModalWindowOutcome::JumpToParent(idx) => {
                    state.picker_state.selected = idx;
                    state.picker_state.scroll_offset = None;
                    return InputOutcome::Changed;
                }
                _ => {
                    // Unhandled and other outcomes fall through to picker.
                }
            }
        }

        // Delegate navigation/search/tab/filter/action to handle_picker_input.
        let Some(state) = self.extensions_modal.as_mut() else {
            return InputOutcome::Changed;
        };

        // Build the same config as the renderer.
        let labels: Vec<&str> = crate::views::extensions_modal::ExtensionsTab::ALL
            .iter()
            .map(|t| t.label())
            .collect();
        let active_idx = crate::views::extensions_modal::ExtensionsTab::ALL
            .iter()
            .position(|t| *t == state.active_tab)
            .unwrap_or(0);
        let has_filter = matches!(
            state.active_tab,
            crate::views::extensions_modal::ExtensionsTab::Hooks
                | crate::views::extensions_modal::ExtensionsTab::Plugins
                | crate::views::extensions_modal::ExtensionsTab::McpServers
        );
        let filter = match state.active_tab {
            crate::views::extensions_modal::ExtensionsTab::Hooks => state.hooks_filter,
            crate::views::extensions_modal::ExtensionsTab::Plugins => state.plugins_filter,
            crate::views::extensions_modal::ExtensionsTab::McpServers => state.mcps_filter,
            _ => crate::views::extensions_modal::StatusFilter::All,
        };
        let action_keys = crate::views::extensions_modal::extensions_action_keys(state.active_tab);
        let entry_count = state.entry_data_indices.len();
        let non_selectable_owned = Self::extensions_modal_non_selectable_mask(state, entry_count);
        let non_selectable = &non_selectable_owned;
        let clickable_owned =
            Self::extensions_modal_non_selectable_clickable_mask(state, entry_count);
        let non_selectable_clickable = &clickable_owned;

        let config = crate::views::picker::PickerConfig {
            title: None,
            show_search_hint: true,
            expandable: true,
            esc_clears_query: true,
            shortcuts: Some(crate::views::picker::picker_shortcuts()),
            pending_hint: None,
            shortcuts_area: None,
            non_selectable,
            non_selectable_clickable,
            tabs: Some(&labels),
            active_tab: active_idx,
            filter_label: if has_filter {
                Some(filter.label())
            } else {
                None
            },
            filter_key_hint: if has_filter { Some("f") } else { None },
            filter_active: filter != crate::views::extensions_modal::StatusFilter::All,
            action_keys: &action_keys,
            disable_search: false,
            compact_bottom_bar: false,
            // Skills-tab letters double as quick keys today and the
            // tab feels noisy when typing a single letter immediately
            // commits a query. Require explicit `/` (or click) to
            // activate search there.
            search_only_on_slash: state.active_tab
                == crate::views::extensions_modal::ExtensionsTab::Skills,
            vim_normal_first: crate::appearance::cache::load_vim_mode(),
        };

        let ev = crossterm::event::Event::Key(*key);
        let outcome = crate::views::picker::handle_picker_input(
            &ev,
            &mut state.picker_state,
            entry_count,
            &config,
        );

        // Search state now lives directly in picker_state (no sync needed).

        match outcome {
            crate::views::picker::PickerOutcome::Closed => {
                self.extensions_modal = None;
                InputOutcome::Changed
            }
            crate::views::picker::PickerOutcome::TabChanged(idx) => {
                if let Some(ref mut state) = self.extensions_modal
                    && let Some(&tab) = crate::views::extensions_modal::ExtensionsTab::ALL.get(idx)
                {
                    // switch_tab also clears the Add form, error
                    // overlay, and pending [processing] badge so the
                    // new tab opens in a clean browse view.
                    state.switch_tab(tab);
                    state.window.tabs_focused = state.picker_state.tabs_focused;
                }
                InputOutcome::Changed
            }
            crate::views::picker::PickerOutcome::FilterCycled => {
                if let Some(ref mut state) = self.extensions_modal {
                    match state.active_tab {
                        crate::views::extensions_modal::ExtensionsTab::Hooks => {
                            state.hooks_filter = state.hooks_filter.next();
                        }
                        crate::views::extensions_modal::ExtensionsTab::Plugins => {
                            state.plugins_filter = state.plugins_filter.next();
                        }
                        crate::views::extensions_modal::ExtensionsTab::McpServers => {
                            state.mcps_filter = state.mcps_filter.next();
                        }
                        _ => {}
                    }
                    // Reset selection after filter change.
                    state.picker_state.selected = 0;
                    state.picker_state.scroll_offset = None;
                    state.picker_state.tabs_focused = false;
                }
                InputOutcome::Changed
            }
            crate::views::picker::PickerOutcome::Action(ch) => {
                if let Some(action) = self
                    .extensions_modal
                    .as_ref()
                    .and_then(|s| crate::views::extensions_modal::resolve_key(s.active_tab, ch))
                {
                    self.execute_modal_button_action(action)
                } else {
                    InputOutcome::Changed
                }
            }
            crate::views::picker::PickerOutcome::Selected(_)
            | crate::views::picker::PickerOutcome::Expand(_) => {
                self.extensions_modal_expand_or_auth()
            }
            crate::views::picker::PickerOutcome::Collapse(_) => {
                self.extensions_modal_toggle_fold();
                InputOutcome::Changed
            }
            crate::views::picker::PickerOutcome::NonSelectableClick(idx) => {
                self.extensions_modal_toggle_mcp_section_at(idx);
                InputOutcome::Changed
            }
            crate::views::picker::PickerOutcome::Copy(_) => InputOutcome::Changed,
            crate::views::picker::PickerOutcome::SubmitQuery => InputOutcome::Changed,
            crate::views::picker::PickerOutcome::Changed => InputOutcome::Changed,
            crate::views::picker::PickerOutcome::Unchanged => InputOutcome::Unchanged,
        }
    }

    /// Handle key events while the modal is in input mode (text field active).
    fn handle_modal_input_key(&mut self, key: &KeyEvent) -> InputOutcome {
        use crate::views::extensions_modal::ModalInputOutcome;

        let Some(ref mut state) = self.extensions_modal else {
            return InputOutcome::Unchanged;
        };
        let Some(ref mut input) = state.input else {
            return InputOutcome::Unchanged;
        };

        match input.handle_key(key) {
            ModalInputOutcome::Changed => InputOutcome::Changed,
            ModalInputOutcome::Unchanged => InputOutcome::Unchanged,
            ModalInputOutcome::Cancel => {
                state.input = None;
                InputOutcome::Changed
            }
            ModalInputOutcome::Submit {
                command_prefix,
                field_texts,
            } => {
                state.input = None;
                if let Some(action) = crate::views::extensions_modal::build_action_from_input(
                    &command_prefix,
                    &field_texts,
                ) {
                    self.execute_modal_button_action(action)
                } else {
                    InputOutcome::Changed
                }
            }
        }
    }

    /// Handle a bracketed-paste event while the hooks/plugins modal is open.
    ///
    /// Routes pasted text to the inline input field (when active) or the
    /// search query (when search mode is active). Without this, the native
    /// paste shortcut (Cmd-V / Shift-Insert) is swallowed because the modal
    /// intercept only routes `Event::Key` and `Event::Mouse` by default.
    pub(super) fn handle_extensions_modal_paste(&mut self, text: &str) -> InputOutcome {
        if let Some(ref mut state) = self.extensions_modal
            && state.apply_paste(text)
        {
            InputOutcome::Changed
        } else {
            InputOutcome::Unchanged
        }
    }

    /// Handle a mouse event while the hooks/plugins modal is open.
    ///
    /// - Clicks on tabs switch the active tab.
    /// - Clicks outside the popup close it.
    /// - Everything else is consumed.
    pub(super) fn handle_extensions_modal_mouse(
        &mut self,
        mouse: &crossterm::event::MouseEvent,
    ) -> InputOutcome {
        use crossterm::event::MouseEventKind;

        // Route chrome events (close button, tabs, click-outside) through
        // the shared ModalWindow handler first.
        let chrome_shortcut_ch: Option<char> = {
            let state = self.extensions_modal.as_mut().unwrap();
            let outcome = crate::views::modal_window::handle_modal_mouse(
                &mut state.window,
                mouse.kind,
                mouse.column,
                mouse.row,
            );
            match outcome {
                crate::views::modal_window::ModalWindowOutcome::CloseRequested => {
                    self.extensions_modal = None;
                    return InputOutcome::Changed;
                }
                crate::views::modal_window::ModalWindowOutcome::TabChanged(idx) => {
                    if let Some(&tab) = crate::views::extensions_modal::ExtensionsTab::ALL.get(idx)
                    {
                        // Clears Add form, error overlay, and pending
                        // badge in addition to resetting picker state.
                        state.switch_tab(tab);
                        // Clicking a tab implies interaction with the tab list;
                        // show the focused highlight and keep arrow nav on tabs.
                        state.picker_state.tabs_focused = true;
                        state.window.tabs_focused = true;
                    }
                    return InputOutcome::Changed;
                }
                crate::views::modal_window::ModalWindowOutcome::Handled => {
                    return InputOutcome::Changed;
                }
                crate::views::modal_window::ModalWindowOutcome::ShortcutActivated(id) => {
                    // Footer shortcut IDs 100+ map to action_keys.
                    // Resolve the char here; dispatch after the borrow
                    // is released so execute_modal_button_action can
                    // take &mut self.
                    if id == 98 {
                        // "Tab/Shift+Tab tabs" hint — cycle to the next
                        // tab, mirroring the Tab keypress flow.
                        let all = crate::views::extensions_modal::ExtensionsTab::ALL;
                        let cur = all.iter().position(|&t| t == state.active_tab).unwrap_or(0);
                        let next = (cur + 1) % all.len();
                        if let Some(&tab) = all.get(next) {
                            // Clears Add form, error overlay, and
                            // pending badge in addition to resetting
                            // picker state.
                            state.switch_tab(tab);
                            state.picker_state.tabs_focused = true;
                            state.window.tabs_focused = true;
                        }
                        return InputOutcome::Changed;
                    } else if id == 99 {
                        // "Esc close" shortcut — signal close via sentinel.
                        Some('\x00')
                    } else if id >= 100 {
                        let keys = crate::views::extensions_modal::extensions_action_keys(
                            state.active_tab,
                        );
                        keys.get(id - 100).map(|&(ch, _)| ch)
                    } else {
                        None
                    }
                }
                _ => None, // Unhandled — fall through to picker
            }
        };
        // Dispatch shortcut click (if any) now that the &mut borrow is released.
        if let Some(ch) = chrome_shortcut_ch {
            if ch == '\x00' {
                // "Esc close" shortcut clicked.
                self.extensions_modal = None;
                return InputOutcome::Changed;
            }
            // Block action shortcuts while an action is in-flight
            // (mirrors the keyboard guard in handle_extensions_modal_key).
            if self
                .extensions_modal
                .as_ref()
                .is_some_and(|s| s.pending_action.is_some())
            {
                return InputOutcome::Changed;
            }
            if let Some(action) = self
                .extensions_modal
                .as_ref()
                .and_then(|s| crate::views::extensions_modal::resolve_key(s.active_tab, ch))
            {
                return self.execute_modal_button_action(action);
            }
            return InputOutcome::Changed;
        }

        let Some(ref mut state) = self.extensions_modal else {
            return InputOutcome::Changed;
        };

        // Modal overlay covers picker rows but not their hit-rects: dismiss
        // on any mouse-down so a click-through doesn't re-trigger the row
        // underneath (which can re-fire OAuth on [needs auth] rows).
        if state.modal_message.is_some()
            && matches!(
                mouse.kind,
                MouseEventKind::Down(crossterm::event::MouseButton::Left)
                    | MouseEventKind::Down(crossterm::event::MouseButton::Right)
                    | MouseEventKind::Down(crossterm::event::MouseButton::Middle)
            )
        {
            // Mirror the keyboard dismissal path: clearing the
            // error/confirmation also clears the in-flight
            // "[processing]" badge so the mouse and keyboard paths
            // agree on what dismiss means.
            state.modal_message = None;
            state.pending_action = None;
            state.pending_entry_index = None;
            return InputOutcome::Changed;
        }

        // Build the same config as the renderer/key handler.
        let labels: Vec<&str> = crate::views::extensions_modal::ExtensionsTab::ALL
            .iter()
            .map(|t| t.label())
            .collect();
        let active_idx = crate::views::extensions_modal::ExtensionsTab::ALL
            .iter()
            .position(|t| *t == state.active_tab)
            .unwrap_or(0);
        let has_filter = matches!(
            state.active_tab,
            crate::views::extensions_modal::ExtensionsTab::Hooks
                | crate::views::extensions_modal::ExtensionsTab::Plugins
                | crate::views::extensions_modal::ExtensionsTab::McpServers
        );
        let filter = match state.active_tab {
            crate::views::extensions_modal::ExtensionsTab::Hooks => state.hooks_filter,
            crate::views::extensions_modal::ExtensionsTab::Plugins => state.plugins_filter,
            crate::views::extensions_modal::ExtensionsTab::McpServers => state.mcps_filter,
            _ => crate::views::extensions_modal::StatusFilter::All,
        };
        let action_keys: Vec<(char, &str)> = vec![]; // No action keys for mouse
        let entry_count = state.entry_data_indices.len();
        let non_selectable_owned = Self::extensions_modal_non_selectable_mask(state, entry_count);
        let non_selectable = &non_selectable_owned;
        let clickable_owned =
            Self::extensions_modal_non_selectable_clickable_mask(state, entry_count);
        let non_selectable_clickable = &clickable_owned;

        let config = crate::views::picker::PickerConfig {
            title: None,
            show_search_hint: true,
            expandable: true,
            esc_clears_query: true,
            shortcuts: Some(crate::views::picker::picker_shortcuts()),
            pending_hint: None,
            shortcuts_area: None,
            non_selectable,
            non_selectable_clickable,
            tabs: Some(&labels),
            active_tab: active_idx,
            filter_label: if has_filter {
                Some(filter.label())
            } else {
                None
            },
            filter_key_hint: if has_filter { Some("f") } else { None },
            filter_active: filter != crate::views::extensions_modal::StatusFilter::All,
            action_keys: &action_keys,
            disable_search: false,
            compact_bottom_bar: false,
            // Same gate as the keyboard handler — keep behavior
            // consistent so a mouse-driven tab switch doesn't change
            // the typing semantics on Skills.
            search_only_on_slash: state.active_tab
                == crate::views::extensions_modal::ExtensionsTab::Skills,
            vim_normal_first: crate::appearance::cache::load_vim_mode(),
        };

        let ev = crossterm::event::Event::Mouse(*mouse);
        let outcome = crate::views::picker::handle_picker_input(
            &ev,
            &mut state.picker_state,
            entry_count,
            &config,
        );

        // Open the connectors URL on mouse-down (parity with Ctrl+O). A section-row
        // click routes as Selected or NonSelectableClick, so intercept both here.
        let clicked_entry = match &outcome {
            crate::views::picker::PickerOutcome::Selected(i)
            | crate::views::picker::PickerOutcome::Expand(i)
            | crate::views::picker::PickerOutcome::NonSelectableClick(i) => Some(*i),
            _ => None,
        };
        if let Some(idx) = clicked_entry
            && self.extensions_modal_click_opens_connectors(idx, mouse.row)
        {
            return self.execute_modal_button_action(
                crate::views::extensions_modal::ButtonAction::OpenManagedConnectors,
            );
        }

        // Hover states are managed by ModalWindow (close) and picker (filter).

        match outcome {
            crate::views::picker::PickerOutcome::Closed => {
                self.extensions_modal = None;
                InputOutcome::Changed
            }
            crate::views::picker::PickerOutcome::TabChanged(idx) => {
                if let Some(ref mut state) = self.extensions_modal
                    && let Some(&tab) = crate::views::extensions_modal::ExtensionsTab::ALL.get(idx)
                {
                    // Clears Add form, error overlay, and pending
                    // badge in addition to resetting picker state.
                    state.switch_tab(tab);
                    // Mouse-driven tab switch via picker hit area → treat tabs as focused.
                    state.picker_state.tabs_focused = true;
                    state.window.tabs_focused = true;
                }
                InputOutcome::Changed
            }
            crate::views::picker::PickerOutcome::FilterCycled => {
                if let Some(ref mut state) = self.extensions_modal {
                    match state.active_tab {
                        crate::views::extensions_modal::ExtensionsTab::Hooks => {
                            state.hooks_filter = state.hooks_filter.next();
                        }
                        crate::views::extensions_modal::ExtensionsTab::Plugins => {
                            state.plugins_filter = state.plugins_filter.next();
                        }
                        crate::views::extensions_modal::ExtensionsTab::McpServers => {
                            state.mcps_filter = state.mcps_filter.next();
                        }
                        _ => {}
                    }
                    state.picker_state.selected = 0;
                    state.picker_state.scroll_offset = None;
                }
                InputOutcome::Changed
            }
            crate::views::picker::PickerOutcome::Selected(_)
            | crate::views::picker::PickerOutcome::Expand(_) => {
                self.extensions_modal_expand_or_auth()
            }
            crate::views::picker::PickerOutcome::NonSelectableClick(idx) => {
                self.extensions_modal_toggle_mcp_section_at(idx);
                InputOutcome::Changed
            }
            crate::views::picker::PickerOutcome::Changed => InputOutcome::Changed,
            crate::views::picker::PickerOutcome::Unchanged => InputOutcome::Unchanged,
            _ => InputOutcome::Changed,
        }
    }

    /// Toggle fold on an MCP section header row (clicked, not keyboard-selected).
    fn extensions_modal_toggle_mcp_section_at(&mut self, entry_idx: usize) {
        let Some(ref mut state) = self.extensions_modal else {
            return;
        };
        if state.active_tab != crate::views::extensions_modal::ExtensionsTab::McpServers {
            return;
        }
        let Some(gk) = state
            .entry_group_keys
            .get(entry_idx)
            .and_then(|k| k.as_ref())
            .map(|s| s.as_str())
        else {
            return;
        };
        if !gk.starts_with("mcp-section:") {
            return;
        }
        // Toggle: `remove` returns true when the section was collapsed (now
        // expanded); otherwise it was expanded, so collapse it.
        if !state.mcps_collapsed_sections.remove(gk) {
            state.mcps_collapsed_sections.insert(gk.to_string());
        }
        state.picker_state.scroll_offset = None;
    }

    /// Whether a click at `mouse_row` on entry `entry_idx` hit the connectors URL
    /// link band recorded at last paint (opens the URL instead of folding).
    fn extensions_modal_click_opens_connectors(&self, entry_idx: usize, mouse_row: u16) -> bool {
        self.extensions_modal.as_ref().is_some_and(|state| {
            // Parity with the Ctrl+O guard: don't open while the search bar has focus.
            !state.picker_state.search_active
                && state
                    .picker_state
                    .link_band
                    .as_ref()
                    .is_some_and(|(idx, band)| *idx == entry_idx && band.contains(&mouse_row))
        })
    }

    /// Non-selectable mask for the extensions modal picker (from last render).
    fn extensions_modal_non_selectable_mask(
        state: &crate::views::extensions_modal::ExtensionsModalState,
        entry_count: usize,
    ) -> Vec<bool> {
        if state.entry_non_selectable.len() == entry_count {
            return state.entry_non_selectable.clone();
        }
        (0..entry_count)
            .map(|i| {
                state
                    .entry_group_keys
                    .get(i)
                    .and_then(|k| k.as_deref())
                    .is_some_and(|k| k.starts_with("mcp-section:"))
            })
            .collect()
    }

    fn extensions_modal_non_selectable_clickable_mask(
        state: &crate::views::extensions_modal::ExtensionsModalState,
        entry_count: usize,
    ) -> Vec<bool> {
        if state.entry_non_selectable_clickable.len() == entry_count {
            return state.entry_non_selectable_clickable.clone();
        }
        (0..entry_count)
            .map(|i| {
                state
                    .entry_group_keys
                    .get(i)
                    .and_then(|k| k.as_deref())
                    .is_some_and(|k| k.starts_with("mcp-section:"))
            })
            .collect()
    }

    /// Expand/collapse the selected row, or trigger MCP OAuth when the server needs auth.
    fn extensions_modal_expand_or_auth(&mut self) -> InputOutcome {
        if self
            .extensions_modal
            .as_ref()
            .is_some_and(|s| s.mcp_auth_intercept_on_expand())
        {
            return self.execute_modal_button_action(
                crate::views::extensions_modal::ButtonAction::McpAuthTrigger,
            );
        }
        self.extensions_modal_toggle_fold();
        InputOutcome::Changed
    }

    /// Toggle the fold state of the selected entry in the extensions modal.
    ///
    /// Used by Enter/click/space to toggle expand/collapse. Group headers
    /// toggle their collapsed state; leaf items toggle detail-field expansion.
    fn extensions_modal_toggle_fold(&mut self) {
        let Some(ref mut state) = self.extensions_modal else {
            return;
        };
        let sel = state.picker_state.selected;
        let group_key = state
            .entry_group_keys
            .get(sel)
            .and_then(|k| k.as_ref())
            .cloned();

        if let Some(gk) = group_key {
            let is_expanded = state.is_group_expanded(sel, &gk);
            // `set_collapsed`'s third arg is the NEW collapsed state.
            // When currently expanded → new state is collapsed (true);
            // when currently collapsed → new state is expanded (false).
            // That value equals `is_expanded` directly. Using `!is_expanded`
            // (the previous code) made `e`/Enter/Space/click into a no-op
            // for every collapsible header (MCP servers and hooks
            // groups).
            self.extensions_modal_set_collapsed(sel, &gk, is_expanded);
        } else {
            // Leaf item: toggle detail fields.
            state.picker_state.scroll_offset = None;
            if !state.picker_state.expanded.remove(&sel) {
                state.picker_state.expanded.insert(sel);
            }
        }
    }

    /// Set the collapsed state for a group key in the extensions modal.
    fn extensions_modal_set_collapsed(
        &mut self,
        sel: usize,
        group_key: &str,
        collapsed: bool,
    ) -> bool {
        let Some(ref mut state) = self.extensions_modal else {
            return false;
        };
        state.picker_state.scroll_offset = None;
        match state.active_tab {
            crate::views::extensions_modal::ExtensionsTab::Hooks => {
                if collapsed {
                    state.hooks_collapsed_groups.insert(group_key.to_string())
                } else {
                    state.hooks_collapsed_groups.remove(group_key)
                }
            }
            crate::views::extensions_modal::ExtensionsTab::Plugins => {
                if collapsed {
                    state.plugins_collapsed_groups.insert(group_key.to_string())
                } else {
                    state.plugins_collapsed_groups.remove(group_key)
                }
            }
            crate::views::extensions_modal::ExtensionsTab::McpServers => {
                if group_key.starts_with("mcp-section:") {
                    if collapsed {
                        state.mcps_collapsed_sections.insert(group_key.to_string())
                    } else {
                        state.mcps_collapsed_sections.remove(group_key)
                    }
                } else if let Some(si) =
                    crate::views::extensions_modal::parse_mcp_tools_server_index(group_key)
                {
                    if collapsed {
                        state.mcps_tools_expanded.remove(&si)
                    } else {
                        state.mcps_tools_expanded.insert(si)
                    }
                } else {
                    false
                }
            }
            _ => {
                if collapsed {
                    state.picker_state.expanded.remove(&sel)
                } else {
                    state.picker_state.expanded.insert(sel)
                }
            }
        }
    }

    /// Execute a modal button action — dispatches to ACP effect.
    fn execute_modal_button_action(
        &mut self,
        action: crate::views::extensions_modal::ButtonAction,
    ) -> InputOutcome {
        use crate::views::extensions_modal::{ButtonAction, ModalInput, TabDataState};

        // A new user-initiated action supersedes any lingering result notice.
        // The chained auto-reload goes through `Effect`, not here, so it keeps
        // the triggering action's notice (see `dispatch_action_result`).
        if let Some(ref mut state) = self.extensions_modal {
            state.result_notice = None;
        }

        match action {
            ButtonAction::HooksAction(hooks_action) => {
                if let Some(ref mut state) = self.extensions_modal {
                    state.modal_message = None;
                    if matches!(hooks_action, kigi_hooks_plugins_types::HooksAction::Reload) {
                        // Reload rebuilds the entire plugin registry -- show
                        // tab-level "Loading..." instead of a single-entry badge.
                        state.pending_action = Some("Reloading...".into());
                        state.pending_entry_index = None;
                        state.hooks_data = TabDataState::Loading;
                        state.plugins_data = TabDataState::Loading;
                    } else {
                        state.pending_action = Some("Processing...".into());
                        state.pending_entry_index = Some(state.picker_state.selected);
                    }
                }
                InputOutcome::Action(Action::ExecuteHooksAction(hooks_action))
            }
            ButtonAction::PluginsAction(plugins_action) => {
                if let Some(ref mut state) = self.extensions_modal {
                    state.modal_message = None;
                    state.last_plugins_action = Some(plugins_action.clone());
                    if matches!(
                        plugins_action,
                        kigi_hooks_plugins_types::PluginsAction::Reload
                    ) {
                        // Reload rebuilds the entire plugin registry -- show
                        // tab-level "Loading..." instead of a single-entry badge.
                        state.pending_action = Some("Reloading...".into());
                        state.pending_entry_index = None;
                        state.plugins_data = TabDataState::Loading;
                        state.hooks_data = TabDataState::Loading;
                    } else {
                        // Per-plugin actions badge the selected row. Update gets
                        // its own verb so the user sees the fetch is underway,
                        // not a generic spinner.
                        let label = if matches!(
                            plugins_action,
                            kigi_hooks_plugins_types::PluginsAction::Update { .. }
                        ) {
                            "Updating..."
                        } else {
                            "Processing..."
                        };
                        state.pending_action = Some(label.into());
                        state.pending_entry_index = Some(state.picker_state.selected);
                    }
                }
                InputOutcome::Action(Action::ExecutePluginsAction(plugins_action))
            }
            ButtonAction::McpAuthTrigger => {
                if let Some(ref mut state) = self.extensions_modal {
                    state.modal_message = None;
                    // `selected_data_index()` resolves to the parent server for
                    // both server and tool rows, so `i` from a tool row
                    // intentionally auths the parent. (Mouse path is stricter
                    // to avoid accidental clicks on indented rows.)
                    if let TabDataState::Loaded(ref servers) = state.mcps_data
                        && let Some(idx) = state.selected_data_index()
                        && let Some(server) = servers.get(idx)
                    {
                        if server.is_managed_gateway {
                            return InputOutcome::Action(Action::OpenManagedConnectors);
                        }
                        // Drop repeats while an action is in flight on the same
                        // row to avoid double-spawning the OAuth browser flow.
                        let sel = state.picker_state.selected;
                        if state.pending_action.is_some() && state.pending_entry_index == Some(sel)
                        {
                            return InputOutcome::Unchanged;
                        }
                        state.pending_action = Some("authenticating...".into());
                        state.pending_entry_index = Some(sel);
                        return InputOutcome::Action(Action::McpAuthTrigger {
                            server_name: server.name.clone(),
                        });
                    }
                }
                InputOutcome::Changed
            }
            ButtonAction::ReloadSkills => {
                if let Some(ref mut state) = self.extensions_modal {
                    state.skills_data = crate::views::extensions_modal::TabDataState::Loading;
                }
                InputOutcome::Action(Action::ReloadSkills)
            }
            ButtonAction::RefreshMcpList => InputOutcome::Action(Action::RefreshMcpList),
            ButtonAction::OpenManagedConnectors => {
                InputOutcome::Action(Action::OpenManagedConnectors)
            }
            ButtonAction::ToggleSelectedMcpServer => {
                if let Some(ref mut state) = self.extensions_modal {
                    use crate::views::extensions_modal::TabDataState;
                    if let TabDataState::Loaded(ref servers) = state.mcps_data {
                        // If the cursor is on a tool row, never fall through
                        // to the server-toggle branch — drop the press on a
                        // stale tool index instead.
                        if let Some((si, ti)) = state.selected_mcp_tool() {
                            if let Some(server) = servers.get(si)
                                && let Some(tool) = server.tools.get(ti)
                            {
                                let label = if !tool.enabled {
                                    "enabling..."
                                } else {
                                    "disabling..."
                                };
                                state.pending_action = Some(label.into());
                                state.pending_entry_index = Some(state.picker_state.selected);
                                return InputOutcome::Action(Action::ToggleMcpTool {
                                    server_name: server.name.clone(),
                                    tool_name: tool.name.clone(),
                                    enabled: !tool.enabled,
                                });
                            }
                            return InputOutcome::Changed;
                        }
                        if let Some(idx) = state.selected_data_index()
                            && let Some(server) = servers.get(idx)
                        {
                            let label = if !server.enabled {
                                "enabling..."
                            } else {
                                "disabling..."
                            };
                            state.pending_action = Some(label.into());
                            state.pending_entry_index = Some(state.picker_state.selected);
                            return InputOutcome::Action(Action::ToggleMcpServer {
                                server_name: server.name.clone(),
                                enabled: !server.enabled,
                            });
                        }
                    }
                }
                InputOutcome::Changed
            }
            ButtonAction::AddMcpServer { name, config } => {
                if let Some(ref mut state) = self.extensions_modal {
                    // No pending_entry_index: the new row doesn't exist yet,
                    // so any index would decorate an unrelated existing row.
                    state.pending_action = Some("adding...".into());
                }
                InputOutcome::Action(Action::UpsertMcpServer { name, config })
            }
            ButtonAction::RemoveSelectedMcpServer => {
                let resolved = self.extensions_modal.as_ref().and_then(|state| {
                    use crate::views::extensions_modal::TabDataState;
                    use crate::views::mcps_modal::is_removable;
                    let TabDataState::Loaded(ref servers) = state.mcps_data else {
                        return None;
                    };
                    let idx = state.selected_data_index()?;
                    let server = servers.get(idx)?;
                    if is_removable(server) {
                        Some(Ok(server.name.clone()))
                    } else {
                        Some(Err(server.name.clone()))
                    }
                });
                match resolved {
                    Some(Err(name)) => {
                        if let Some(ref mut s) = self.extensions_modal {
                            s.modal_message =
                                Some(crate::views::extensions_modal::ModalMessage::Error(
                                    format!("Cannot remove managed server '{name}'"),
                                ));
                        }
                        InputOutcome::Changed
                    }
                    Some(Ok(server_name)) => {
                        if let Some(ref mut s) = self.extensions_modal {
                            s.pending_action = Some("removing...".into());
                            s.pending_entry_index = Some(s.picker_state.selected);
                        }
                        InputOutcome::Action(Action::DeleteMcpServer { server_name })
                    }
                    None => InputOutcome::Changed,
                }
            }
            ButtonAction::RemoveSelectedHook => {
                // Remove the hook source_dir of the currently selected hook.
                if let Some(ref state) = self.extensions_modal {
                    use crate::views::extensions_modal::TabDataState;
                    if let TabDataState::Loaded(ref data) = state.hooks_data
                        && let Some(idx) = state.selected_data_index()
                        && let Some(hook) = data.hooks.get(idx)
                    {
                        let path = hook.source_dir.clone();
                        return self.execute_modal_button_action(
                            crate::views::extensions_modal::ButtonAction::HooksAction(
                                kigi_hooks_plugins_types::HooksAction::Remove { path },
                            ),
                        );
                    }
                }
                InputOutcome::Changed
            }
            ButtonAction::ToggleSelectedHook => {
                if let Some(ref state) = self.extensions_modal
                    && let crate::views::extensions_modal::TabDataState::Loaded(ref data) =
                        state.hooks_data
                    && let Some(idx) = state.selected_data_index()
                    && let Some(hook) = data.hooks.get(idx)
                {
                    let source = &hook.source_dir;
                    let is_collapsed = state.hooks_collapsed_groups.contains(source);

                    if is_collapsed {
                        // Group toggle: collect all hooks in this source group.
                        let group_hooks: Vec<&kigi_hooks_plugins_types::HookInfo> = data
                            .hooks
                            .iter()
                            .filter(|h| h.source_dir == *source)
                            .collect();
                        let any_enabled = group_hooks.iter().any(|h| !h.disabled);
                        let hook_names: Vec<String> =
                            group_hooks.iter().map(|h| h.name.clone()).collect();
                        let action = kigi_hooks_plugins_types::HooksAction::ToggleSource {
                            hook_names,
                            disable: any_enabled,
                        };
                        return self.execute_modal_button_action(ButtonAction::HooksAction(action));
                    } else {
                        // Single hook toggle.
                        let action = if hook.disabled {
                            kigi_hooks_plugins_types::HooksAction::Enable {
                                hook_name: hook.name.clone(),
                            }
                        } else {
                            kigi_hooks_plugins_types::HooksAction::Disable {
                                hook_name: hook.name.clone(),
                            }
                        };
                        return self.execute_modal_button_action(ButtonAction::HooksAction(action));
                    }
                }
                InputOutcome::Changed
            }
            ButtonAction::ToggleSelectedPlugin => {
                if let Some(ref state) = self.extensions_modal
                    && let crate::views::extensions_modal::TabDataState::Loaded(ref data) =
                        state.plugins_data
                    && let Some(idx) = state.selected_data_index()
                    && let Some(plugin) = data.plugins.get(idx)
                {
                    let action = if plugin.enabled {
                        kigi_hooks_plugins_types::PluginsAction::Disable {
                            plugin_id: plugin.id.clone(),
                        }
                    } else {
                        kigi_hooks_plugins_types::PluginsAction::Enable {
                            plugin_id: plugin.id.clone(),
                        }
                    };
                    return self.execute_modal_button_action(ButtonAction::PluginsAction(action));
                }
                InputOutcome::Changed
            }
            ButtonAction::ToggleSelectedSkill => {
                if let Some(ref mut state) = self.extensions_modal {
                    use crate::views::extensions_modal::TabDataState;
                    if let TabDataState::Loaded(ref skills) = state.skills_data
                        && let Some(idx) = state.selected_data_index()
                        && let Some(skill) = skills.get(idx)
                    {
                        state.pending_action = Some("toggling...".into());
                        state.pending_entry_index = Some(state.picker_state.selected);
                        return InputOutcome::Action(Action::ToggleSkill {
                            skill_name: skill.name.clone(),
                            enabled: !skill.enabled,
                        });
                    }
                }
                InputOutcome::Changed
            }
            ButtonAction::UninstallSelectedPlugin => {
                if let Some(ref mut state) = self.extensions_modal
                    && let crate::views::extensions_modal::TabDataState::Loaded(ref data) =
                        state.plugins_data
                    && let Some(idx) = state.selected_data_index()
                    && let Some(plugin) = data.plugins.get(idx)
                {
                    let action = kigi_hooks_plugins_types::PluginsAction::Uninstall {
                        plugin_id: plugin.id.clone(),
                        confirmed: false,
                    };
                    return self.execute_modal_button_action(ButtonAction::PluginsAction(action));
                }
                InputOutcome::Changed
            }
            ButtonAction::UpdateSelectedPlugin => {
                // Fetch latest from the plugin's source for the selected plugin
                // only (`plugin_id: Some(..)`) — distinct from `r` reload, which
                // re-copies installed plugins at their current version.
                if let Some(ref state) = self.extensions_modal
                    && let crate::views::extensions_modal::TabDataState::Loaded(ref data) =
                        state.plugins_data
                    && let Some(idx) = state.selected_data_index()
                    && let Some(plugin) = data.plugins.get(idx)
                {
                    let action = kigi_hooks_plugins_types::PluginsAction::Update {
                        plugin_id: Some(plugin.id.clone()),
                    };
                    return self.execute_modal_button_action(ButtonAction::PluginsAction(action));
                }
                InputOutcome::Changed
            }
            ButtonAction::ToggleExpand => {
                // Same logic as Space key — toggle collapse on current tab.
                if let Some(ref mut state) = self.extensions_modal {
                    use crate::views::extensions_modal::{ExtensionsTab, TabDataState};
                    match state.active_tab {
                        ExtensionsTab::Hooks => {
                            if let TabDataState::Loaded(ref data) = state.hooks_data
                                && let Some(idx) = state.selected_data_index()
                                && let Some(hook) = data.hooks.get(idx)
                            {
                                let key = hook.source_dir.clone();
                                if !state.hooks_collapsed_groups.remove(&key) {
                                    state.hooks_collapsed_groups.insert(key);
                                }
                            }
                        }
                        ExtensionsTab::Plugins => {
                            let sel = state.picker_state.selected;
                            if let Some(gk) = state
                                .entry_group_keys
                                .get(sel)
                                .and_then(|k| k.as_ref())
                                .cloned()
                            {
                                if !state.plugins_collapsed_groups.remove(&gk) {
                                    state.plugins_collapsed_groups.insert(gk);
                                }
                            } else if state.picker_state.expanded.contains(&sel) {
                                state.picker_state.expanded.remove(&sel);
                            } else {
                                state.picker_state.expanded.insert(sel);
                            }
                        }
                        ExtensionsTab::McpServers => {
                            let sel = state.picker_state.selected;
                            if let Some(gk) = state
                                .entry_group_keys
                                .get(sel)
                                .and_then(|k| k.as_ref())
                                .map(|s| s.as_str())
                            {
                                if gk.starts_with("mcp-section:") {
                                    if !state.mcps_collapsed_sections.remove(gk) {
                                        state.mcps_collapsed_sections.insert(gk.to_string());
                                    }
                                } else if let Some(si) =
                                    crate::views::extensions_modal::parse_mcp_tools_server_index(gk)
                                {
                                    if state.mcps_tools_expanded.contains(&si) {
                                        state.mcps_tools_expanded.remove(&si);
                                    } else {
                                        state.mcps_tools_expanded.insert(si);
                                    }
                                }
                            }
                        }
                        ExtensionsTab::Skills => {}
                    }
                }
                InputOutcome::Changed
            }
            ButtonAction::CycleFilter => {
                if let Some(ref mut state) = self.extensions_modal {
                    use crate::views::extensions_modal::{ExtensionsTab, TabDataState};
                    match state.active_tab {
                        ExtensionsTab::Plugins => {
                            state.plugins_filter = state.plugins_filter.next();
                            state.picker_state.selected = 0;
                        }
                        ExtensionsTab::McpServers => {
                            state.mcps_filter = state.mcps_filter.next();
                            state.picker_state.selected = 0;
                        }
                        ExtensionsTab::Hooks => {
                            let new_filter = state.hooks_filter.next();
                            state.hooks_filter = new_filter;
                            if let TabDataState::Loaded(ref data) = state.hooks_data {
                                state.picker_state.selected = data
                                    .hooks
                                    .iter()
                                    .position(|h| {
                                        crate::views::extensions_modal::fuzzy_matches_hook(
                                            h,
                                            &state.picker_state.query,
                                        ) && new_filter.matches(!h.disabled)
                                    })
                                    .unwrap_or(0);
                            }
                        }
                        ExtensionsTab::Skills => {
                            state.skills_filter = state.skills_filter.next();
                            state.picker_state.selected = 0;
                        }
                    }
                }
                InputOutcome::Changed
            }
            ButtonAction::StartInput {
                command_prefix,
                fields,
            } => {
                if let Some(ref mut state) = self.extensions_modal {
                    state.modal_message = None;
                    state.input = Some(ModalInput::from_specs(command_prefix, fields));
                }
                InputOutcome::Changed
            }
        }
    }
}

#[cfg(test)]
mod extensions_modal_search_key_tests {
    use crate::app::app_view::InputOutcome;
    use crate::views::extensions_modal::{ExtensionsModalState, ExtensionsTab};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    #[test]
    fn esc_on_empty_search_exits_search_keeps_modal_open() {
        let mut agent = super::test_fixtures::make_agent();
        agent.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::Plugins));

        let outcome = agent.handle_extensions_modal_key(&key(KeyCode::Char('/')));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            agent
                .extensions_modal
                .as_ref()
                .unwrap()
                .picker_state
                .search_active,
            "`/` should activate search"
        );

        let outcome = agent.handle_extensions_modal_key(&key(KeyCode::Esc));
        assert!(matches!(outcome, InputOutcome::Changed));
        let state = agent
            .extensions_modal
            .as_ref()
            .expect("Esc on an empty search must not close the modal");
        assert!(
            !state.picker_state.search_active,
            "Esc should deactivate search"
        );
        assert!(state.picker_state.query.is_empty());
    }

    #[test]
    fn esc_after_canceling_empty_search_closes_modal() {
        let mut agent = super::test_fixtures::make_agent();
        agent.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::Plugins));

        agent.handle_extensions_modal_key(&key(KeyCode::Char('/')));
        agent.handle_extensions_modal_key(&key(KeyCode::Esc));
        assert!(agent.extensions_modal.is_some(), "first Esc cancels search");

        agent.handle_extensions_modal_key(&key(KeyCode::Esc));
        assert!(
            agent.extensions_modal.is_none(),
            "Esc with search inactive and empty query closes the modal"
        );
    }

    #[test]
    fn esc_without_search_closes_modal_immediately() {
        let mut agent = super::test_fixtures::make_agent();
        agent.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::Plugins));

        let outcome = agent.handle_extensions_modal_key(&key(KeyCode::Esc));
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(agent.extensions_modal.is_none());
    }

    #[test]
    fn esc_with_typed_query_exits_search_keeps_modal_open() {
        // Pin vim-mode off; this test asserts the non-vim picker path.
        crate::appearance::cache::set_vim_mode(false);
        let mut agent = super::test_fixtures::make_agent();
        agent.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::Plugins));

        agent.handle_extensions_modal_key(&key(KeyCode::Char('/')));
        agent.handle_extensions_modal_key(&key(KeyCode::Char('a')));
        {
            let state = agent.extensions_modal.as_ref().unwrap();
            assert!(state.picker_state.search_active);
            assert_eq!(state.picker_state.query, "a");
        }

        agent.handle_extensions_modal_key(&key(KeyCode::Esc));
        {
            let state = agent
                .extensions_modal
                .as_ref()
                .expect("modal stays open while a query is present");
            assert!(!state.picker_state.search_active);
            assert_eq!(state.picker_state.query, "a");
        }

        agent.handle_extensions_modal_key(&key(KeyCode::Esc));
        {
            let state = agent
                .extensions_modal
                .as_ref()
                .expect("clearing the retained query keeps the modal open");
            assert!(!state.picker_state.search_active);
            assert!(state.picker_state.query.is_empty());
        }

        agent.handle_extensions_modal_key(&key(KeyCode::Esc));
        assert!(
            agent.extensions_modal.is_none(),
            "Esc with no search and no query closes the modal"
        );
    }

    #[test]
    fn tab_during_active_search_switches_tab_and_keeps_query() {
        let mut agent = super::test_fixtures::make_agent();
        agent.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::Plugins));

        agent.handle_extensions_modal_key(&key(KeyCode::Char('/')));
        agent.handle_extensions_modal_key(&key(KeyCode::Char('g')));

        let outcome = agent.handle_extensions_modal_key(&key(KeyCode::Tab));
        assert!(matches!(outcome, InputOutcome::Changed));
        let state = agent
            .extensions_modal
            .as_ref()
            .expect("Tab during search keeps the modal open");
        assert_eq!(state.active_tab, ExtensionsTab::Skills);
        assert!(
            state.picker_state.search_active,
            "search stays active across a tab switch"
        );
        assert_eq!(
            state.picker_state.query, "g",
            "the query carries over to the new tab"
        );
    }

    #[test]
    fn back_tab_during_active_search_switches_to_previous_tab() {
        let mut agent = super::test_fixtures::make_agent();
        agent.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::Plugins));

        agent.handle_extensions_modal_key(&key(KeyCode::Char('/')));
        agent.handle_extensions_modal_key(&key(KeyCode::Char('g')));

        agent.handle_extensions_modal_key(&key(KeyCode::BackTab));
        let state = agent.extensions_modal.as_ref().unwrap();
        assert_eq!(state.active_tab, ExtensionsTab::Hooks);
        assert!(state.picker_state.search_active);
        assert_eq!(state.picker_state.query, "g");
    }

    #[test]
    fn shift_tab_during_active_search_switches_to_previous_tab() {
        let mut agent = super::test_fixtures::make_agent();
        agent.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::Plugins));

        agent.handle_extensions_modal_key(&key(KeyCode::Char('/')));
        agent.handle_extensions_modal_key(&key(KeyCode::Char('g')));

        agent.handle_extensions_modal_key(&shift_key(KeyCode::Tab));
        let state = agent.extensions_modal.as_ref().unwrap();
        assert_eq!(state.active_tab, ExtensionsTab::Hooks);
        assert!(state.picker_state.search_active);
        assert_eq!(state.picker_state.query, "g");
    }

    #[test]
    fn tab_during_search_wraps_around_tabs() {
        let mut agent = super::test_fixtures::make_agent();
        agent.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::McpServers));

        agent.handle_extensions_modal_key(&key(KeyCode::Char('/')));
        agent.handle_extensions_modal_key(&key(KeyCode::Tab));
        let state = agent.extensions_modal.as_ref().unwrap();
        assert_eq!(state.active_tab, ExtensionsTab::Hooks);
        assert!(state.picker_state.search_active);
    }
}

#[cfg(test)]
mod connectors_url_click_tests {
    use super::AgentView;
    use crate::app::actions::Action;
    use crate::app::app_view::InputOutcome;
    use crate::views::extensions_modal::{
        ExtensionsModalState, ExtensionsTab, TabDataState, render_extensions_modal,
    };
    use crate::views::mcps_modal::{McpServerDisplayStatus, McpServerInfo, McpWireSource};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn managed_server() -> McpServerInfo {
        McpServerInfo {
            name: "grok_com_linear".into(),
            display_name: None,
            status: McpServerDisplayStatus::Ready,
            tool_count: 0,
            auth_required: false,
            tools: vec![],
            enabled: true,
            source: "managed".into(),
            wire_source: McpWireSource::Managed,
            plugin_name: None,
            is_managed_gateway: false,
        }
    }

    // Build an agent whose extensions modal shows an expanded Managed section,
    // then paint it so `hit_areas` + `link_band` reflect the real layout.
    fn rendered_agent() -> AgentView {
        let mut agent = super::test_fixtures::make_agent();
        let mut state = ExtensionsModalState::new(ExtensionsTab::McpServers);
        state.mcps_data = TabDataState::Loaded(vec![managed_server()]);
        agent.extensions_modal = Some(state);
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        render_extensions_modal(
            &mut buf,
            area,
            agent.extensions_modal.as_mut().unwrap(),
            None,
            false,
            0,
        );
        agent
    }

    fn left_down(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    // (column inside the Managed row, its recorded URL band) from the last paint.
    fn managed_url_hit(agent: &AgentView) -> (u16, std::ops::Range<u16>) {
        let state = agent.extensions_modal.as_ref().unwrap();
        let (entry_idx, band) = state
            .picker_state
            .link_band
            .clone()
            .expect("expanded Managed section records a connectors URL band");
        let hit = state.picker_state.hit_areas.as_ref().unwrap();
        let pos = hit
            .entry_indices
            .iter()
            .position(|&e| e == entry_idx)
            .unwrap();
        (hit.item_rects[pos].x + 2, band)
    }

    #[test]
    fn mouse_down_on_url_row_opens_connectors() {
        let mut agent = rendered_agent();
        let (col, band) = managed_url_hit(&agent);
        let outcome = agent.handle_extensions_modal_mouse(&left_down(col, band.start));
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::OpenManagedConnectors)
        ));
        // The section stays expanded (opened, did not fold).
        assert!(
            !agent
                .extensions_modal
                .as_ref()
                .unwrap()
                .mcps_collapsed_sections
                .contains("mcp-section:managed")
        );
    }

    #[test]
    fn mouse_down_on_label_row_folds_not_opens() {
        let mut agent = rendered_agent();
        let (col, label_row) = {
            let state = agent.extensions_modal.as_ref().unwrap();
            let (entry_idx, _band) = state.picker_state.link_band.clone().unwrap();
            let hit = state.picker_state.hit_areas.as_ref().unwrap();
            let pos = hit
                .entry_indices
                .iter()
                .position(|&e| e == entry_idx)
                .unwrap();
            let rect = hit.item_rects[pos];
            (rect.x + 2, rect.y) // first row of the item rect is the fold-toggle label
        };
        let outcome = agent.handle_extensions_modal_mouse(&left_down(col, label_row));
        assert!(!matches!(
            outcome,
            InputOutcome::Action(Action::OpenManagedConnectors)
        ));
        // Fold happened: the Managed section is now collapsed.
        assert!(
            agent
                .extensions_modal
                .as_ref()
                .unwrap()
                .mcps_collapsed_sections
                .contains("mcp-section:managed")
        );
    }

    #[test]
    fn mouse_down_on_url_row_while_searching_does_not_open() {
        // Parity with the Ctrl+O guard: opening is suppressed while search is active.
        let mut agent = rendered_agent();
        let (col, band) = managed_url_hit(&agent);
        agent
            .extensions_modal
            .as_mut()
            .unwrap()
            .picker_state
            .search_active = true;
        let outcome = agent.handle_extensions_modal_mouse(&left_down(col, band.start));
        assert!(!matches!(
            outcome,
            InputOutcome::Action(Action::OpenManagedConnectors)
        ));
    }
}
