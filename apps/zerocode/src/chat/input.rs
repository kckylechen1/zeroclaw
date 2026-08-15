//! Keyboard and mouse handling extracted from chat.rs.

use super::*;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;
use std::sync::Arc;

impl super::Chat {
    pub(crate) async fn handle_key(
        &mut self,
        key: KeyEvent,
        term: &mut crate::config_manager::Term,
    ) -> bool {
        // Determine which phase we're in without holding a borrow on self.
        // For the picker, extract what we need; for active, delegate below.
        match &mut self.phase {
            ChatPhase::PickAgent {
                agents,
                list_state,
                loading,
            } => {
                if *loading {
                    return false;
                }
                use crate::keymap::{ChatTabAction, GlobalAction, ModalAction};
                // Three action types in scope here — explicit short-circuit
                // chain instead of one mixed match.
                match ModalAction::from_chord(&key) {
                    Some(ModalAction::Confirm) => {
                        if let Some(i) = list_state.selected()
                            && let Some(alias) = agents.get(i).cloned()
                        {
                            self.pick_or_start_session(&alias).await;
                        }
                        return false;
                    }
                    Some(ModalAction::Cancel) => return true,
                    _ => {}
                }
                if GlobalAction::from_chord(&key) == Some(GlobalAction::Quit) {
                    return true;
                }
                match ChatTabAction::from_chord(&key) {
                    Some(ChatTabAction::BrowseUp) | Some(ChatTabAction::BrowseUpVim) => {
                        let i = list_state.selected().unwrap_or(0);
                        list_state.select(Some(i.saturating_sub(1)));
                    }
                    Some(ChatTabAction::BrowseDown) | Some(ChatTabAction::BrowseDownVim) => {
                        let i = list_state.selected().unwrap_or(0);
                        if i + 1 < agents.len() {
                            list_state.select(Some(i + 1));
                        }
                    }
                    _ => {}
                }
                return false;
            }
            ChatPhase::PickSession {
                sessions,
                list_state,
                agents,
            } => {
                use crate::keymap::{ChatTabAction, ModalAction};
                if ModalAction::from_chord(&key) == Some(ModalAction::Confirm) {
                    if let Some(i) = list_state.selected()
                        && let Some(entry) = sessions.get(i).cloned()
                    {
                        self.resume_session_entry(entry).await;
                    }
                    return false;
                }
                if ModalAction::from_chord(&key) == Some(ModalAction::Cancel)
                    || ChatTabAction::from_chord(&key) == Some(ChatTabAction::NewSession)
                {
                    let agents = agents.clone();
                    self.start_fresh_from_picker(agents).await;
                    return false;
                }
                match ModalAction::from_chord(&key) {
                    Some(ModalAction::Up) => {
                        let i = list_state.selected().unwrap_or(0);
                        list_state.select(Some(i.saturating_sub(1)));
                    }
                    Some(ModalAction::Down) => {
                        let i = list_state.selected().unwrap_or(0);
                        if i + 1 < sessions.len() {
                            list_state.select(Some(i + 1));
                        }
                    }
                    _ => {}
                }
                return false;
            }
            ChatPhase::PickCwd {
                agent_alias,
                explorer,
            } => {
                let action = explorer.handle_key(key);
                match action {
                    ExplorerAction::ConfirmDir(path) => {
                        let alias = agent_alias.clone();
                        let cwd_str = path.to_str().map(str::to_string);
                        self.start_session(&alias, cwd_str.as_deref()).await;
                    }
                    ExplorerAction::Cancel => {
                        self.phase = ChatPhase::PickAgent {
                            agents: Vec::new(),
                            list_state: ListState::default(),
                            loading: true,
                        };
                        // Re-fetch agents asynchronously.
                        let _ = self.init().await;
                    }
                    ExplorerAction::Confirm(_) | ExplorerAction::None => {}
                }
                return false;
            }
            ChatPhase::Error(_) => {
                use crate::keymap::{ChatTabAction, GlobalAction};
                return GlobalAction::from_chord(&key) == Some(GlobalAction::Quit)
                    || ChatTabAction::from_chord(&key) == Some(ChatTabAction::ErrorDismiss);
            }
            ChatPhase::Active(_) => { /* handled below to avoid borrow conflict */ }
        }

        // Active phase — borrow state directly to avoid double &mut self.
        let ChatPhase::Active(ref mut state) = self.phase else {
            return false;
        };

        // ── Model / model_provider picker overlay key handling ───
        // Takes priority over all other Active-phase keys while open.
        if state.model_picker.is_open() {
            use crate::keymap::ModalAction;

            let action = ModalAction::from_chord(&key);
            let up = action == Some(ModalAction::Up);
            let down = action == Some(ModalAction::Down);

            // Movement first.
            if up || down {
                match &mut state.model_picker {
                    ModelPickerOverlay::Model(p)
                    | ModelPickerOverlay::ConfiguredProviderStage(p) => {
                        if up {
                            p.move_up();
                        } else {
                            p.move_down();
                        }
                    }
                    ModelPickerOverlay::Loading | ModelPickerOverlay::None => {}
                }
                state.mark_dirty_full();
                return false;
            }

            match action {
                Some(ModalAction::Cancel) => {
                    state.model_picker = ModelPickerOverlay::None;
                    state.mark_dirty_full();
                    return false;
                }
                Some(ModalAction::Confirm) => {
                    let rpc = self.rpc.clone();
                    Self::confirm_model_picker_selection(&rpc, state).await;
                    return false;
                }
                _ => {
                    // Any other key while the picker is open is swallowed so it
                    // doesn't leak into the input bar.
                    return false;
                }
            }
        }

        if state.pending_elicitation.is_some() {
            use crate::keymap::ModalAction;
            let action = ModalAction::from_chord(&key);

            // Multi-select toggle on Space. Single-select ignores Space.
            if action == Some(ModalAction::Toggle) {
                let mut toggled = false;
                if let Some(e) = state.pending_elicitation.as_mut()
                    && e.multi
                    && let Some(slot) = e.selected.get_mut(e.cursor)
                {
                    *slot = !*slot;
                    toggled = true;
                }
                if toggled {
                    state.mark_dirty_full();
                }
                return false;
            }

            match action {
                Some(ModalAction::Up) => {
                    if let Some(e) = state.pending_elicitation.as_mut() {
                        e.cursor = e.cursor.saturating_sub(1);
                    }
                    state.mark_dirty_full();
                    return false;
                }
                Some(ModalAction::Down) => {
                    if let Some(e) = state.pending_elicitation.as_mut()
                        && e.cursor + 1 < e.choices.len()
                    {
                        e.cursor += 1;
                    }
                    state.mark_dirty_full();
                    return false;
                }
                Some(ModalAction::Confirm) => {
                    // Build the response without holding the modal borrow,
                    // then answer the daemon. For an invalid multi-select
                    // (bounds unmet) keep the modal open.
                    let payload = state
                        .pending_elicitation
                        .as_ref()
                        .and_then(|e| e.accept_content().map(|c| (e.request_id.clone(), c)));
                    if let Some((id, content)) = payload {
                        state.pending_elicitation = None;
                        state.mark_dirty_full();
                        let rpc = self.rpc.clone();
                        tokio::spawn(async move {
                            let _ = rpc
                                .respond_to_inbound_request(
                                    id,
                                    Ok(serde_json::json!({
                                        "action": "accept",
                                        "content": content
                                    })),
                                )
                                .await;
                        });
                    }
                    // else: invalid selection — swallow, leave modal up.
                    return false;
                }
                Some(ModalAction::Cancel) => {
                    if let Some(e) = state.pending_elicitation.take() {
                        state.mark_dirty_full();
                        let id = e.request_id;
                        let rpc = self.rpc.clone();
                        tokio::spawn(async move {
                            let _ = rpc
                                .respond_to_inbound_request(
                                    id,
                                    Ok(serde_json::json!({ "action": "cancel" })),
                                )
                                .await;
                        });
                    }
                    return false;
                }
                _ => {
                    // Swallow every other key so the prompt stays modal and
                    // nothing leaks into the input bar.
                    return false;
                }
            }
        }

        // ── Session overlay key handling ─────────────────────────
        let mut handled_session_overlay = false;
        let mut confirm_session = None;
        if let SessionOverlay::List {
            sessions,
            list_state,
        } = &mut state.session_overlay
        {
            handled_session_overlay = true;
            use crate::keymap::ModalAction;
            match ModalAction::from_chord(&key) {
                Some(ModalAction::Cancel) => {
                    state.session_overlay = SessionOverlay::None;
                }
                Some(ModalAction::Confirm) => {
                    if let Some(i) = list_state.selected() {
                        confirm_session = sessions.get(i).cloned();
                    }
                }
                Some(ModalAction::Up) => {
                    let i = list_state.selected().unwrap_or(0);
                    list_state.select(Some(i.saturating_sub(1)));
                }
                Some(ModalAction::Down) => {
                    let i = list_state.selected().unwrap_or(0);
                    if i + 1 < sessions.len() {
                        list_state.select(Some(i + 1));
                    }
                }
                _ => {}
            }
        }
        if handled_session_overlay {
            if let Some(entry) = confirm_session {
                Self::switch_to_session_entry(&self.rpc, self.pane_kind, state, entry).await;
            }
            return false;
        }

        {
            use crate::keymap::ChatTabAction as QAction;
            let qaction = QAction::from_chord(&key);
            match qaction {
                Some(QAction::PauseResumeQueue) => {
                    let paused = state.toggle_queue_pause();
                    if paused {
                        // The paused state is shown as ghost text in the empty
                        // input bar, so no info-bar notice is needed here.
                        state.clear_info_notice();
                    } else {
                        state.set_info_notice(crate::i18n::t("zc-queue-resumed"));
                        self.pump_queue();
                    }
                    return false;
                }
                Some(QAction::QueueNavUp) if state.queue_sidebar_open() => {
                    state.queue_select_step(-1);
                    return false;
                }
                Some(QAction::QueueNavDown) if state.queue_sidebar_open() => {
                    state.queue_select_step(1);
                    return false;
                }
                Some(QAction::QueueDelete) if state.queue_sidebar_open() => {
                    state.delete_selected_queued();
                    return false;
                }
                Some(QAction::QueueEdit) if state.queue_sidebar_open() => {
                    let bar_busy = !state.input_bar.input().trim().is_empty()
                        || state.input_bar.has_pending_attachments();
                    if bar_busy {
                        state
                            .entries
                            .push(ChatEntry::SystemMessage(Arc::<str>::from(crate::i18n::t(
                                "zc-queue-edit-busy",
                            ))));
                        state.mark_dirty_append();
                    } else if let Some((text, attachments)) = state.take_selected_for_edit() {
                        state.input_bar.load_for_edit(text, attachments);
                    }
                    return false;
                }
                Some(QAction::QueueWiden) if state.queue_sidebar_open() => {
                    state.widen_queue_sidebar();
                    return false;
                }
                Some(QAction::QueueNarrow) if state.queue_sidebar_open() => {
                    state.narrow_queue_sidebar();
                    return false;
                }
                _ => {}
            }
        }

        // Any key press clears the mouse-click highlight — the user is done
        // with visual selection and is interacting via keyboard.
        state.clear_mouse_highlight();

        // ── Auto-exit browse mode on typing keys ─────────────────
        // If the user pressed a printable key that isn't a browse-mode
        // navigation key (j/k/↑/↓/Esc/Enter/Ctrl+C), exit browse mode
        // so they can type without an extra Esc press.
        if state.in_browse_mode() {
            let is_browse_key = {
                use crate::keymap::ChatTabAction;
                matches!(
                    ChatTabAction::from_chord(&key),
                    Some(
                        ChatTabAction::BrowseEnter
                            | ChatTabAction::BrowseUp
                            | ChatTabAction::BrowseDown
                            | ChatTabAction::BrowseUpVim
                            | ChatTabAction::BrowseDownVim
                            | ChatTabAction::BrowseSelectExtend
                            | ChatTabAction::BrowseSelectExtendDown
                            | ChatTabAction::BrowseExitSelection
                            | ChatTabAction::CopySelection
                    )
                )
            };
            if !is_browse_key {
                state.exit_browse_mode();
            }
        }

        if state.pending_approval().is_none() && !state.turn_in_flight {
            use crate::keymap::ChatTabAction;
            if let Some(ChatTabAction::BrowseEnter) = ChatTabAction::from_chord(&key) {
                if state.in_browse_mode() {
                    state.browse_move_up(1, false);
                } else {
                    state.enter_browse_mode();
                }
                return false;
            }
        }

        // Enter (slash commands + submit), text input, cursor, backspace.
        // It does NOT handle approval, selection, session management, etc.
        if state.pending_approval().is_none() && !state.in_browse_mode() {
            let action = state.input_bar.handle_key(key);
            match action {
                InputBarAction::Submit { text, attachments } => {
                    state.clear_info_notice();
                    state.resume_queue();
                    let prompt = text.unwrap_or_default();
                    let enq = state.enqueue_message(prompt, attachments);
                    self.after_enqueue(enq);
                    return false;
                }
                InputBarAction::Inject { text, attachments } => {
                    state.clear_info_notice();
                    let prompt = text.unwrap_or_default();
                    let enq = state.inject_message(prompt, attachments);
                    if enq.is_ok()
                        && state.turn_in_flight
                        && !matches!(state.turn_status, TurnStatus::Cancelling)
                    {
                        let sid = state.session_id.clone();
                        let res = self.rpc.session_cancel(&sid).await;
                        if let ChatPhase::Active(ref mut state) = self.phase {
                            if res.is_ok() {
                                state.enter_cancelling();
                            } else {
                                state.commit_turn(String::new(), false);
                            }
                        }
                    }
                    self.after_enqueue(enq);
                    return false;
                }
                InputBarAction::StatusMessage(msg) => {
                    state.set_info_notice(msg);
                    return false;
                }
                InputBarAction::ToggleThinking => {
                    state.show_thoughts = !state.show_thoughts;
                    state.mark_dirty_full();
                    let status = if state.show_thoughts {
                        crate::i18n::t("zc-chat-thinking-visible")
                    } else {
                        crate::i18n::t("zc-chat-thinking-hidden")
                    };
                    state
                        .entries
                        .push(ChatEntry::SystemMessage(Arc::<str>::from(status)));
                    state.mark_dirty_append();
                    return false;
                }
                InputBarAction::EnterBrowseMode => {
                    state.enter_browse_mode();
                    return false;
                }
                InputBarAction::OpenHelp => {
                    self.help_requested = true;
                    return false;
                }
                InputBarAction::ClearQueue(idx) => {
                    let notice = state.clear_queue_cmd(idx);
                    state.set_info_notice(notice);
                    return false;
                }
                InputBarAction::RestartSession => {
                    let rpc = self.rpc.clone();
                    let pane_kind = self.pane_kind;
                    if let Some(next_phase) =
                        Self::restart_session_for_state(&rpc, pane_kind, state).await
                    {
                        self.phase = next_phase;
                    }
                    return false;
                }
                InputBarAction::ResumeQueue => {
                    state.clear_info_notice();
                    if state.resume_queue() {
                        self.pump_queue();
                    }
                    return false;
                }
                InputBarAction::SetModel(model) => {
                    let rpc = self.rpc.clone();
                    Self::apply_session_override(
                        &rpc,
                        state,
                        crate::client::SessionOverrides {
                            model: Some(model),
                            ..Default::default()
                        },
                    )
                    .await;
                    return false;
                }
                InputBarAction::SetModelProvider(model_provider) => {
                    let rpc = self.rpc.clone();
                    Self::apply_session_override(
                        &rpc,
                        state,
                        crate::client::SessionOverrides {
                            model_provider: Some(model_provider),
                            ..Default::default()
                        },
                    )
                    .await;
                    return false;
                }
                InputBarAction::OpenModelPicker => {
                    let rpc = self.rpc.clone();
                    let tx = self.model_fetch_tx.clone();
                    Self::open_model_picker(&rpc, &tx, state).await;
                    return false;
                }
                InputBarAction::OpenModelProviderPicker => {
                    let rpc = self.rpc.clone();
                    Self::open_provider_picker(&rpc, state).await;
                    return false;
                }
                InputBarAction::Consumed => return false,
                InputBarAction::NotHandled => { /* fall through to chat-specific keys */ }
            }
        }

        // ── Chat-specific key handling ───────────────────────────
        use crate::keymap::{ChatTabAction, GlobalAction};
        // Quit chord wins (chat overrides conditionally on turn state below).
        if GlobalAction::from_chord(&key) == Some(GlobalAction::Quit) {
            if state.turn_in_flight {
                if !matches!(state.turn_status, TurnStatus::Cancelling) {
                    let res = self.rpc.session_cancel(&state.session_id).await;
                    if res.is_ok() {
                        state.enter_cancelling();
                    } else {
                        state.commit_turn(String::new(), false);
                    }
                }
            } else {
                return true;
            }
            return false;
        }
        match ChatTabAction::from_chord(&key) {
            Some(ChatTabAction::BrowseExitSelection) => {
                if state.in_browse_mode() {
                    state.exit_browse_mode();
                } else if state.turn_in_flight
                    && !matches!(state.turn_status, TurnStatus::Cancelling)
                {
                    let res = self.rpc.session_cancel(&state.session_id).await;
                    if res.is_ok() {
                        state.enter_cancelling();
                    } else {
                        state.commit_turn(String::new(), false);
                    }
                }
            }
            Some(ChatTabAction::ApprovalApprove) if state.pending_approval().is_some() => {
                if let Some(pa) = state.take_pending_approval() {
                    let _ = self
                        .rpc
                        .session_approve(
                            &state.session_id,
                            &pa.request_id,
                            ApprovalDecision::AllowOnce,
                        )
                        .await;
                }
            }
            Some(ChatTabAction::CancelTurn) if state.pending_approval().is_some() => {
                if let Some(pa) = state.take_pending_approval() {
                    let _ = self
                        .rpc
                        .session_approve(
                            &state.session_id,
                            &pa.request_id,
                            ApprovalDecision::Reject,
                        )
                        .await;
                }
            }
            Some(ChatTabAction::ApprovalApproveAll) if state.pending_approval().is_some() => {
                if let Some(pa) = state.take_pending_approval() {
                    let _ = self
                        .rpc
                        .session_approve(
                            &state.session_id,
                            &pa.request_id,
                            ApprovalDecision::AllowAlways,
                        )
                        .await;
                }
            }
            Some(ChatTabAction::ApprovalApproveEdit) if state.pending_approval().is_some() => {
                let is_edit_tool = state
                    .pending_approval()
                    .map(|pa| matches!(pa.tool_name.as_str(), "file_edit" | "file_write"))
                    .unwrap_or(false);
                if is_edit_tool && let Some(pa) = state.take_pending_approval() {
                    let initial = pa.arguments_summary.clone();
                    let edited = open_editor_for_content(&initial).await;
                    let _ = term.clear();
                    let _ = self
                        .rpc
                        .session_approve(
                            &state.session_id,
                            &pa.request_id,
                            ApprovalDecision::RejectWithEdit {
                                replacement: edited,
                            },
                        )
                        .await;
                }
            }
            Some(ChatTabAction::NewSession) if !state.turn_in_flight => {
                let rpc = self.rpc.clone();
                let pane_kind = self.pane_kind;
                if let Some(next_phase) =
                    Self::restart_session_for_state(&rpc, pane_kind, state).await
                {
                    self.phase = next_phase;
                }
            }
            Some(ChatTabAction::SwitchSession) if !state.turn_in_flight => {
                // ACP and Chat live in separate stores and must not cross-pick:
                //  • Chat → unified session_backend (filter out channel-backed
                //    sessions; those are owned by the channels pane).
                //  • ACP  → dedicated acp-sessions.db, listed by a separate RPC.
                let picker_sessions = if self.pane_kind == PaneKind::Acp {
                    self.rpc
                        .acp_session_list()
                        .await
                        .map(|list| list.sessions)
                        .unwrap_or_default()
                } else {
                    match self.rpc.session_list(None).await {
                        Ok(list) => list
                            .sessions
                            .into_iter()
                            .filter(|s| s.channel_id.is_none())
                            .collect(),
                        Err(_) => Vec::new(),
                    }
                };

                let mut ls = ListState::default();
                if !picker_sessions.is_empty() {
                    ls.select(Some(0));
                }
                state.session_overlay = SessionOverlay::List {
                    sessions: picker_sessions,
                    list_state: ls,
                };
            }
            Some(ChatTabAction::ToggleThoughts)
                if state.input_bar.input().is_empty()
                    && state.pending_approval().is_none()
                    && !state.in_browse_mode() =>
            {
                state.show_thoughts = !state.show_thoughts;
                state.mark_dirty_full();
            }
            Some(ChatTabAction::TodoToggle) => {
                state.todo_tracker.toggle();
                state.mark_dirty_full();
            }
            Some(ChatTabAction::BrowseEnter) => {
                if state.in_browse_mode() {
                    state.browse_move_up(1, false);
                } else {
                    state.enter_browse_mode();
                }
            }
            Some(ChatTabAction::BrowseExit) if state.in_browse_mode() => {
                state.exit_browse_mode();
            }
            Some(ChatTabAction::BrowseUp) => {
                if state.in_browse_mode() {
                    state.browse_move_up(1, false);
                } else if !state.pinned_to_bottom {
                    state.scroll_up(1);
                }
            }
            Some(ChatTabAction::BrowseDown) => {
                if state.in_browse_mode() {
                    state.browse_move_down(1, false);
                } else if !state.pinned_to_bottom {
                    state.scroll_down(1);
                }
            }
            Some(ChatTabAction::BrowseSelectExtend) => {
                if state.in_browse_mode() {
                    state.browse_move_up(1, true);
                } else {
                    state.scroll_up(1);
                }
            }
            Some(ChatTabAction::BrowseSelectExtendDown) => {
                if state.in_browse_mode() {
                    state.browse_move_down(1, true);
                } else {
                    state.scroll_down(1);
                }
            }
            Some(ChatTabAction::FastScrollUp) => {
                state.scroll_up(5);
            }
            Some(ChatTabAction::FastScrollDown) => {
                state.scroll_down(5);
            }
            Some(ChatTabAction::ScrollUp) => {
                state.scroll_up(1);
            }
            Some(ChatTabAction::ScrollDown) => {
                state.scroll_down(1);
            }
            Some(ChatTabAction::PageUp) => {
                state.page_up();
            }
            Some(ChatTabAction::PageDown) => {
                state.page_down();
            }
            Some(ChatTabAction::JumpStart) => {
                state.scroll_to_top();
            }
            Some(ChatTabAction::JumpEnd) => {
                state.scroll_to_bottom();
            }
            Some(ChatTabAction::BrowseUpVim)
                if state.in_browse_mode()
                    && state.pending_approval().is_none()
                    && !state.turn_in_flight =>
            {
                state.browse_move_up(1, false);
            }
            Some(ChatTabAction::BrowseDownVim)
                if state.in_browse_mode()
                    && state.pending_approval().is_none()
                    && !state.turn_in_flight =>
            {
                state.browse_move_down(1, false);
            }
            Some(ChatTabAction::CopySelection) if state.has_selection() => {
                let text = state.yank_selection();
                if !text.is_empty() {
                    crate::mouse::copy_osc52(&text);
                }
            }
            Some(ChatTabAction::CopyAllVisible) if state.has_selection() => {
                let text = state.yank_selection();
                if !text.is_empty() {
                    crate::mouse::copy_osc52(&text);
                }
            }
            _ => {}
        }
        false
    }

    async fn handle_model_picker_mouse(
        rpc: &Arc<RpcClient>,
        mouse: MouseEvent,
        area: Rect,
        state: &mut ChatState,
    ) {
        let Some(modal_rect) = model_picker_overlay_area(&state.model_picker, area) else {
            return;
        };

        let col = mouse.column;
        let row = mouse.row;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if !mouse::in_rect(col, row, modal_rect) {
                    state.model_picker = ModelPickerOverlay::None;
                    state.mark_dirty_full();
                    return;
                }

                let item_count = state.model_picker.item_count();
                if let Some(idx) = mouse::list_click_index(row, modal_rect, 0, item_count) {
                    if let Some(picker) = state.model_picker.picker_mut() {
                        picker.cursor = idx;
                    }
                    Self::confirm_model_picker_selection(rpc, state).await;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if mouse::in_rect(col, row, modal_rect) =>
            {
                if let Some(picker) = state.model_picker.picker_mut() {
                    if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                        picker.move_up();
                    } else {
                        picker.move_down();
                    }
                    state.mark_dirty_full();
                }
            }
            _ => {}
        }
    }

    async fn switch_to_session_entry(
        rpc: &Arc<RpcClient>,
        pane_kind: PaneKind,
        state: &mut ChatState,
        entry: crate::client::SessionEntry,
    ) {
        let new_sid = entry.session_id;
        let new_name = entry.name;
        let agent_alias = entry
            .agent_alias
            .unwrap_or_else(|| state.agent_alias.clone());
        if new_sid == state.session_id {
            state.session_overlay = SessionOverlay::None;
            state.mark_dirty_full();
            return;
        }

        let rehydrate_result = if pane_kind == PaneKind::Acp {
            rpc.session_new_acp(&agent_alias, None, Some(&new_sid))
                .await
        } else {
            rpc.session_new_with_id(&agent_alias, None, Some(&new_sid))
                .await
        };
        let rehydrated = match rehydrate_result {
            Ok(session) => session,
            Err(e) => {
                state.session_overlay = SessionOverlay::None;
                state.info_message = Some(crate::widgets::InfoMessage::error(crate::i18n::t_args(
                    "zc-chat-session-switch-error",
                    &[("error", &e.to_string())],
                )));
                state.mark_dirty_full();
                return;
            }
        };

        let _ = rpc.session_close(&state.session_id).await;
        state.session_overlay = SessionOverlay::None;
        state.reset_for_session(new_sid.clone(), new_name);
        state.agent_alias = agent_alias.clone();
        state.cwd = rehydrated.workspace_dir;

        Self::refresh_model_identity(rpc, state).await;
        if let Ok(msgs) = rpc.session_messages(&new_sid).await {
            state.load_history(msgs.messages);
        }
    }

    /// Apply a session override (model and/or model_provider) to the active
    /// session via `session/configure`, reporting the outcome on the info bar.
    /// On a model_provider switch the daemon rebuilds the provider box live.
    pub(crate) async fn apply_session_override(
        rpc: &RpcClient,
        state: &mut ChatState,
        overrides: crate::client::SessionOverrides,
    ) {
        let waiting = crate::widgets::InfoMessage::info(crate::i18n::t("zc-model-switch-applying"));
        state.info_message = Some(waiting);
        state.mark_dirty_full();

        match rpc.session_configure(&state.session_id, overrides).await {
            Ok(result) => {
                let model = result.overrides.model.unwrap_or_default();
                let model_provider = result.overrides.model_provider.unwrap_or_default();
                let summary = if !model_provider.is_empty() {
                    crate::i18n::t_args(
                        "zc-model-switch-provider-ok",
                        &[("provider", &model_provider), ("model", &model)],
                    )
                } else {
                    crate::i18n::t_args("zc-model-switch-model-ok", &[("model", &model)])
                };
                state.info_message = Some(crate::widgets::InfoMessage::note(summary));
                let provider_ref = (!model_provider.is_empty()).then_some(model_provider.as_str());
                let resolved_model = if !model.is_empty() {
                    Some(model.clone())
                } else if let Some(r) = provider_ref {
                    Self::configured_model(rpc, r).await
                } else {
                    None
                };
                state.set_model_identity(provider_ref, resolved_model.as_deref());
                // A model_provider switch changes the catalog — drop the cache
                // so the next `/model` use refetches.
                if provider_ref.is_some() {
                    state.input_bar.set_model_catalog(String::new(), Vec::new());
                }
            }
            Err(e) => {
                state.info_message = Some(crate::widgets::InfoMessage::error(crate::i18n::t_args(
                    "zc-model-switch-failed",
                    &[("error", &e.to_string())],
                )));
            }
        }
        state.mark_dirty_full();
    }

    pub(crate) async fn refresh_model_identity(rpc: &RpcClient, state: &mut ChatState) {
        if let Some(provider_ref) = Self::resolve_model_provider_ref(rpc, &state.agent_alias).await
        {
            let model = Self::configured_model(rpc, &provider_ref).await;
            state.set_model_identity(Some(&provider_ref), model.as_deref());
        }
    }

    /// Resolve the agent's configured model_provider reference (`<type>.<alias>`)
    /// from config.
    async fn resolve_model_provider_ref(rpc: &RpcClient, agent_alias: &str) -> Option<String> {
        let prop = format!("agents.{agent_alias}.model_provider");
        let entries = rpc.config_list(Some(&prop)).await.ok()?;
        entries.into_iter().find(|e| e.path == prop).and_then(|e| {
            e.value
                .as_ref()
                .and_then(|v| v.as_str().map(str::to_string))
        })
    }

    /// Read the model configured for a dotted model_provider ref
    /// (`providers.models.<family>.<alias>.model`), used to pre-select the
    /// current model in the picker.
    async fn configured_model(rpc: &RpcClient, model_provider_ref: &str) -> Option<String> {
        let prop = format!("providers.models.{model_provider_ref}.model");
        let entries = rpc.config_list(Some(&prop)).await.ok()?;
        entries.into_iter().find(|e| e.path == prop).and_then(|e| {
            e.value
                .as_ref()
                .and_then(|v| v.as_str().map(str::to_string))
        })
    }

    /// Fetch the model catalog for a model_provider family. Returns an empty vec
    /// on failure; the caller surfaces the error on the info bar.
    async fn fetch_models(rpc: &RpcClient, family: &str) -> Vec<String> {
        match rpc.catalog_models(family).await {
            Ok(res) => res.models,
            Err(_) => Vec::new(),
        }
    }

    /// Open the single-stage model picker for the active agent's model_provider,
    /// pre-selecting the currently-configured model.
    async fn open_model_picker(
        rpc: &Arc<RpcClient>,
        model_fetch_tx: &mpsc::Sender<ModelFetchResult>,
        state: &mut ChatState,
    ) {
        let active_provider = match state.model_provider_ref.clone() {
            Some(r) => Some(r),
            None => Self::resolve_model_provider_ref(rpc, &state.agent_alias).await,
        };
        let Some(model_provider_ref) = active_provider else {
            state.info_message = Some(crate::widgets::InfoMessage::error(crate::i18n::t(
                "zc-model-catalog-no-provider",
            )));
            state.mark_dirty_full();
            return;
        };
        let family = model_provider_ref
            .split('.')
            .next()
            .unwrap_or(&model_provider_ref)
            .to_string();

        // Warm cache: open immediately, no fetch, no loading state.
        if state.input_bar.model_catalog_provider() == Some(family.as_str())
            && !state.input_bar.model_catalog().is_empty()
        {
            let models = state.input_bar.model_catalog().to_vec();
            let current = match state.model.clone() {
                Some(m) => Some(m),
                None => Self::configured_model(rpc, &model_provider_ref).await,
            };
            state.model_picker = ModelPickerOverlay::Model(crate::widgets::PickerState::new(
                models,
                current.as_deref(),
            ));
            state.info_message = None;
            state.mark_dirty_full();
            return;
        }

        // Cold cache: show the Loading modal now and fetch off the draw loop so
        // the waiting state actually paints. The result returns over
        // model_fetch_tx and is drained in refresh_if_inactive.
        state.model_picker = ModelPickerOverlay::Loading;
        state.info_message = Some(crate::widgets::InfoMessage::info(crate::i18n::t(
            "zc-model-catalog-loading",
        )));
        state.mark_dirty_full();

        let rpc = rpc.clone();
        let tx = model_fetch_tx.clone();
        let session_id = state.session_id.clone();
        let model_provider_ref_c = model_provider_ref.clone();
        let session_model = state.model.clone();
        tokio::spawn(async move {
            let models = Self::fetch_models(&rpc, &family).await;
            let current = match session_model {
                Some(m) => Some(m),
                None => Self::configured_model(&rpc, &model_provider_ref_c).await,
            };
            let _ = tx
                .send(ModelFetchResult {
                    session_id,
                    family,
                    model_provider_ref: model_provider_ref_c,
                    models,
                    current,
                })
                .await;
        });
    }

    /// Apply a completed background catalog fetch: swap the Loading picker to
    /// the populated list (or surface an empty-catalog error), and warm the
    /// autocomplete cache. Ignores results for a session that has since
    /// changed or a picker the user already dismissed.
    pub(super) fn apply_model_fetch(&mut self, res: ModelFetchResult) {
        let ChatPhase::Active(state) = &mut self.phase else {
            return;
        };
        if state.session_id != res.session_id {
            return;
        }
        if !matches!(state.model_picker, ModelPickerOverlay::Loading) {
            return;
        }
        if res.models.is_empty() {
            state.model_picker = ModelPickerOverlay::None;
            state.info_message = Some(crate::widgets::InfoMessage::error(crate::i18n::t(
                "zc-model-catalog-empty",
            )));
            state.mark_dirty_full();
            return;
        }
        state
            .input_bar
            .set_model_catalog(res.family, res.models.clone());
        state.model_picker = ModelPickerOverlay::Model(crate::widgets::PickerState::new(
            res.models,
            res.current.as_deref(),
        ));
        let _ = res.model_provider_ref;
        state.info_message = None;
        state.mark_dirty_full();
    }

    /// Open stage 1 of the two-stage model_provider picker.
    async fn open_provider_picker(rpc: &RpcClient, state: &mut ChatState) {
        match rpc.quickstart_state().await {
            Ok(snap) => {
                let providers = snap.model_providers;
                if providers.is_empty() {
                    state.info_message = Some(crate::widgets::InfoMessage::error(crate::i18n::t(
                        "zc-model-catalog-no-provider",
                    )));
                    state.mark_dirty_full();
                    return;
                }
                let current = match state.model_provider_ref.clone() {
                    Some(r) => Some(r),
                    None => Self::resolve_model_provider_ref(rpc, &state.agent_alias).await,
                };
                state.input_bar.set_provider_catalog(providers.clone());
                state.model_picker = ModelPickerOverlay::ConfiguredProviderStage(
                    crate::widgets::PickerState::new(providers, current.as_deref()),
                );
                state.mark_dirty_full();
            }
            Err(e) => {
                state.info_message = Some(crate::widgets::InfoMessage::error(crate::i18n::t_args(
                    "zc-model-provider-catalog-failed",
                    &[("error", &e.to_string())],
                )));
                state.mark_dirty_full();
            }
        }
    }

    async fn open_agent_picker(&mut self, current_alias: String) {
        let agents = match self.rpc.agents_status().await {
            Ok(result) => result
                .agents
                .into_iter()
                .filter(|agent| agent.enabled)
                .map(|agent| agent.alias)
                .collect::<Vec<_>>(),
            Err(e) => {
                if let ChatPhase::Active(state) = &mut self.phase {
                    state.info_message =
                        Some(crate::widgets::InfoMessage::error(crate::i18n::t_args(
                            "zc-chat-error-fetch-agents",
                            &[("error", &e.to_string())],
                        )));
                    state.mark_dirty_full();
                }
                return;
            }
        };

        if agents.len() <= 1 {
            return;
        }

        let selected = agents
            .iter()
            .position(|agent| agent == &current_alias)
            .unwrap_or(0);
        let mut list_state = ListState::default();
        list_state.select(Some(selected));

        self.resume_session_id = None;
        self.resume_agent_alias = None;
        self.phase = ChatPhase::PickAgent {
            agents,
            list_state,
            loading: false,
        };
    }

    pub(crate) async fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) {
        // Dir-picker explorer handles its own mouse events.
        if let ChatPhase::PickCwd { explorer, .. } = &mut self.phase {
            explorer.handle_mouse(mouse);
            return;
        }

        if matches!(self.phase, ChatPhase::PickSession { .. }) {
            let mut confirm_session: Option<SessionEntry> = None;
            if let ChatPhase::PickSession {
                sessions,
                list_state,
                ..
            } = &mut self.phase
            {
                let overlay_area = session_list_overlay_area(area);
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                        if mouse::in_rect(mouse.column, mouse.row, overlay_area) =>
                    {
                        if let Some(idx) = mouse::list_click_index(
                            mouse.row,
                            overlay_area,
                            list_state.offset(),
                            sessions.len(),
                        ) {
                            list_state.select(Some(idx));
                            if self
                                .session_list_double_click
                                .click(mouse.column, mouse.row)
                            {
                                confirm_session = sessions.get(idx).cloned();
                            }
                        }
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        if mouse::in_rect(mouse.column, mouse.row, overlay_area) =>
                    {
                        let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                        let i = list_state.selected().unwrap_or(0);
                        list_state.select(Some(mouse::list_scroll(i, sessions.len(), up, 1)));
                    }
                    _ => {}
                }
            }
            if let Some(entry) = confirm_session {
                self.resume_session_entry(entry).await;
            }
            return;
        }

        // Agent picker: click highlights a row, double-click confirms (enters
        // the session), wheel moves the selection.
        if matches!(self.phase, ChatPhase::PickAgent { loading: false, .. }) {
            let mut confirm_alias: Option<String> = None;
            if let ChatPhase::PickAgent {
                agents, list_state, ..
            } = &mut self.phase
            {
                let list_area = self.pick_agent_list_area;
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(idx) = mouse::list_click_index(
                            mouse.row,
                            list_area,
                            list_state.offset(),
                            agents.len(),
                        ) {
                            list_state.select(Some(idx));
                            if self.pick_agent_double_click.click(mouse.column, mouse.row) {
                                confirm_alias = agents.get(idx).cloned();
                            }
                        }
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                        let i = list_state.selected().unwrap_or(0);
                        list_state.select(Some(mouse::list_scroll(i, agents.len(), up, 1)));
                    }
                    _ => {}
                }
            }
            if let Some(alias) = confirm_alias {
                self.pick_or_start_session(&alias).await;
            }
            return;
        }

        if let ChatPhase::Active(state) = &self.phase
            && let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && !state.turn_in_flight
            && !state.input_bar.has_file_explorer()
            && matches!(state.session_overlay, SessionOverlay::None)
            && !state.model_picker.is_open()
            && state.title_hit_target_at(mouse.column, mouse.row) == Some(TitleHitTarget::Agent)
        {
            let current_alias = state.agent_alias.clone();
            self.open_agent_picker(current_alias).await;
            return;
        }

        if let ChatPhase::Active(ref mut state) = self.phase {
            // Let the file explorer handle mouse events first when open.
            if state.input_bar.handle_mouse(mouse) {
                state.clear_mouse_highlight();
                return;
            }

            if state.model_picker.is_open() {
                let rpc = self.rpc.clone();
                Self::handle_model_picker_mouse(&rpc, mouse, area, state).await;
                return;
            }

            // Session list overlay intercepts all mouse events when open.
            if let SessionOverlay::List {
                sessions,
                list_state,
            } = &mut state.session_overlay
            {
                let mut confirm_session: Option<crate::client::SessionEntry> = None;
                let col = mouse.column;
                let row = mouse.row;
                let overlay_area = session_list_overlay_area(area);

                match mouse.kind {
                    MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        if !mouse::in_rect(col, row, overlay_area) {
                            // Click outside → close overlay.
                            state.session_overlay = SessionOverlay::None;
                        } else {
                            let count = sessions.len();
                            if let Some(idx) = mouse::list_click_index(
                                row,
                                overlay_area,
                                list_state.offset(),
                                count,
                            ) {
                                list_state.select(Some(idx));
                                if self.session_list_double_click.click(col, row) {
                                    confirm_session = sessions.get(idx).cloned();
                                }
                            }
                        }
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        if mouse::in_rect(col, row, overlay_area) =>
                    {
                        let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                        let count = sessions.len();
                        let i = list_state.selected().unwrap_or(0);
                        list_state.select(Some(mouse::list_scroll(i, count, up, 1)));
                    }
                    _ => {}
                }
                if let Some(entry) = confirm_session {
                    Self::switch_to_session_entry(&self.rpc, self.pane_kind, state, entry).await;
                }
                return;
            }

            use crossterm::event::KeyModifiers as KM;
            let col = mouse.column;
            let row = mouse.row;

            if !state.model_picker.is_open()
                && let MouseEventKind::Down(MouseButton::Left) = mouse.kind
                && let Some(target) = state.title_hit_target_at(col, row)
            {
                match target {
                    TitleHitTarget::Agent => {}
                    TitleHitTarget::ModelProvider => {
                        let rpc = self.rpc.clone();
                        Self::open_provider_picker(&rpc, state).await;
                    }
                    TitleHitTarget::Model => {
                        let rpc = self.rpc.clone();
                        let tx = self.model_fetch_tx.clone();
                        Self::open_model_picker(&rpc, &tx, state).await;
                    }
                }
                return;
            }

            // Queue sidebar intercepts mouse events over its area before the
            // conversation handler, so clicks select queued items and the wheel
            // scrolls the queue rather than the transcript.
            if state.queue_sidebar_open() && state.point_in_queue_sidebar(col, row) {
                match mouse.kind {
                    MouseEventKind::ScrollUp => state.queue_scroll_by(-3),
                    MouseEventKind::ScrollDown => state.queue_scroll_by(3),
                    MouseEventKind::Down(MouseButton::Left) => {
                        state.queue_click_at(col, row);
                    }
                    _ => {}
                }
                return;
            }

            // The scrollbar is shared by browse mode and character-level
            // transcript selection, so handle its drag lifecycle before those
            // interaction modes diverge.
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(track) = state.scrollbar_track_rect
                        && mouse::in_rect(col, row, track)
                    {
                        state.clear_transcript_selection();
                        state.scrollbar_drag = Some(ScrollbarDrag {
                            start_scroll: state.scroll_offset,
                            start_row: row,
                        });
                        let max = state
                            .last_total_rows
                            .saturating_sub(state.last_inner_height);
                        if track.height > 0 {
                            let rel = row.saturating_sub(track.y) as u32;
                            let new_off = (rel * max as u32 / track.height.max(1) as u32) as u16;
                            state.scroll_offset = new_off.min(max);
                            state.pinned_to_bottom = state.scroll_offset >= max;
                        }
                        return;
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(drag) = state.scrollbar_drag {
                        state.clear_transcript_selection();
                        let max = state
                            .last_total_rows
                            .saturating_sub(state.last_inner_height);
                        let track_h = state
                            .scrollbar_track_rect
                            .map(|r| r.height)
                            .unwrap_or(0)
                            .max(1);
                        let dy = row as i32 - drag.start_row as i32;
                        let scroll_delta = dy * max as i32 / track_h as i32;
                        let new_off =
                            (drag.start_scroll as i32 + scroll_delta).clamp(0, max as i32);
                        state.scroll_offset = new_off as u16;
                        state.pinned_to_bottom = state.scroll_offset >= max;
                        return;
                    }
                }
                MouseEventKind::Up(MouseButton::Left) if state.scrollbar_drag.is_some() => {
                    state.scrollbar_drag = None;
                    return;
                }
                _ => {}
            }

            if !state.in_browse_mode() {
                match mouse.kind {
                    MouseEventKind::ScrollUp => state.scroll_up(3),
                    MouseEventKind::ScrollDown => state.scroll_down(3),
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(region) = state
                            .copy_hit_regions
                            .iter()
                            .find(|region| {
                                matches!(region.kind, CopyHitKind::Code | CopyHitKind::Transcript)
                                    && mouse::in_rect(col, row, region.rect)
                            })
                            .cloned()
                        {
                            if !region.text.is_empty() {
                                crate::mouse::copy_osc52(&region.text);
                                match region.kind {
                                    CopyHitKind::Code => {
                                        state.clear_mouse_highlight();
                                        state.set_copy_feedback(CopyFeedbackTarget::Code(
                                            region.group,
                                        ));
                                    }
                                    CopyHitKind::Transcript => {
                                        state.set_overlay_copy_feedback(region.rect);
                                    }
                                    CopyHitKind::Message => {}
                                }
                                state.set_info_notice(crate::i18n::t("zc-chat-copied-clipboard"));
                            }
                        } else {
                            state.clear_mouse_highlight();
                            state.begin_transcript_drag(col, row);
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        state.update_transcript_drag(col, row);
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        state.finish_transcript_drag();
                    }
                    _ => {}
                }
                return;
            }

            match mouse.kind {
                MouseEventKind::ScrollUp => state.scroll_up(3),
                MouseEventKind::ScrollDown => state.scroll_down(3),
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(region) = state
                        .copy_hit_regions
                        .iter()
                        .find(|r| mouse::in_rect(col, row, r.rect))
                        .cloned()
                    {
                        if !region.text.is_empty() {
                            crate::mouse::copy_osc52(&region.text);
                            match region.kind {
                                CopyHitKind::Code => {
                                    state.clear_mouse_highlight();
                                    state.set_copy_feedback(CopyFeedbackTarget::Code(region.group));
                                }
                                CopyHitKind::Message => {
                                    state.clear_browse_selection();
                                    state.set_overlay_copy_feedback(region.rect);
                                }
                                CopyHitKind::Transcript => {
                                    state.set_overlay_copy_feedback(region.rect);
                                }
                            }
                            state.set_info_notice(crate::i18n::t("zc-chat-copied-clipboard"));
                        }
                        return;
                    }
                    let hit = state
                        .entry_rects
                        .iter()
                        .find(|(_, r)| mouse::in_rect(col, row, *r))
                        .map(|(idx, _)| *idx);
                    let shift = mouse.modifiers.contains(KM::SHIFT);
                    let ctrl = mouse.modifiers.contains(KM::CONTROL);
                    if let Some(idx) = hit {
                        if ctrl {
                            if !state.browse_multi.remove(&idx) {
                                state.browse_multi.insert(idx);
                            }
                            state.mark_dirty_full();
                        } else if shift {
                            if state.browse_cursor.is_none() {
                                state.browse_cursor = Some(idx);
                            }
                            state.browse_anchor = state.browse_cursor;
                            state.browse_cursor = Some(idx);
                            state.mark_dirty_full();
                        } else {
                            // Plain click
                            state.browse_multi.clear();
                            state.browse_anchor = None;
                            // In browse mode: move cursor and prepare for
                            // optional drag-range selection. Copying still
                            // requires the explicit keyboard or button action.
                            state.browse_cursor = Some(idx);
                            state.mouse_down_entry = Some(idx);
                            state.mark_dirty_full();
                        }
                    } else {
                        state.clear_browse_selection();
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(start) = state.mouse_down_entry {
                        // Drag extends selection only in browse mode.
                        if state.in_browse_mode() {
                            let hit = state
                                .entry_rects
                                .iter()
                                .find(|(_, r)| mouse::in_rect(col, row, *r))
                                .map(|(idx, _)| *idx);
                            if let Some(end) = hit {
                                state.browse_anchor = Some(start);
                                state.browse_cursor = Some(end);
                                state.mark_dirty_full();
                            }
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    // Mouse-up ends a browse-mode drag gesture only. It must
                    // not copy implicitly: users expect dragging transcript
                    // text to be safe while selecting words/lines in the
                    // terminal, and whole-message copy now lives behind the
                    // explicit `[Copy]` affordance.
                    state.mouse_down_entry = None;
                }
                _ => {}
            }
        }
    }
}
