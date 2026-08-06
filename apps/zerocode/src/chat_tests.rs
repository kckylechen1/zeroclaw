use super::*;

    fn state() -> ChatState {
        ChatState::new(
            "sess-1".to_string(),
            "myagent".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        )
    }

    fn transcript_snapshot(area: Rect, rows: &[&str]) -> TranscriptSnapshot {
        use unicode_width::UnicodeWidthChar;

        let mut cells = Vec::with_capacity(usize::from(area.width) * usize::from(area.height));
        for row in 0..area.height {
            let mut column = 0;
            for ch in rows
                .get(usize::from(row))
                .copied()
                .unwrap_or_default()
                .chars()
            {
                if column >= area.width {
                    break;
                }
                let width = (ch.width().unwrap_or(0) as u16)
                    .max(1)
                    .min(area.width - column);
                cells.push(TranscriptCell {
                    symbol: ch.to_string(),
                    span_start: column,
                });
                for _ in 1..width {
                    cells.push(TranscriptCell {
                        symbol: String::new(),
                        span_start: column,
                    });
                }
                column += width;
            }
            while column < area.width {
                cells.push(TranscriptCell {
                    symbol: " ".to_string(),
                    span_start: column,
                });
                column += 1;
            }
        }
        TranscriptSnapshot { area, cells }
    }

    #[test]
    fn transcript_selection_extracts_visible_wrapped_text() {
        let snapshot = transcript_snapshot(Rect::new(10, 5, 8, 2), &["alpha be", "ta gamma"]);
        let forward = TranscriptSelection {
            anchor: CellPoint { column: 6, row: 0 },
            head: CellPoint { column: 1, row: 1 },
            dragged: true,
        };
        let reverse = TranscriptSelection {
            anchor: CellPoint { column: 1, row: 1 },
            head: CellPoint { column: 6, row: 0 },
            dragged: true,
        };
        let click = TranscriptSelection {
            anchor: CellPoint { column: 6, row: 0 },
            head: CellPoint { column: 6, row: 0 },
            dragged: false,
        };

        assert_eq!(snapshot.selected_text(forward).as_deref(), Some("be\nta"));
        assert_eq!(snapshot.selected_text(reverse).as_deref(), Some("be\nta"));
        assert_eq!(snapshot.selected_text(click), None);
    }

    #[test]
    fn transcript_selection_expands_wide_character_cells() {
        let snapshot = transcript_snapshot(Rect::new(0, 0, 4, 1), &["A界B"]);
        let selection = TranscriptSelection {
            anchor: CellPoint { column: 2, row: 0 },
            head: CellPoint { column: 3, row: 0 },
            dragged: true,
        };

        assert!(snapshot.has_text_at(CellPoint { column: 2, row: 0 }));
        assert!(snapshot.selection_contains(selection, CellPoint { column: 1, row: 0 }));
        assert_eq!(snapshot.selected_text(selection).as_deref(), Some("界B"));
    }

    #[test]
    fn transcript_selection_drag_is_limited_to_conversation_body() {
        let mut state = state();
        state.transcript_snapshot = Some(transcript_snapshot(
            Rect::new(10, 5, 8, 2),
            &["alpha be", "ta gamma"],
        ));

        assert!(!state.begin_transcript_drag(2, 1));
        assert_eq!(state.transcript_selection, None);

        assert!(state.begin_transcript_drag(16, 5));
        assert!(state.update_transcript_drag(11, 6));
        state.finish_transcript_drag();
        assert_eq!(state.transcript_selected_text().as_deref(), Some("be\nta"));
        assert_eq!(state.copy_feedback, None);
        assert!(state.info_message.is_none());
    }

    #[test]
    fn transcript_selection_clears_on_scroll_and_session_reset() {
        let mut state = state();
        state.transcript_snapshot = Some(transcript_snapshot(Rect::new(0, 0, 5, 1), &["hello"]));
        assert!(state.begin_transcript_drag(0, 0));
        assert!(state.update_transcript_drag(1, 0));
        state.finish_transcript_drag();
        state.set_overlay_copy_feedback(Rect::new(0, 0, 5, 1));

        state.scroll_up(1);
        assert_eq!(state.transcript_selection, None);
        assert_eq!(state.copy_feedback, None);
        assert!(state.copy_hit_regions.is_empty());

        assert!(state.begin_transcript_drag(0, 0));
        assert!(state.update_transcript_drag(1, 0));
        state.finish_transcript_drag();
        state.scroll_to_top();
        assert_eq!(state.transcript_selection, None);

        assert!(state.begin_transcript_drag(0, 0));
        assert!(state.update_transcript_drag(1, 0));
        state.finish_transcript_drag();
        state.last_total_rows = 10;
        state.last_inner_height = 1;
        state.scroll_to_bottom();
        assert_eq!(state.transcript_selection, None);

        assert!(state.begin_transcript_drag(0, 0));
        assert!(state.update_transcript_drag(1, 0));
        state.finish_transcript_drag();
        state.enter_browse_mode();
        assert_eq!(state.transcript_selection, None);

        state.exit_browse_mode();
        assert!(state.begin_transcript_drag(0, 0));
        assert!(state.update_transcript_drag(1, 0));
        state.finish_transcript_drag();
        state.reset_for_session("sess-2".to_string(), None);
        assert_eq!(state.transcript_selection, None);
        assert!(state.transcript_snapshot.is_none());
    }

    #[test]
    fn transcript_selection_clears_when_snapshot_changes() {
        let replacements = [
            (
                "geometry",
                transcript_snapshot(Rect::new(0, 0, 6, 1), &["hello "]),
            ),
            (
                "content",
                transcript_snapshot(Rect::new(0, 0, 5, 1), &["hullo"]),
            ),
        ];

        for (case, replacement) in replacements {
            let mut state = state();
            state.transcript_snapshot =
                Some(transcript_snapshot(Rect::new(0, 0, 5, 1), &["hello"]));
            assert!(state.begin_transcript_drag(0, 0));
            assert!(state.update_transcript_drag(1, 0));
            state.copy_hit_regions.push(CopyHitRegion {
                rect: Rect::new(0, 0, 2, 1),
                text: "he".to_string(),
                kind: CopyHitKind::Transcript,
                group: 0,
            });
            state.copy_feedback = Some(CopyFeedback {
                target: CopyFeedbackTarget::Overlay(Rect::new(0, 0, 2, 1)),
                shown_at: Instant::now(),
            });

            state.set_transcript_snapshot(replacement);

            assert_eq!(state.transcript_selection, None, "{case} selection");
            assert!(state.copy_hit_regions.is_empty(), "{case} copy regions");
            assert_eq!(state.copy_feedback, None, "{case} copy feedback");
        }
    }

    #[tokio::test]
    async fn transcript_selection_rendered_drag_excludes_chrome() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let (mut chat, _rx) = test_chat();
        let mut state = state();
        state
            .entries
            .push(ChatEntry::AgentMessage(Arc::<str>::from("hello world")));
        state.mark_dirty_full();

        let area = Rect::new(0, 0, 80, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut state, area, PaneKind::Chat))
            .expect("draw chat");

        let snapshot = state
            .transcript_snapshot
            .as_ref()
            .expect("render captures transcript cells");
        assert!(
            snapshot.area.y > area.y,
            "panel chrome stays outside snapshot"
        );
        let (text_row, text_col) = snapshot
            .cells
            .chunks(usize::from(snapshot.area.width))
            .enumerate()
            .find_map(|(row, cells)| {
                cells
                    .iter()
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
                    .find("hello")
                    .map(|column| (row as u16, column as u16))
            })
            .expect("rendered transcript contains message text");
        let start_col = snapshot.area.x + text_col;
        let start_row = snapshot.area.y + text_row;
        chat.phase = ChatPhase::Active(Box::new(state));

        for event in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let column = if matches!(event, MouseEventKind::Down(_)) {
                start_col
            } else {
                start_col + 4
            };
            chat.handle_mouse(
                MouseEvent {
                    kind: event,
                    column,
                    row: start_row,
                    modifiers: KeyModifiers::NONE,
                },
                area,
            )
            .await;
        }

        let ChatPhase::Active(state) = &mut chat.phase else {
            panic!("expected active chat");
        };
        assert_eq!(state.transcript_selected_text().as_deref(), Some("hello"));
        assert!(!state.begin_transcript_drag(area.x, area.y));
        assert_eq!(state.transcript_selection, None);
    }

    #[tokio::test]
    async fn transcript_selection_copy_action_is_explicit() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let (mut chat, _rx) = test_chat();
        let mut state = state();
        state.transcript_snapshot = Some(transcript_snapshot(
            Rect::new(1, 1, 20, 1),
            &["hello               "],
        ));
        assert!(state.begin_transcript_drag(1, 1));
        assert!(state.update_transcript_drag(2, 1));
        state.finish_transcript_drag();

        let area = Rect::new(0, 0, 80, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render_transcript_copy_overlay(frame, &mut state))
            .expect("draw copy action");
        let region = state
            .copy_hit_regions
            .iter()
            .find(|region| region.kind == CopyHitKind::Transcript)
            .cloned()
            .expect("selection exposes transcript copy action");
        assert_eq!(region.text, "he");
        assert_eq!(state.copy_feedback, None);

        chat.phase = ChatPhase::Active(Box::new(state));
        chat.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: region.rect.x,
                row: region.rect.y,
                modifiers: KeyModifiers::NONE,
            },
            area,
        )
        .await;

        let ChatPhase::Active(state) = &chat.phase else {
            panic!("expected active chat");
        };
        assert!(matches!(
            state.copy_feedback,
            Some(CopyFeedback {
                target: CopyFeedbackTarget::Overlay(_),
                ..
            })
        ));
        assert!(state.info_message.is_some());
    }

    #[tokio::test]
    async fn scrollbar_drag_works_outside_browse_mode() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let (mut chat, _rx) = test_chat();
        let mut state = state();
        state.last_total_rows = 100;
        state.last_inner_height = 20;
        state.scrollbar_track_rect = Some(Rect::new(79, 2, 1, 10));
        chat.phase = ChatPhase::Active(Box::new(state));

        for (kind, row) in [
            (MouseEventKind::Down(MouseButton::Left), 4),
            (MouseEventKind::Drag(MouseButton::Left), 8),
        ] {
            chat.handle_mouse(
                MouseEvent {
                    kind,
                    column: 79,
                    row,
                    modifiers: KeyModifiers::NONE,
                },
                Rect::new(0, 0, 80, 20),
            )
            .await;
        }

        {
            let ChatPhase::Active(state) = &chat.phase else {
                panic!("expected active chat");
            };
            assert!(state.scrollbar_drag.is_some());
            assert!(state.scroll_offset > 0);
            assert_eq!(state.transcript_selection, None);
        }

        chat.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 79,
                row: 8,
                modifiers: KeyModifiers::NONE,
            },
            Rect::new(0, 0, 80, 20),
        )
        .await;

        let ChatPhase::Active(state) = &chat.phase else {
            panic!("expected active chat");
        };
        assert!(state.scrollbar_drag.is_none());
    }

    #[test]
    fn hidden_tracker_leaves_full_area_for_body() {
        let t = crate::todo_tracker::TodoTracker::new(
            crate::todo_tracker::TodoLocation::Right,
            true,
            false,
        ); // hidden, no plan
        let full = Rect::new(0, 0, 100, 40);
        let (body, tracker) = carve_todo_area(&t, full);
        assert_eq!(body, full);
        assert!(tracker.is_none());
    }

    #[test]
    fn visible_right_tracker_carves_column() {
        let mut t = crate::todo_tracker::TodoTracker::new(
            crate::todo_tracker::TodoLocation::Right,
            true,
            true,
        );
        t.set_plan(vec![crate::wire::PlanEntry {
            content: "A".into(),
            status: crate::wire::PlanStatus::Pending,
            priority: crate::wire::PlanPriority::Medium,
            active_form: None,
        }]);
        let full = Rect::new(0, 0, 100, 40);
        let (body, tracker) = carve_todo_area(&t, full);
        let tracker = tracker.expect("visible tracker gets an area");
        assert_eq!(body.width + tracker.width, full.width);
        assert_eq!(tracker.width, 32);
        assert_eq!(body.height, full.height);
    }

    #[test]
    fn tracker_width_is_clamped_on_narrow_terminals() {
        let t = crate::todo_tracker::TodoTracker::new(
            crate::todo_tracker::TodoLocation::Right,
            true,
            true,
        );
        let full = Rect::new(0, 0, 40, 20); // narrow
        let (_body, tracker) = carve_todo_area(&t, full);
        let tracker = tracker.expect("side panel visible");
        assert!(tracker.width <= full.width / 2, "clamped to <= 50% width");
    }

    async fn next_rpc_request(rx: &mut mpsc::Receiver<String>, reason: &str) -> serde_json::Value {
        let line = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{reason}"))
            .expect("RPC request channel should stay open");
        serde_json::from_str(&line).expect("RPC request should be JSON")
    }

    fn respond_ok(rpc: &RpcOutbound, request: &serde_json::Value, result: serde_json::Value) {
        let id = request["id"]
            .as_str()
            .expect("RPC request should have an id");
        rpc.dispatch_response(id, Some(result), None);
    }

    fn respond_err(rpc: &RpcOutbound, request: &serde_json::Value, code: i32, message: &str) {
        let id = request["id"]
            .as_str()
            .expect("RPC request should have an id");
        rpc.dispatch_response(
            id,
            None,
            Some(crate::jsonrpc::JsonRpcError {
                code,
                message: message.to_string(),
                data: None,
            }),
        );
    }

    #[test]
    fn visible_line_slice_renders_only_the_viewport_not_the_whole_history() {
        let mut s = state();
        for i in 0..400 {
            s.entries
                .push(ChatEntry::AgentMessage(Arc::<str>::from(format!(
                    "line entry number {i}"
                ))));
        }
        s.mark_dirty_full();
        let width = 80u16;
        s.rebuild_lines(width);

        let total = s.cached_lines.len();
        assert!(total > 100, "expected a deep history, got {total} lines");

        let height = 20u16;
        let max_scroll = s.cached_total_rows.saturating_sub(height);
        let mid_scroll = max_scroll / 2;

        let (slice, local_scroll) = s.visible_line_slice(mid_scroll, height);

        assert!(
            slice.len() < total,
            "viewport slice ({}) must be smaller than full history ({total})",
            slice.len()
        );
        assert!(
            slice.len() <= (height as usize) + 8,
            "viewport slice ({}) should be bounded near the viewport height ({height}), not the history",
            slice.len()
        );
        assert!(
            local_scroll < height,
            "local scroll ({local_scroll}) must land inside the first visible entry, below viewport height ({height})"
        );
    }

    #[test]
    fn visible_line_slice_handles_top_and_bottom_extents() {
        let mut s = state();
        for i in 0..50 {
            s.entries
                .push(ChatEntry::AgentMessage(Arc::<str>::from(format!(
                    "entry {i}"
                ))));
        }
        s.mark_dirty_full();
        s.rebuild_lines(80);
        let height = 12u16;

        let (top, top_local) = s.visible_line_slice(0, height);
        assert_eq!(top_local, 0, "scroll 0 keeps the first entry aligned");
        assert!(!top.is_empty());

        let max_scroll = s.cached_total_rows.saturating_sub(height);
        let (bottom, _) = s.visible_line_slice(max_scroll, height);
        assert!(!bottom.is_empty(), "bottom extent must still yield lines");
    }

    #[test]
    fn title_shows_agent_uid_provider_model() {
        let mut s = ChatState::new(
            "9caf2a14-0e6d-4127-b016-357c0b757b87".to_string(),
            "personal_code".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        s.set_model_identity(Some("anthropic.personal_code"), Some("claude-opus-4-8"));
        assert_eq!(
            s.title(),
            "personal_code  9caf2a1  anthropic.personal_code  claude-opus-4-8"
        );
    }

    #[test]
    fn title_falls_back_before_identity_resolved() {
        let s = ChatState::new(
            "abcdef1234".to_string(),
            "myagent".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        assert_eq!(s.title(), "myagent  abcdef1");
    }

    #[test]
    fn set_model_identity_keeps_full_ref_and_updates_live() {
        let mut s = ChatState::new(
            "abcdef1234".to_string(),
            "ag".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        s.set_model_identity(Some("openai.work"), Some("gpt-5"));
        assert_eq!(s.title(), "ag  abcdef1  openai.work  gpt-5");
        s.set_model_identity(None, Some("gpt-5-mini"));
        assert_eq!(s.title(), "ag  abcdef1  openai.work  gpt-5-mini");
        s.set_model_identity(Some("anthropic.personal_code"), Some("claude-opus-4-8"));
        assert_eq!(
            s.title(),
            "ag  abcdef1  anthropic.personal_code  claude-opus-4-8"
        );
    }

    #[test]
    fn title_hit_rects_target_provider_and_model_segments() {
        let mut s = ChatState::new(
            "abcdef1234".to_string(),
            "ag".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        s.set_model_identity(Some("openai.work"), Some("gpt-5"));
        let area = Rect::new(10, 4, 80, 20);

        s.refresh_title_hit_rects(area);

        assert_eq!(
            s.title_hit_target_at(25, 4),
            Some(TitleHitTarget::ModelProvider)
        );
        assert_eq!(s.title_hit_target_at(38, 4), Some(TitleHitTarget::Model));
        assert_eq!(s.title_hit_target_at(12, 4), Some(TitleHitTarget::Agent));
        assert_eq!(s.title_hit_target_at(25, 5), None);
    }

    #[test]
    fn title_hit_rects_target_agent_before_model_identity_resolves() {
        let mut s = ChatState::new(
            "abcdef1234".to_string(),
            "ag".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );

        s.refresh_title_hit_rects(Rect::new(10, 4, 80, 20));

        assert_eq!(s.title_hit_rects.len(), 1);
        assert_eq!(s.title_hit_target_at(12, 4), Some(TitleHitTarget::Agent));
        assert_eq!(s.title_hit_target_at(16, 4), None);
    }

    #[test]
    fn title_hit_rects_clip_at_pane_edge() {
        let mut s = ChatState::new(
            "abcdef1234".to_string(),
            "ag".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        s.set_model_identity(Some("openai.work"), Some("gpt-5"));

        s.refresh_title_hit_rects(Rect::new(10, 4, 25, 20));

        assert_eq!(
            s.title_hit_target_at(33, 4),
            Some(TitleHitTarget::ModelProvider)
        );
        assert_eq!(s.title_hit_target_at(35, 4), None);
    }

    #[test]
    fn model_provider_picker_overlay_rows_are_hit_testable() {
        let mut s = state();
        s.model_picker =
            ModelPickerOverlay::ConfiguredProviderStage(crate::widgets::PickerState::new(
                vec!["openai.default".into(), "deepseek.default".into()],
                None,
            ));

        let area = Rect::new(0, 0, 80, 20);
        let modal = model_picker_overlay_area(&s.model_picker, area).unwrap();

        assert_eq!(
            mouse::list_click_index(modal.y + 1, modal, 0, s.model_picker.item_count()),
            Some(0)
        );
        assert_eq!(
            mouse::list_click_index(modal.y + 2, modal, 0, s.model_picker.item_count()),
            Some(1)
        );
        assert_eq!(
            mouse::list_click_index(modal.y, modal, 0, s.model_picker.item_count()),
            None
        );
    }

    #[test]
    fn model_picker_overlay_default_is_closed() {
        let s = state();
        assert!(!s.model_picker.is_open());
    }

    #[test]
    fn model_picker_overlay_open_states_report_open() {
        let model =
            ModelPickerOverlay::Model(crate::widgets::PickerState::new(vec!["a".into()], None));
        assert!(model.is_open());
        let stage1 = ModelPickerOverlay::ConfiguredProviderStage(crate::widgets::PickerState::new(
            vec!["anthropic.personal_code".into()],
            None,
        ));
        assert!(stage1.is_open());
    }

    #[tokio::test]
    async fn open_picker_makes_chat_claim_text_input() {
        // While the picker is open the pane is modal (claims text-input so
        // global keys are suppressed and routed to the picker handler).
        let (tx, _rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Chat);
        chat.phase = ChatPhase::Active(Box::new(state()));
        if let ChatPhase::Active(s) = &mut chat.phase {
            s.model_picker = ModelPickerOverlay::Model(crate::widgets::PickerState::new(
                vec!["a".into(), "b".into()],
                None,
            ));
        }
        assert!(chat.wants_text_input());
    }

    #[tokio::test]
    async fn pending_elicitation_makes_chat_claim_text_input() {
        let (tx, _rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Chat);
        chat.phase = ChatPhase::Active(Box::new(state()));
        // Not modal before the prompt arrives (empty input → command mode).
        assert!(!chat.wants_text_input());
        if let ChatPhase::Active(s) = &mut chat.phase {
            s.set_pending_elicitation(single_elicitation());
        }
        assert!(
            chat.wants_text_input(),
            "an active pending elicitation must claim modal focus"
        );
    }

    #[tokio::test]
    async fn wants_quit_chord_tracks_in_flight_turn_state() {
        let (tx, _rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Chat);
        chat.phase = ChatPhase::Active(Box::new(state()));

        assert!(
            !chat.wants_quit_chord(),
            "idle pane must leave Ctrl+C to the quit modal"
        );

        if let ChatPhase::Active(s) = &mut chat.phase {
            s.turn_in_flight = true;
        }
        assert!(
            chat.wants_quit_chord(),
            "an in-flight turn must consume Ctrl+C to cancel before quit"
        );

        if let ChatPhase::Active(s) = &mut chat.phase {
            s.enter_cancelling();
        }
        assert!(
            !chat.wants_quit_chord(),
            "an already-cancelling turn must not re-consume Ctrl+C"
        );
    }

    #[tokio::test]
    async fn current_session_id_reports_active_session() {
        let (tx, _rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Acp);
        // No session yet → None.
        assert_eq!(chat.current_session_id(), None);
        chat.phase = ChatPhase::Active(Box::new(state()));
        // Active → the live session id (the `state()` helper's id).
        assert!(chat.current_session_id().is_some());
    }

    #[tokio::test]
    async fn resume_session_id_dropped_when_init_lands_in_multi_agent_picker() {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Acp);
        chat.set_resume_session_id(Some("sess-prev".to_string()));

        let init = tokio::spawn(async move {
            let _ = chat.init().await;
            chat
        });

        let line = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("init should request the agent list")
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        let id = request["id"].as_str().unwrap().to_string();
        // Two enabled agents → multi-agent picker, no auto-start.
        rpc.dispatch_response(
            &id,
            Some(serde_json::json!({
                "agents": [
                    {"alias": "alpha", "enabled": true, "live_sessions": 0, "persisted_sessions": 0},
                    {"alias": "beta", "enabled": true, "live_sessions": 0, "persisted_sessions": 0}
                ]
            })),
            None,
        );

        let chat = tokio::time::timeout(Duration::from_secs(2), init)
            .await
            .expect("init should finish")
            .unwrap();
        // A carried resume id with no matching agent must not survive into the
        // picker, or a manual pick of a different agent would reattach a
        // mismatched session.
        assert_eq!(chat.resume_session_id, None);
        assert!(matches!(chat.phase, ChatPhase::PickAgent { .. }));
    }

    #[tokio::test]
    async fn multi_agent_reconnect_reattaches_prior_agent_session() {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Chat);
        chat.set_resume_session_id(Some("sess-prev".to_string()));
        chat.set_resume_agent_alias(Some("beta".to_string()));

        let init = tokio::spawn(async move {
            let _ = chat.init().await;
            chat
        });

        // First request: the agent list.
        let line = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("init should request the agent list")
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        let id = request["id"].as_str().unwrap().to_string();
        rpc.dispatch_response(
            &id,
            Some(serde_json::json!({
                "agents": [
                    {"alias": "alpha", "enabled": true, "live_sessions": 0, "persisted_sessions": 0},
                    {"alias": "beta", "enabled": true, "live_sessions": 1, "persisted_sessions": 0}
                ]
            })),
            None,
        );

        // Second request: the one-shot [todotracker] config fetch fired on the
        // first session start. Respond with an empty field set (defaults apply).
        let line = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("start_session should fetch todotracker config")
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "config/list");
        let id = request["id"].as_str().unwrap().to_string();
        rpc.dispatch_response(&id, Some(serde_json::json!([])), None);

        // Third request must be session_new_with_id carrying the prior id for
        // the prior agent — NOT a fresh pick / fresh session. This is the whole
        // fix: a multi-agent reconnect reattaches instead of minting fresh.
        let line = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("reconnect should reattach the prior session")
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "session/new");
        let params = &request["params"];
        assert_eq!(params["agent_alias"], "beta");
        assert_eq!(params["session_id"], "sess-prev");

        init.abort();
    }

    #[tokio::test]
    async fn acp_init_opens_recent_session_picker() {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Acp);

        let init = tokio::spawn(async move {
            let _ = chat.init().await;
            chat
        });

        let request = next_rpc_request(&mut rx, "init should request agents/status").await;
        assert_eq!(request["method"], method::AGENTS_STATUS);
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "agents": [
                    {"alias": "alpha", "enabled": true, "live_sessions": 0, "persisted_sessions": 0},
                    {"alias": "beta", "enabled": true, "live_sessions": 0, "persisted_sessions": 1}
                ]
            }),
        );

        let request = next_rpc_request(&mut rx, "ACP init should request recent sessions").await;
        assert_eq!(request["method"], method::SESSION_LIST_ACP);
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "sessions": [
                    {
                        "session_id": "sess-ghost",
                        "session_key": "sess-ghost",
                        "created_at": "2026-07-07T00:00:00Z",
                        "last_activity": "2026-07-07T00:10:00Z",
                        "message_count": 1,
                        "agent_alias": "ghost",
                        "channel_id": null,
                        "name": "Ghost"
                    },
                    {
                        "session_id": "sess-beta",
                        "session_key": "sess-beta",
                        "created_at": "2026-07-07T00:00:00Z",
                        "last_activity": "2026-07-07T00:05:00Z",
                        "message_count": 2,
                        "agent_alias": "beta",
                        "channel_id": null,
                        "name": "Beta work"
                    }
                ]
            }),
        );

        let chat = tokio::time::timeout(Duration::from_secs(2), init)
            .await
            .expect("init should finish")
            .unwrap();
        let ChatPhase::PickSession {
            sessions,
            list_state,
            agents,
        } = chat.phase
        else {
            panic!("ACP init should show the saved-session picker");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "sess-beta");
        assert_eq!(sessions[0].agent_alias.as_deref(), Some("beta"));
        assert_eq!(list_state.selected(), Some(0));
        assert_eq!(agents, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn acp_init_session_picker_enter_resumes_selected_session() {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Acp);

        let init = tokio::spawn(async move {
            let _ = chat.init().await;
            chat
        });

        let request = next_rpc_request(&mut rx, "init should request agents/status").await;
        assert_eq!(request["method"], method::AGENTS_STATUS);
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "agents": [
                    {"alias": "beta", "enabled": true, "live_sessions": 0, "persisted_sessions": 1}
                ]
            }),
        );

        let request = next_rpc_request(&mut rx, "ACP init should request recent sessions").await;
        assert_eq!(request["method"], method::SESSION_LIST_ACP);
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "sessions": [
                    {
                        "session_id": "sess-beta",
                        "session_key": "sess-beta",
                        "created_at": "2026-07-07T00:00:00Z",
                        "last_activity": "2026-07-07T00:05:00Z",
                        "message_count": 2,
                        "agent_alias": "beta",
                        "channel_id": null,
                        "name": "Beta work"
                    }
                ]
            }),
        );

        let mut chat = tokio::time::timeout(Duration::from_secs(2), init)
            .await
            .expect("init should finish")
            .unwrap();
        assert!(matches!(chat.phase, ChatPhase::PickSession { .. }));

        let resume = tokio::spawn(async move {
            let entry = match &chat.phase {
                ChatPhase::PickSession { sessions, .. } => sessions[0].clone(),
                _ => panic!("expected saved-session picker"),
            };
            chat.resume_session_entry(entry).await;
            chat
        });

        let request = next_rpc_request(&mut rx, "resume should load todotracker config").await;
        assert_eq!(request["method"], method::CONFIG_LIST);
        assert_eq!(request["params"]["prefix"], "todotracker");
        respond_ok(&rpc, &request, serde_json::json!([]));

        let request = next_rpc_request(&mut rx, "Enter should resume selected session").await;
        assert_eq!(request["method"], method::SESSION_NEW);
        let params = &request["params"];
        assert_eq!(params["agent_alias"], "beta");
        assert_eq!(params["session_id"], "sess-beta");
        assert_eq!(params["chat_mode"], "acp");
        assert_eq!(params["exclude_memory"], true);
        assert!(params["cwd"].is_null());
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "session_id": "sess-beta",
                "workspace_dir": "/tmp/beta"
            }),
        );

        let request = next_rpc_request(&mut rx, "resume should refresh model identity").await;
        assert_eq!(request["method"], method::CONFIG_LIST);
        assert_eq!(request["params"]["prefix"], "agents.beta.model_provider");
        respond_ok(&rpc, &request, serde_json::json!([]));

        let request = next_rpc_request(&mut rx, "resume should load history").await;
        assert_eq!(request["method"], method::SESSION_MESSAGES);
        assert_eq!(request["params"]["session_id"], "sess-beta");
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "messages": [
                    {"role": "user", "content": "resume me"}
                ],
                "total": 1,
                "start": 0
            }),
        );

        let chat = tokio::time::timeout(Duration::from_secs(2), resume)
            .await
            .expect("resume should finish")
            .unwrap();
        let ChatPhase::Active(state) = chat.phase else {
            panic!("Enter should enter the saved ACP session");
        };
        assert_eq!(state.session_id, "sess-beta");
        assert_eq!(state.agent_alias, "beta");
        assert_eq!(state.cwd.as_deref(), Some("/tmp/beta"));
    }

    #[tokio::test]
    async fn acp_init_session_picker_cancel_starts_fresh_session() {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Acp);

        let init = tokio::spawn(async move {
            let _ = chat.init().await;
            chat
        });

        let request = next_rpc_request(&mut rx, "init should request agents/status").await;
        assert_eq!(request["method"], method::AGENTS_STATUS);
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "agents": [
                    {"alias": "beta", "enabled": true, "live_sessions": 1, "persisted_sessions": 1}
                ]
            }),
        );

        let request = next_rpc_request(&mut rx, "ACP init should request recent sessions").await;
        assert_eq!(request["method"], method::SESSION_LIST_ACP);
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "sessions": [
                    {
                        "session_id": "sess-beta",
                        "session_key": "sess-beta",
                        "created_at": "2026-07-07T00:00:00Z",
                        "last_activity": "2026-07-07T00:05:00Z",
                        "message_count": 2,
                        "agent_alias": "beta",
                        "channel_id": null,
                        "name": "Beta work"
                    }
                ]
            }),
        );

        let mut chat = tokio::time::timeout(Duration::from_secs(2), init)
            .await
            .expect("init should finish")
            .unwrap();
        assert!(matches!(chat.phase, ChatPhase::PickSession { .. }));

        let fresh = tokio::spawn(async move {
            let agents = match &chat.phase {
                ChatPhase::PickSession { agents, .. } => agents.clone(),
                _ => panic!("expected saved-session picker"),
            };
            chat.start_fresh_from_picker(agents).await;
            chat
        });

        let request = next_rpc_request(&mut rx, "fresh start should load todotracker config").await;
        assert_eq!(request["method"], method::CONFIG_LIST);
        assert_eq!(request["params"]["prefix"], "todotracker");
        respond_ok(&rpc, &request, serde_json::json!([]));

        let request = next_rpc_request(&mut rx, "Esc should start a fresh session").await;
        assert_eq!(request["method"], method::SESSION_NEW);
        let params = &request["params"];
        assert_eq!(params["agent_alias"], "beta");
        assert_eq!(params["chat_mode"], "acp");
        assert_eq!(params["exclude_memory"], true);
        assert!(params["session_id"].is_null());
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "session_id": "sess-fresh",
                "workspace_dir": "/tmp/fresh"
            }),
        );

        let request =
            next_rpc_request(&mut rx, "fresh session should refresh model identity").await;
        assert_eq!(request["method"], method::CONFIG_LIST);
        respond_ok(&rpc, &request, serde_json::json!([]));

        let chat = tokio::time::timeout(Duration::from_secs(2), fresh)
            .await
            .expect("fresh start should finish")
            .unwrap();
        let ChatPhase::Active(state) = chat.phase else {
            panic!("Esc should enter a fresh ACP session");
        };
        assert_eq!(state.session_id, "sess-fresh");
    }

    #[tokio::test]
    async fn acp_init_clears_stale_carried_resume_for_disabled_agent() {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Acp);
        chat.resume_session_id = Some("sess-prev".to_string());
        chat.resume_agent_alias = Some("beta".to_string());

        let init = tokio::spawn(async move {
            let _ = chat.init().await;
            chat
        });

        let request = next_rpc_request(&mut rx, "init should request agents/status").await;
        assert_eq!(request["method"], method::AGENTS_STATUS);
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "agents": [
                    {"alias": "alpha", "enabled": true, "live_sessions": 0, "persisted_sessions": 0}
                ]
            }),
        );

        let request = next_rpc_request(
            &mut rx,
            "stale carried resume should fall back to session picker",
        )
        .await;
        assert_eq!(request["method"], method::SESSION_LIST_ACP);
        respond_ok(&rpc, &request, serde_json::json!({ "sessions": [] }));

        let request =
            next_rpc_request(&mut rx, "fresh fallback should load todotracker config").await;
        assert_eq!(request["method"], method::CONFIG_LIST);
        assert_eq!(request["params"]["prefix"], "todotracker");
        respond_ok(&rpc, &request, serde_json::json!([]));

        let request =
            next_rpc_request(&mut rx, "stale carried resume should not be sent for alpha").await;
        assert_eq!(request["method"], method::SESSION_NEW);
        assert_eq!(request["params"]["agent_alias"], "alpha");
        assert!(request["params"]["session_id"].is_null());
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "session_id": "sess-fresh",
                "workspace_dir": "/tmp/fresh"
            }),
        );

        let request =
            next_rpc_request(&mut rx, "fresh fallback should refresh model identity").await;
        assert_eq!(request["method"], method::CONFIG_LIST);
        assert_eq!(request["params"]["prefix"], "agents.alpha.model_provider");
        respond_ok(&rpc, &request, serde_json::json!([]));

        let chat = tokio::time::timeout(Duration::from_secs(2), init)
            .await
            .expect("init should finish")
            .unwrap();
        let ChatPhase::Active(state) = chat.phase else {
            panic!("stale carried resume should still enter a fresh ACP session");
        };
        assert_eq!(state.session_id, "sess-fresh");
        assert_eq!(state.agent_alias, "alpha");
    }

    #[tokio::test]
    async fn agent_picker_click_selects_row() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let (tx, _rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Chat);
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        chat.phase = ChatPhase::PickAgent {
            agents: vec!["alpha".into(), "beta".into(), "gamma".into()],
            list_state,
            loading: false,
        };
        // Stored rect is the draw's shifted form: list_click_index treats (y+1)
        // as the first item. With y=1, first item maps to row 2.
        chat.pick_agent_list_area = Rect::new(1, 1, 20, 6);
        // Click the third item → row 2 + 2 = 4.
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        chat.handle_mouse(click, Rect::new(0, 0, 40, 10)).await;
        if let ChatPhase::PickAgent { list_state, .. } = &chat.phase {
            assert_eq!(
                list_state.selected(),
                Some(2),
                "click selects the clicked row"
            );
        } else {
            panic!("expected PickAgent phase");
        }
    }

    #[tokio::test]
    async fn session_picker_double_click_resumes_selected_session() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Acp);
        let area = Rect::new(0, 0, 100, 30);
        let overlay_area = session_list_overlay_area(area);
        let mut state = ChatState::new(
            "sess-old".to_string(),
            "alpha".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        state.session_overlay = SessionOverlay::List {
            sessions: vec![crate::client::SessionEntry {
                session_id: "sess-new".to_string(),
                session_key: "sess-new".to_string(),
                created_at: "2026-07-07T00:00:00Z".to_string(),
                last_activity: "2026-07-07T00:01:00Z".to_string(),
                message_count: 1,
                agent_alias: Some("beta".to_string()),
                channel_id: None,
                name: Some("Beta work".to_string()),
            }],
            list_state,
        };
        chat.phase = ChatPhase::Active(Box::new(state));

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: overlay_area.x + 2,
            row: overlay_area.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        chat.handle_mouse(click, area).await;

        let switch = tokio::spawn(async move {
            chat.handle_mouse(click, area).await;
            chat
        });

        let request =
            next_rpc_request(&mut rx, "double-click should resume selected session").await;
        assert_eq!(request["method"], method::SESSION_NEW);
        assert_eq!(request["params"]["agent_alias"], "beta");
        assert_eq!(request["params"]["session_id"], "sess-new");
        assert_eq!(request["params"]["chat_mode"], "acp");
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "session_id": "sess-new",
                "workspace_dir": "/tmp/new"
            }),
        );

        let request = next_rpc_request(&mut rx, "successful switch should close old session").await;
        assert_eq!(request["method"], method::SESSION_CLOSE);
        assert_eq!(request["params"]["session_id"], "sess-old");
        respond_ok(&rpc, &request, serde_json::json!({}));

        let request = next_rpc_request(&mut rx, "double-click should refresh model identity").await;
        assert_eq!(request["method"], method::CONFIG_LIST);
        respond_ok(&rpc, &request, serde_json::json!([]));

        let request = next_rpc_request(&mut rx, "double-click should load history").await;
        assert_eq!(request["method"], method::SESSION_MESSAGES);
        assert_eq!(request["params"]["session_id"], "sess-new");
        respond_ok(
            &rpc,
            &request,
            serde_json::json!({
                "messages": [
                    {"role": "agent", "content": "restored"}
                ],
                "total": 1,
                "start": 0
            }),
        );

        let chat = tokio::time::timeout(Duration::from_secs(2), switch)
            .await
            .expect("double-click switch should finish")
            .unwrap();
        let ChatPhase::Active(state) = chat.phase else {
            panic!("double-click should leave the chat active");
        };
        assert_eq!(state.session_id, "sess-new");
        assert_eq!(state.agent_alias, "beta");
        assert_eq!(state.cwd.as_deref(), Some("/tmp/new"));
        assert!(matches!(state.session_overlay, SessionOverlay::None));
    }

    #[tokio::test]
    async fn session_picker_double_click_restore_error_keeps_old_session() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Acp);
        let area = Rect::new(0, 0, 100, 30);
        let overlay_area = session_list_overlay_area(area);
        let mut state = ChatState::new(
            "sess-old".to_string(),
            "alpha".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        state.session_overlay = SessionOverlay::List {
            sessions: vec![crate::client::SessionEntry {
                session_id: "sess-dead".to_string(),
                session_key: "sess-dead".to_string(),
                created_at: "2026-07-07T00:00:00Z".to_string(),
                last_activity: "2026-07-07T00:01:00Z".to_string(),
                message_count: 1,
                agent_alias: Some("beta".to_string()),
                channel_id: None,
                name: Some("Dead work".to_string()),
            }],
            list_state,
        };
        chat.phase = ChatPhase::Active(Box::new(state));

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: overlay_area.x + 2,
            row: overlay_area.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        chat.handle_mouse(click, area).await;

        let switch = tokio::spawn(async move {
            chat.handle_mouse(click, area).await;
            chat
        });

        let request = next_rpc_request(&mut rx, "double-click should try selected session").await;
        assert_eq!(request["method"], method::SESSION_NEW);
        assert_eq!(request["params"]["agent_alias"], "beta");
        assert_eq!(request["params"]["session_id"], "sess-dead");
        respond_err(
            &rpc,
            &request,
            crate::jsonrpc::error_codes::SESSION_NOT_FOUND,
            "Session not found",
        );

        let chat = tokio::time::timeout(Duration::from_secs(2), switch)
            .await
            .expect("failed switch should finish")
            .unwrap();
        let ChatPhase::Active(state) = chat.phase else {
            panic!("failed switch should keep the chat active");
        };
        assert_eq!(state.session_id, "sess-old");
        assert_eq!(state.agent_alias, "alpha");
        assert!(matches!(state.session_overlay, SessionOverlay::None));
        let info = state
            .info_message
            .as_ref()
            .expect("failed switch should surface an info-bar error");
        assert!(info.text.contains("Failed to switch session"));
        assert!(info.text.contains("Session not found"));
    }

    #[tokio::test]
    async fn active_agent_title_click_opens_agent_picker() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Chat);
        let area = Rect::new(10, 4, 80, 20);
        let mut state = ChatState::new(
            "abcdef1234".to_string(),
            "beta".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        state.refresh_title_hit_rects(area);
        chat.phase = ChatPhase::Active(Box::new(state));

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        let switch = tokio::spawn(async move {
            chat.handle_mouse(click, area).await;
            chat
        });

        let line = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("agent title click should request the agent list")
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], method::AGENTS_STATUS);
        let id = request["id"].as_str().unwrap().to_string();
        rpc.dispatch_response(
            &id,
            Some(serde_json::json!({
                "agents": [
                    {"alias": "alpha", "enabled": true, "live_sessions": 0},
                    {"alias": "beta", "enabled": true, "live_sessions": 1},
                    {"alias": "disabled", "enabled": false, "live_sessions": 0}
                ]
            })),
            None,
        );

        let chat = tokio::time::timeout(Duration::from_secs(2), switch)
            .await
            .expect("agent picker should open after agents/status response")
            .unwrap();
        let ChatPhase::PickAgent {
            agents, list_state, ..
        } = chat.phase
        else {
            panic!("expected PickAgent phase");
        };
        assert_eq!(agents, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(list_state.selected(), Some(1));
    }

    #[tokio::test]
    async fn active_agent_title_click_ignored_while_turn_in_flight() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Chat);
        let area = Rect::new(10, 4, 80, 20);
        let mut state = ChatState::new(
            "abcdef1234".to_string(),
            "beta".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        state.turn_in_flight = true;
        state.refresh_title_hit_rects(area);
        chat.phase = ChatPhase::Active(Box::new(state));

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };

        chat.handle_mouse(click, area).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "in-flight agent title click must not call agents/status"
        );
        assert!(
            matches!(chat.phase, ChatPhase::Active(_)),
            "in-flight agent title click must leave the active session visible"
        );
    }

    #[tokio::test]
    async fn input_bar_click_clears_transcript_mouse_highlight() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let (mut chat, _rx) = test_chat();
        let mut state = state();
        state
            .entries
            .push(ChatEntry::AgentMessage(Arc::<str>::from("hello")));
        state.mouse_down_entry = Some(0);
        state.mark_dirty_full();

        let area = Rect::new(0, 0, 80, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &mut state, area, PaneKind::Chat);
            })
            .expect("draw chat");

        state.transcript_selection = Some(TranscriptSelection {
            anchor: CellPoint { column: 0, row: 0 },
            head: CellPoint { column: 1, row: 0 },
            dragged: true,
        });

        state.dirty = LinesDirty::Clean;
        chat.phase = ChatPhase::Active(Box::new(state));

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: area.height.saturating_sub(2),
            modifiers: KeyModifiers::NONE,
        };
        chat.handle_mouse(click, area).await;

        let ChatPhase::Active(state) = &chat.phase else {
            panic!("expected active chat");
        };
        assert_eq!(state.transcript_selection, None);
        assert_eq!(state.mouse_down_entry, None);
        assert_eq!(
            state.dirty,
            LinesDirty::Full,
            "clearing the highlight must invalidate rendered transcript lines"
        );
    }

    #[tokio::test]
    async fn blank_side_click_clears_transcript_mouse_highlight() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let (mut chat, _rx) = test_chat();
        let mut state = state();
        state
            .entries
            .push(ChatEntry::AgentMessage(Arc::<str>::from("hi")));
        state.mouse_down_entry = Some(0);
        state.mark_dirty_full();

        let area = Rect::new(0, 0, 80, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &mut state, area, PaneKind::Chat);
            })
            .expect("draw chat");

        state.transcript_selection = Some(TranscriptSelection {
            anchor: CellPoint { column: 0, row: 0 },
            head: CellPoint { column: 1, row: 0 },
            dragged: true,
        });

        // The rendered entry rect must hug the text, not span the panel, so
        // there is blank space beside the short message to click in.
        let (_, rect) = state
            .entry_rects
            .iter()
            .find(|(idx, _)| *idx == 0)
            .copied()
            .expect("entry 0 has a screen rect");
        assert!(
            rect.width < area.width - 2,
            "short message rect must not span the full panel width: {rect:?}"
        );
        // A column just past the text but well within the panel — the blank
        // margin beside the message.
        let blank_col = rect.x + rect.width + 1;
        let blank_row = rect.y;
        assert!(
            blank_col < area.width - 1,
            "blank column stays in the panel"
        );

        state.dirty = LinesDirty::Clean;
        chat.phase = ChatPhase::Active(Box::new(state));

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: blank_col,
            row: blank_row,
            modifiers: KeyModifiers::NONE,
        };
        chat.handle_mouse(click, area).await;

        let ChatPhase::Active(state) = &chat.phase else {
            panic!("expected active chat");
        };
        assert_eq!(state.transcript_selection, None);
        assert_eq!(state.mouse_down_entry, None);
        assert_eq!(
            state.dirty,
            LinesDirty::Full,
            "clearing the highlight must invalidate rendered transcript lines"
        );
    }

    #[tokio::test]
    async fn plain_message_copy_action_stays_in_browse_mode() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let (mut chat, _rx) = test_chat();
        let mut state = state();
        state
            .entries
            .push(ChatEntry::AgentMessage(Arc::<str>::from("hello")));
        state.mark_dirty_full();

        let area = Rect::new(0, 0, 80, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &mut state, area, PaneKind::Chat);
            })
            .expect("draw chat");

        let entry_rect = state
            .entry_rects
            .first()
            .expect("entry region should be rendered")
            .1;
        let rows_before = state.cached_total_rows;
        state.dirty = LinesDirty::Clean;
        chat.phase = ChatPhase::Active(Box::new(state));

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: entry_rect.x + 1,
            row: entry_rect.y,
            modifiers: KeyModifiers::NONE,
        };
        chat.handle_mouse(click, area).await;

        let ChatPhase::Active(state) = &mut chat.phase else {
            panic!("expected active chat");
        };
        assert_eq!(
            state.browse_cursor, None,
            "normal-mode click must not enter browse mode"
        );
        assert!(
            state.info_message.is_none(),
            "normal-mode click must not copy or show copied feedback"
        );
        assert!(
            state.copy_hit_regions.is_empty(),
            "normal-mode click must not reveal a copy action"
        );

        state.enter_browse_mode();

        terminal
            .draw(|frame| {
                render(frame, state, area, PaneKind::Chat);
            })
            .expect("redraw browse-mode chat");
        let selected_entry_rect = state
            .entry_rects
            .first()
            .expect("selected entry region should still be rendered")
            .1;
        assert_eq!(
            state.cached_total_rows, rows_before,
            "message copy affordance must overlay the transcript without adding rows"
        );
        assert_eq!(
            selected_entry_rect.y, entry_rect.y,
            "revealing message copy must not push earlier transcript rows"
        );

        let copy_rect = state
            .copy_hit_regions
            .iter()
            .find(|region| region.text == "hello")
            .expect("browse-mode selected message copy action should be rendered")
            .rect;
        assert_eq!(
            copy_rect.y, selected_entry_rect.y,
            "message copy action should overlay the selected row"
        );
        let body_x = area.x + 1;
        let body_width = area.width.saturating_sub(2);
        let expected_x = body_x + body_width.saturating_sub(copy_rect.width) / 2;
        assert_eq!(
            copy_rect.x, expected_x,
            "message copy action should be horizontally centered in the chat body"
        );
        let copy_click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: copy_rect.x,
            row: copy_rect.y,
            modifiers: KeyModifiers::NONE,
        };
        chat.handle_mouse(copy_click, area).await;

        let ChatPhase::Active(state) = &chat.phase else {
            panic!("expected active chat");
        };
        assert_eq!(
            state.info_message.as_ref().map(|m| m.text.as_str()),
            Some(crate::i18n::t("zc-chat-copied-clipboard").as_str()),
            "explicit message copy action should copy"
        );
        assert_eq!(
            state.browse_cursor, None,
            "copy action should dismiss selection"
        );
        assert!(
            matches!(
                state.copy_feedback,
                Some(CopyFeedback {
                    target: CopyFeedbackTarget::Overlay(_),
                    ..
                })
            ),
            "message copy should leave a transient copied-state cue"
        );
    }

    #[tokio::test]
    async fn modifier_click_does_not_copy_whole_message_outside_browse_mode() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let (mut chat, _rx) = test_chat();
        let mut state = state();
        state.entries.push(ChatEntry::AgentMessage(Arc::<str>::from(
            "select just this word",
        )));
        state.mark_dirty_full();

        let area = Rect::new(0, 0, 80, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &mut state, area, PaneKind::Chat);
            })
            .expect("draw chat");
        let entry_rect = state
            .entry_rects
            .first()
            .expect("entry region should be rendered")
            .1;
        state.dirty = LinesDirty::Clean;
        chat.phase = ChatPhase::Active(Box::new(state));

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: entry_rect.x + 1,
            row: entry_rect.y,
            modifiers: KeyModifiers::CONTROL,
        };
        chat.handle_mouse(click, area).await;

        let ChatPhase::Active(state) = &chat.phase else {
            panic!("expected active chat");
        };
        assert!(
            state.info_message.is_none(),
            "modifier-click outside browse mode must not app-copy the whole message"
        );
        assert_eq!(
            state.browse_cursor, None,
            "modifier-click outside browse mode should not select the whole message"
        );
    }

    #[tokio::test]
    async fn mouse_up_after_browse_drag_does_not_copy_selection() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let (mut chat, _rx) = test_chat();
        let mut state = state();
        state
            .entries
            .push(ChatEntry::AgentMessage(Arc::<str>::from("first")));
        state
            .entries
            .push(ChatEntry::AgentMessage(Arc::<str>::from("second")));
        state.browse_cursor = Some(1);
        state.browse_anchor = Some(0);
        state.mouse_down_entry = Some(0);
        state.mark_dirty_full();
        chat.phase = ChatPhase::Active(Box::new(state));

        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        chat.handle_mouse(up, Rect::new(0, 0, 80, 20)).await;

        let ChatPhase::Active(state) = &chat.phase else {
            panic!("expected active chat");
        };
        assert_eq!(state.mouse_down_entry, None);
        assert!(
            state.info_message.is_none(),
            "ending a mouse drag must not app-copy the selected messages"
        );
    }

    #[tokio::test]
    async fn code_copy_shows_shared_feedback() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let (mut chat, _rx) = test_chat();
        let mut state = state();
        state
            .entries
            .push(ChatEntry::AgentMessage(Arc::<str>::from("previous")));
        state.entries.push(ChatEntry::AgentMessage(Arc::<str>::from(
            "```bash\necho hello\n```",
        )));
        state.mark_dirty_full();
        assert!(!state.in_browse_mode());

        let area = Rect::new(0, 0, 80, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &mut state, area, PaneKind::Chat);
            })
            .expect("draw chat");

        let code_regions: Vec<CopyHitRegion> = state
            .copy_hit_regions
            .iter()
            .filter(|region| region.text == "echo hello")
            .cloned()
            .collect();
        assert_eq!(
            code_regions.len(),
            2,
            "top and bottom fence labels should both be copy targets"
        );
        assert_eq!(
            code_regions[0].group, code_regions[1].group,
            "top and bottom copy targets for one fence should share feedback"
        );
        let copy_rect = code_regions[0].rect;
        let copy_group = code_regions[0].group;
        state.dirty = LinesDirty::Clean;
        chat.phase = ChatPhase::Active(Box::new(state));

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: copy_rect.x,
            row: copy_rect.y,
            modifiers: KeyModifiers::NONE,
        };
        chat.handle_mouse(click, area).await;

        let ChatPhase::Active(state) = &chat.phase else {
            panic!("expected active chat");
        };
        assert_eq!(state.mouse_down_entry, None);
        assert_eq!(
            state.info_message.as_ref().map(|m| m.text.as_str()),
            Some(crate::i18n::t("zc-chat-copied-clipboard").as_str())
        );
        assert!(matches!(
            state.copy_feedback,
            Some(CopyFeedback {
                target: CopyFeedbackTarget::Code(group),
                ..
            }) if group == copy_group
        ));
    }

    fn authoritative_rows(s: &ChatState, width: u16) -> u16 {
        Paragraph::new(s.cached_lines.iter().map(borrow_line).collect::<Vec<_>>())
            .wrap(Wrap { trim: false })
            .line_count(width) as u16
    }

    #[test]
    fn cached_total_rows_matches_full_line_count() {
        let width: u16 = 40;
        let mut s = state();

        for i in 0..50 {
            s.push_user_message(Some(format!("message number {i} with enough text to wrap across the forty column width budget")), Vec::new());
        }
        s.rebuild_lines(width);
        assert_eq!(
            s.cached_total_rows,
            authoritative_rows(&s, width),
            "full-rebuild row total must match line_count"
        );

        for i in 50..60 {
            s.push_user_message(
                Some(format!(
                    "appended message {i} also long enough to wrap somewhere in the middle of a row"
                )),
                Vec::new(),
            );
        }
        s.rebuild_lines(width);
        assert_eq!(
            s.cached_total_rows,
            authoritative_rows(&s, width),
            "incremental-append row total must match line_count"
        );

        let narrower: u16 = 20;
        s.rebuild_lines(narrower);
        assert_eq!(
            s.cached_total_rows,
            authoritative_rows(&s, narrower),
            "width change must force a recompute that still matches line_count"
        );
    }

    #[tokio::test]
    async fn chat_entry_refresh_reloads_agents_from_error_phase() {
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Chat);
        chat.phase = ChatPhase::Error("No enabled agents yet.".to_string());

        let refresh = tokio::spawn(async move {
            chat.refresh_if_inactive().await;
            chat
        });

        let line = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("refresh should request the agent list")
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], method::AGENTS_STATUS);

        let id = request["id"].as_str().unwrap().to_string();
        rpc.dispatch_response(
            &id,
            Some(serde_json::json!({
                "agents": [
                    {"alias": "alpha", "enabled": true, "live_sessions": 0, "persisted_sessions": 0},
                    {"alias": "beta", "enabled": true, "live_sessions": 0, "persisted_sessions": 0}
                ]
            })),
            None,
        );

        let chat = tokio::time::timeout(Duration::from_secs(2), refresh)
            .await
            .expect("refresh should finish after agents/status response")
            .unwrap();
        let ChatPhase::PickAgent {
            agents, loading, ..
        } = chat.phase
        else {
            panic!("refresh should leave stale error state");
        };
        assert_eq!(agents, vec!["alpha".to_string(), "beta".to_string()]);
        assert!(!loading);
    }

    #[tokio::test]
    async fn chat_entry_refresh_reloads_agents_from_pick_phase() {
        // Re-entering the pane while parked on the picker must re-fetch the
        // agent list so an agent created elsewhere (Quickstart / Config) shows
        // up — and the existing highlight must survive the refresh. Regression
        // for "new agent missing from Code/Chat tab when agents already exist".
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(Arc::clone(&rpc)));
        let mut chat = Chat::new(client, PaneKind::Chat);
        let mut list_state = ListState::default();
        list_state.select(Some(1)); // user has "beta" highlighted
        chat.phase = ChatPhase::PickAgent {
            agents: vec!["alpha".to_string(), "beta".to_string()],
            list_state,
            loading: false,
        };

        let refresh = tokio::spawn(async move {
            chat.refresh_if_inactive().await;
            chat
        });

        let line = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("refresh should request the agent list")
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], method::AGENTS_STATUS);

        let id = request["id"].as_str().unwrap().to_string();
        rpc.dispatch_response(
            &id,
            Some(serde_json::json!({
                "agents": [
                    {"alias": "alpha", "enabled": true, "live_sessions": 0},
                    {"alias": "beta", "enabled": true, "live_sessions": 0},
                    {"alias": "gamma", "enabled": true, "live_sessions": 0}
                ]
            })),
            None,
        );

        let chat = tokio::time::timeout(Duration::from_secs(2), refresh)
            .await
            .expect("refresh should finish after agents/status response")
            .unwrap();
        let ChatPhase::PickAgent {
            agents, list_state, ..
        } = chat.phase
        else {
            panic!("refresh should keep the agent picker");
        };
        // The newly-created agent is now present...
        assert_eq!(
            agents,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );
        // ...and the prior highlight ("beta", row 1) is preserved.
        assert_eq!(list_state.selected(), Some(1));
    }

    #[tokio::test]
    async fn apply_update_during_turn_in_flight() {
        let mut s = state();
        s.turn_in_flight = true;
        s.apply_update(SessionUpdate::AgentMessageChunk {
            session_id: "sess-1".to_string(),
            text: "streaming...".to_string(),
        });
        assert_eq!(s.current_agent_text(), "streaming...");
    }

    #[test]
    fn input_append_and_clear() {
        let mut s = state();
        s.input_bar.push_input_char('h');
        s.input_bar.push_input_char('i');
        assert_eq!(s.input_bar.input(), "hi");
        let taken = s.input_bar.take_input();
        assert_eq!(taken, "hi");
        assert_eq!(s.input_bar.input(), "");
    }

    #[test]
    fn text_chunk_accumulates() {
        let mut s = state();
        s.apply_update(SessionUpdate::AgentMessageChunk {
            session_id: "sess-1".to_string(),
            text: "Hello".to_string(),
        });
        s.apply_update(SessionUpdate::AgentMessageChunk {
            session_id: "sess-1".to_string(),
            text: " world".to_string(),
        });
        assert_eq!(s.current_agent_text(), "Hello world");
    }

    #[test]
    fn history_trimmed_update_adds_visible_system_notice() {
        let mut s = state();
        s.apply_update(SessionUpdate::HistoryTrimmed {
            session_id: "sess-1".to_string(),
            dropped_messages: 12,
            kept_turns: 3,
            reason: "history message limit exceeded".to_string(),
        });

        assert!(matches!(
            s.entries().last(),
            Some(ChatEntry::SystemMessage(text))
                if text.contains("history message limit exceeded")
                    && text.contains("12")
                    && text.contains("3")
        ));
    }

    #[test]
    fn tool_call_followed_by_result_is_one_entry() {
        let mut s = state();
        s.apply_update(SessionUpdate::ToolCall {
            session_id: "sess-1".to_string(),
            tool_call_id: "tc1".to_string(),
            name: "shell".to_string(),
            raw_input: serde_json::json!({"command":"ls"}),
        });
        s.apply_update(SessionUpdate::ToolResult {
            session_id: "sess-1".to_string(),
            tool_call_id: "tc1".to_string(),
            raw_output: "file.txt\n".to_string(),
        });
        let entries = s.entries();
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            ChatEntry::Tool {
                result: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn approval_request_sets_pending_approval() {
        let mut s = state();
        s.apply_update(SessionUpdate::ApprovalRequest {
            session_id: "sess-1".to_string(),
            request_id: "req-1".to_string(),
            tool_name: "shell".to_string(),
            arguments_summary: "rm -rf /".to_string(),
            timeout_secs: 30,
        });
        assert!(s.pending_approval().is_some());
        let pa = s.pending_approval().unwrap();
        assert_eq!(pa.request_id, "req-1");
        assert_eq!(pa.tool_name, "shell");
    }

    #[test]
    fn approval_overlay_uses_theme_background_after_clear() {
        use ratatui::{Terminal, backend::TestBackend};

        let _theme_guard = theme::set_active_for_test(theme::default_theme());
        let expected_bg = theme::background();
        assert_ne!(
            expected_bg,
            ratatui::style::Color::Reset,
            "default ZeroCode theme should provide a concrete modal background"
        );

        let mut s = state();
        s.apply_update(SessionUpdate::ApprovalRequest {
            session_id: "sess-1".to_string(),
            request_id: "req-1".to_string(),
            tool_name: "shell".to_string(),
            arguments_summary: "command: pwd".to_string(),
            timeout_secs: 120,
        });

        let area = Rect::new(0, 0, 100, 30);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_approval_overlay(frame, &s, area);
            })
            .expect("draw approval overlay");

        let cell = &terminal.backend().buffer()[(10, 28)];
        assert_eq!(
            cell.style().bg,
            Some(expected_bg),
            "approval overlay interior must use the active ZeroCode theme background"
        );
    }

    #[test]
    fn queue_sidebar_uses_theme_background_after_clear() {
        use ratatui::{Terminal, backend::TestBackend};

        let _theme_guard = theme::set_active_for_test(theme::default_theme());
        let expected_bg = theme::background();
        assert_ne!(
            expected_bg,
            ratatui::style::Color::Reset,
            "default ZeroCode theme should provide a concrete sidebar background"
        );

        let mut s = state();
        s.enqueue_message("what's happening".to_string(), Vec::new())
            .expect("queue message");
        s.enqueue_message("second queued message".to_string(), Vec::new())
            .expect("queue message");
        s.ensure_queue_selection();

        let area = Rect::new(0, 0, 36, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_queue_sidebar(frame, &mut s, area);
            })
            .expect("draw queue sidebar");

        let cell = &terminal.backend().buffer()[(4, 6)];
        assert_eq!(
            cell.style().bg,
            Some(expected_bg),
            "queue sidebar interior must use the active ZeroCode theme background"
        );

        let unselected_text_cell = &terminal.backend().buffer()[(6, 2)];
        assert_eq!(
            unselected_text_cell.style().fg,
            Some(theme::active().body),
            "unselected queue text must use the active ZeroCode body foreground"
        );
        assert_eq!(
            unselected_text_cell.style().bg,
            Some(expected_bg),
            "unselected queue text must keep the themed fill background"
        );
    }

    #[test]
    fn session_list_overlay_uses_theme_background_after_clear() {
        use ratatui::{Terminal, backend::TestBackend};

        let _theme_guard = theme::set_active_for_test(theme::default_theme());
        let expected_bg = theme::background();
        assert_ne!(
            expected_bg,
            ratatui::style::Color::Reset,
            "default ZeroCode theme should provide a concrete modal background"
        );

        let sessions = vec![SessionEntry {
            session_id: "session-1".to_string(),
            session_key: "session-1".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_activity: "2026-01-01T00:00:00Z".to_string(),
            agent_alias: Some("agent".to_string()),
            channel_id: None,
            name: Some("first prompt".to_string()),
            message_count: 1,
        }];
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        let area = Rect::new(0, 0, 100, 30);
        let overlay_area = session_list_overlay_area(area);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_session_list_overlay(
                    frame,
                    area,
                    &sessions,
                    &list_state,
                    crate::i18n::t("zc-chat-session-list-switch-title"),
                );
            })
            .expect("draw session list overlay");

        let cell = &terminal.backend().buffer()[(overlay_area.x + 4, overlay_area.y + 6)];
        assert_eq!(
            cell.style().bg,
            Some(expected_bg),
            "session list overlay interior must use the active ZeroCode theme background"
        );

        let selected_text_cell =
            &terminal.backend().buffer()[(overlay_area.x + 4, overlay_area.y + 1)];
        assert_eq!(
            selected_text_cell.style().fg,
            Some(theme::active().heading),
            "selected session row must keep the overlay highlight foreground"
        );
        assert_eq!(
            selected_text_cell.style().bg,
            Some(expected_bg),
            "selected session row must keep the themed fill background"
        );
    }

    #[test]
    fn thought_chunk_visible_before_commit() {
        let mut s = state();
        s.turn_in_flight = true;
        s.apply_update(SessionUpdate::AgentThoughtChunk {
            session_id: "sess-1".to_string(),
            text: "reasoning...".to_string(),
        });
        assert_eq!(s.current_thought_text(), "reasoning...");
        assert!(
            s.entries().is_empty(),
            "thought must not become an entry mid-turn"
        );
    }

    #[test]
    fn thought_flushed_as_entry_before_tool_call() {
        let mut s = state();
        s.turn_in_flight = true;
        s.apply_update(SessionUpdate::AgentThoughtChunk {
            session_id: "sess-1".to_string(),
            text: "plan: run ls".to_string(),
        });
        s.apply_update(SessionUpdate::ToolCall {
            session_id: "sess-1".to_string(),
            tool_call_id: "tc1".to_string(),
            name: "shell".to_string(),
            raw_input: serde_json::json!({"command": "ls"}),
        });
        // Thought must be committed as an entry before the tool entry.
        assert_eq!(s.entries().len(), 2);
        assert!(
            matches!(&s.entries()[0], ChatEntry::AgentThought(t) if t.as_ref() == "plan: run ls")
        );
        assert!(matches!(&s.entries()[1], ChatEntry::Tool { .. }));
        // streaming_thought is now clear.
        assert!(s.current_thought_text().is_empty());
    }

    #[test]
    fn thought_flushed_as_entry_before_first_response_chunk() {
        let mut s = state();
        s.turn_in_flight = true;
        s.apply_update(SessionUpdate::AgentThoughtChunk {
            session_id: "sess-1".to_string(),
            text: "thinking".to_string(),
        });
        s.apply_update(SessionUpdate::AgentMessageChunk {
            session_id: "sess-1".to_string(),
            text: "Here is".to_string(),
        });
        // Thought entry committed before streaming text starts.
        assert_eq!(s.entries().len(), 1);
        assert!(matches!(&s.entries()[0], ChatEntry::AgentThought(t) if t.as_ref() == "thinking"));
        assert_eq!(s.current_agent_text(), "Here is");
        assert!(s.current_thought_text().is_empty());
    }

    #[test]
    fn subsequent_message_chunks_do_not_re_flush_thought() {
        let mut s = state();
        s.turn_in_flight = true;
        s.apply_update(SessionUpdate::AgentThoughtChunk {
            session_id: "sess-1".to_string(),
            text: "thinking".to_string(),
        });
        s.apply_update(SessionUpdate::AgentMessageChunk {
            session_id: "sess-1".to_string(),
            text: "Hello".to_string(),
        });
        s.apply_update(SessionUpdate::AgentMessageChunk {
            session_id: "sess-1".to_string(),
            text: " world".to_string(),
        });
        // Only one AgentThought entry, not two.
        assert_eq!(s.entries().len(), 1);
        assert_eq!(s.current_agent_text(), "Hello world");
    }

    // ── Interleaving regression tests ────────────────────────────

    #[test]
    fn text_before_tool_call_is_flushed_as_separate_agent_message() {
        let mut s = state();
        s.turn_in_flight = true;

        // Pre-tool text chunk.
        s.apply_update(SessionUpdate::AgentMessageChunk {
            session_id: "sess-1".to_string(),
            text: "I will run ls.".to_string(),
        });

        // Tool call interrupts the text stream.
        s.apply_update(SessionUpdate::ToolCall {
            session_id: "sess-1".to_string(),
            tool_call_id: "tc1".to_string(),
            name: "shell".to_string(),
            raw_input: serde_json::json!({"command": "ls"}),
        });

        // At this point the pre-tool text must be committed as its own entry.
        assert_eq!(
            s.entries().len(),
            2,
            "expected AgentMessage + Tool entries, got {:?}",
            s.entries()
        );
        assert!(
            matches!(&s.entries()[0], ChatEntry::AgentMessage(t) if t.as_ref() == "I will run ls."),
            "first entry must be AgentMessage with pre-tool text"
        );
        assert!(
            matches!(&s.entries()[1], ChatEntry::Tool { .. }),
            "second entry must be Tool"
        );
        // streaming_text must be cleared after the flush.
        assert!(
            s.current_agent_text().is_empty(),
            "streaming_text must be empty after tool-call flush"
        );
    }

    #[test]
    fn text_after_tool_call_commits_separately() {
        let mut s = state();
        s.turn_in_flight = true;

        // Pre-tool text.
        s.apply_update(SessionUpdate::AgentMessageChunk {
            session_id: "sess-1".to_string(),
            text: "Running ls.".to_string(),
        });
        // Tool call flushes pre-tool text.
        s.apply_update(SessionUpdate::ToolCall {
            session_id: "sess-1".to_string(),
            tool_call_id: "tc1".to_string(),
            name: "shell".to_string(),
            raw_input: serde_json::json!({"command": "ls"}),
        });
        // Tool result.
        s.apply_update(SessionUpdate::ToolResult {
            session_id: "sess-1".to_string(),
            tool_call_id: "tc1".to_string(),
            raw_output: "file.txt\n".to_string(),
        });
        // Post-tool text.
        s.apply_update(SessionUpdate::AgentMessageChunk {
            session_id: "sess-1".to_string(),
            text: "Done.".to_string(),
        });
        assert_eq!(s.current_agent_text(), "Done.");

        // commit_turn: only the post-tool text should become a new AgentMessage.
        s.commit_turn("Done.".to_string(), true);

        // Final order: AgentMessage("Running ls.") | Tool | AgentMessage("Done.")
        assert_eq!(
            s.entries().len(),
            3,
            "expected 3 entries: pre-tool AgentMessage, Tool, post-tool AgentMessage"
        );
        assert!(
            matches!(&s.entries()[0], ChatEntry::AgentMessage(t) if t.as_ref() == "Running ls."),
            "first entry must be pre-tool AgentMessage"
        );
        assert!(
            matches!(
                &s.entries()[1],
                ChatEntry::Tool {
                    result: Some(_),
                    ..
                }
            ),
            "second entry must be Tool with result"
        );
        assert!(
            matches!(&s.entries()[2], ChatEntry::AgentMessage(t) if t.as_ref() == "Done."),
            "third entry must be post-tool AgentMessage"
        );
    }

    #[test]
    fn no_spurious_agent_message_when_no_pre_tool_text() {
        let mut s = state();
        s.turn_in_flight = true;

        // Tool call with no preceding text chunk.
        s.apply_update(SessionUpdate::ToolCall {
            session_id: "sess-1".to_string(),
            tool_call_id: "tc1".to_string(),
            name: "shell".to_string(),
            raw_input: serde_json::json!({"command": "ls"}),
        });

        // Only the Tool entry should exist — no empty AgentMessage.
        assert_eq!(s.entries().len(), 1);
        assert!(matches!(&s.entries()[0], ChatEntry::Tool { .. }));
    }

    #[test]
    fn commit_turn_does_not_duplicate_already_flushed_text() {
        let mut s = state();
        s.turn_in_flight = true;

        s.apply_update(SessionUpdate::AgentMessageChunk {
            session_id: "sess-1".to_string(),
            text: "Before tool.".to_string(),
        });
        s.apply_update(SessionUpdate::ToolCall {
            session_id: "sess-1".to_string(),
            tool_call_id: "tc1".to_string(),
            name: "shell".to_string(),
            raw_input: serde_json::json!({"command": "ls"}),
        });
        // No post-tool text; commit_turn receives the full text but streaming_text is empty.
        s.commit_turn("Before tool.".to_string(), true);

        // Must be exactly: AgentMessage("Before tool.") | Tool
        // NOT: AgentMessage | Tool | AgentMessage (duplicate)
        assert_eq!(
            s.entries().len(),
            2,
            "commit_turn must not add a duplicate AgentMessage for already-flushed text"
        );
        assert!(
            matches!(&s.entries()[0], ChatEntry::AgentMessage(t) if t.as_ref() == "Before tool.")
        );
        assert!(matches!(&s.entries()[1], ChatEntry::Tool { .. }));
    }

    /// When no streaming text was accumulated, commit_turn must use the
    /// daemon-provided final text as a fallback — rendered exactly once.
    #[test]
    fn commit_turn_renders_nonempty_fallback_when_no_streaming() {
        let mut s = state();
        s.turn_in_flight = true;

        // No streaming chunks; commit_turn receives non-empty final text.
        s.commit_turn("Hello from daemon.".to_string(), true);

        assert_eq!(s.entries().len(), 1);
        assert!(
            matches!(&s.entries()[0], ChatEntry::AgentMessage(t) if t.as_ref() == "Hello from daemon.")
        );
    }

    /// When a turn completes with no streamed text, no tool calls, and no
    /// final content, commit_turn must render a diagnostic system message
    /// so the user knows the turn finished.
    #[test]
    fn commit_turn_shows_diagnostic_when_no_output_at_all() {
        let mut s = state();
        s.turn_in_flight = true;

        // Empty everything: no streaming, no tools, empty final text.
        s.commit_turn(String::new(), true);

        assert_eq!(s.entries().len(), 1);
        assert!(
            matches!(&s.entries()[0], ChatEntry::SystemMessage(t) if t.as_ref() == "Turn completed with no output."),
            "expected diagnostic SystemMessage for empty completion, got {:?}",
            s.entries()[0]
        );
    }

    /// When a cancelled or failed turn has no output, commit_turn must NOT
    /// append the "Turn completed with no output" diagnostic — cancelled/
    /// failed turns are not clean completions and should not claim otherwise.
    #[test]
    fn commit_turn_no_diagnostic_when_not_clean() {
        let mut s = state();
        s.turn_in_flight = true;

        // Clean=false (cancelled/failed), empty everything.
        s.commit_turn(String::new(), false);

        assert!(
            s.entries().is_empty(),
            "cancelled turn should not emit completion diagnostic, got {:?}",
            s.entries()
        );
    }

    /// When tool calls were made during a turn but no text was streamed and
    /// final text is empty, commit_turn must NOT add a diagnostic — the tool
    /// entries are the visible record of work.
    #[test]
    fn commit_turn_no_diagnostic_when_tool_calls_present() {
        let mut s = state();
        s.turn_in_flight = true;

        s.apply_update(SessionUpdate::ToolCall {
            session_id: "sess-1".to_string(),
            tool_call_id: "tc1".to_string(),
            name: "shell".to_string(),
            raw_input: serde_json::json!({"command": "ls"}),
        });
        s.commit_turn(String::new(), true);

        // Only the Tool entry — no diagnostic needed.
        assert_eq!(s.entries().len(), 1);
        assert!(matches!(&s.entries()[0], ChatEntry::Tool { .. }));
    }

    #[test]
    fn turn_commit_flushes_streaming_buffer() {
        let mut s = state();
        s.apply_update(SessionUpdate::AgentMessageChunk {
            session_id: "sess-1".to_string(),
            text: "Done".to_string(),
        });
        s.commit_turn("Done".to_string(), true);
        assert_eq!(s.current_agent_text(), "");
        assert!(
            s.entries()
                .iter()
                .any(|e| matches!(e, ChatEntry::AgentMessage(t) if t.as_ref() == "Done"))
        );
    }

    // ── markdown_to_lines ──────────────────────────────────────────

    fn rendered(input: &str, width: u16) -> String {
        markdown_to_lines(input, width)
            .into_iter()
            .map(|l| {
                l.spans
                    .into_iter()
                    .map(|s| s.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn md_code_block_bars_span_full_width() {
        let width: u16 = 50;
        let out = rendered("```rust\nlet x = 1;\n```\n", width);
        let header = out.lines().find(|l| l.starts_with('\u{250c}')).unwrap();
        let footer = out.lines().find(|l| l.starts_with('\u{2514}')).unwrap();
        assert_eq!(header.chars().count(), width as usize, "header: {header:?}");
        assert_eq!(
            header.chars().count(),
            footer.chars().count(),
            "header and footer must match width"
        );
        let copy_col = |l: &str| l.chars().take_while(|c| *c != '[').count();
        assert_eq!(
            copy_col(header),
            copy_col(footer),
            "[Copy] must start at the same column on header and footer\nheader: {header:?}\nfooter: {footer:?}"
        );
    }

    #[test]
    fn md_code_block_header_shows_language() {
        let out = rendered("```python\nx = 1\n```\n", 50);
        let header = out.lines().find(|l| l.starts_with('\u{250c}')).unwrap();
        assert!(
            header.contains(" python "),
            "header must show the fence language: {header:?}"
        );
        assert!(
            !header.contains(" code "),
            "labeled fence must not fall back to ` code `: {header:?}"
        );
    }

    #[test]
    fn md_code_block_header_strips_info_extras() {
        let out = rendered("```python title=\"x\"\nx = 1\n```\n", 50);
        let header = out.lines().find(|l| l.starts_with('\u{250c}')).unwrap();
        assert!(
            header.contains(" python "),
            "only the language token is used as the label: {header:?}"
        );
        assert!(
            !header.contains("title"),
            "info-string extras must not leak into the label: {header:?}"
        );
    }

    #[test]
    fn md_code_block_unlabeled_fence_falls_back() {
        let out = rendered("```\nx = 1\n```\n", 50);
        let header = out.lines().find(|l| l.starts_with('\u{250c}')).unwrap();
        assert!(
            header.contains(" code "),
            "unlabeled fence keeps the ` code ` fallback: {header:?}"
        );
    }

    #[test]
    fn md_code_block_body_has_no_left_gutter() {
        let out = rendered("```rust\nlet x = 1;\n```\n", 50);
        let body = out
            .lines()
            .find(|l| l.contains("let x = 1;"))
            .expect("code body line");
        assert!(
            !body.starts_with('\u{2502}'),
            "code body must not start with a vertical gutter: {body:?}"
        );
        assert_eq!(
            body.strip_prefix("  ").map(str::trim_end),
            Some("let x = 1;"),
            "body line is two-space indented code: {body:?}"
        );
    }

    #[test]
    fn md_code_block_body_is_syntax_highlighted() {
        let _g = theme::set_active_for_test(
            theme::theme_by_name("icy_blue").expect("icy_blue registered"),
        );
        let lines = markdown_to_lines("```rust\nfn main() {}\n```\n", 60);
        let body = lines
            .iter()
            .find(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .contains("fn")
            })
            .expect("code body line");
        assert!(
            body.spans.len() > 2,
            "highlighted body should split into multiple token spans, got {}",
            body.spans.len()
        );
        let keyword_fg = theme::SyntaxScope::Keyword.color();
        assert!(
            body.spans.iter().any(|s| s.style.fg == Some(keyword_fg)),
            "the `fn` keyword should carry the themed keyword colour"
        );
    }

    #[test]
    fn md_code_block_unknown_language_stays_plain() {
        let lines = markdown_to_lines("```nonexistent_lang_xyz\nfoo bar\n```\n", 60);
        let body = lines
            .iter()
            .find(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .contains("foo bar")
            })
            .expect("code body line");
        let text: String = body.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            text.strip_prefix("  ").map(str::trim_end),
            Some("foo bar"),
            "unknown language keeps the flat two-space-indented body: {text:?}"
        );
    }

    #[test]
    fn copy_label_cells_locate_copy_on_header_bar() {
        let lines = markdown_to_lines("```rust\nlet x = 1;\n```\n", 50);
        let header = lines
            .iter()
            .find(|l| {
                l.spans
                    .first()
                    .map(|s| s.content.starts_with('\u{250c}'))
                    .unwrap_or(false)
            })
            .expect("header bar");
        let (col, cells) = label_cells(header, " [Copy] ").expect("copy label present");
        assert_eq!(cells, "[Copy]".chars().count() as u16);
        // The cell at `col` on the rendered header must be the '[' of [Copy].
        let rendered: String = header
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert_eq!(
            rendered.chars().nth(col as usize),
            Some('['),
            "label_cells column must point at '[' of [Copy]: {rendered:?}"
        );
    }

    #[test]
    fn copy_region_recovers_full_highlighted_body() {
        let _g = theme::set_active_for_test(
            theme::theme_by_name("icy_blue").expect("icy_blue registered"),
        );
        let mut state = ChatState::new(
            "sess".to_string(),
            "agent".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        state.cached_lines = markdown_to_lines("```rust\nfn main() {}\nlet y = 2;\n```\n", 60);
        let body = Rect::new(0, 0, 60, 20);
        state.rebuild_copy_regions(60, 0, body);
        assert!(
            !state.copy_hit_regions.is_empty(),
            "a highlighted fence must still register copy regions"
        );
        assert_eq!(
            state.copy_hit_regions[0].text, "fn main() {}\nlet y = 2;",
            "copy text contains only the code body without markdown fences"
        );
    }

    #[test]
    fn copy_region_unlabeled_fence_omits_language() {
        let mut state = ChatState::new(
            "sess".to_string(),
            "agent".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        state.cached_lines = markdown_to_lines("```\nplain text\n```\n", 60);
        let body = Rect::new(0, 0, 60, 20);
        state.rebuild_copy_regions(60, 0, body);
        assert_eq!(
            state.copy_hit_regions[0].text, "plain text",
            "copy text contains only the code body without fences"
        );
    }

    #[test]
    fn copy_regions_track_scroll_with_history_above_viewport() {
        let _g = theme::set_active_for_test(
            theme::theme_by_name("icy_blue").expect("icy_blue registered"),
        );
        let mut state = ChatState::new(
            "sess".to_string(),
            "agent".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        let pad = "filler line\n".repeat(200);
        state
            .entries
            .push(ChatEntry::AgentMessage(Arc::<str>::from(pad.as_str())));
        state.entries.push(ChatEntry::AgentMessage(Arc::<str>::from(
            "```rust\nfn main() {}\n```\n",
        )));
        state.dirty = LinesDirty::Full;
        state.rebuild_lines(60);

        let fence_entry = state.cached_screen_ranges.last().copied().expect("fence");
        let body = Rect::new(0, 0, 60, 20);

        state.rebuild_copy_regions(60, fence_entry.1, body);
        assert_eq!(
            state.copy_hit_regions[0].text, "fn main() {}",
            "scrolled-to fence registers a copy region with body only"
        );

        state.rebuild_copy_regions(60, 0, body);
        assert!(
            state.copy_hit_regions.is_empty(),
            "fence far below the viewport registers nothing"
        );
    }

    #[test]
    fn fenced_text_returns_body_without_markdown_fences() {
        assert_eq!(fenced_text(Some("python"), "x = 1"), "x = 1");
        assert_eq!(fenced_text(None, "x = 1"), "x = 1");
    }

    #[test]
    fn md_table_renders_box_drawing_borders() {
        let out = rendered("| A | B |\n|---|---|\n| 1 | 2 |\n", 40);
        assert!(out.contains('\u{250C}'), "missing top-left corner: {out}");
        assert!(
            out.contains('\u{2514}'),
            "missing bottom-left corner: {out}"
        );
        assert!(out.contains('\u{2502}'), "missing vertical: {out}");
        assert!(out.contains('A'));
        assert!(out.contains('1'));
    }

    #[test]
    fn md_table_truncates_when_width_is_tight() {
        let out = rendered(
            "| col |\n|-----|\n| this cell is far too long for a tiny width |\n",
            20,
        );
        assert!(out.contains('\u{2026}'), "expected ellipsis: {out}");
    }

    #[test]
    fn md_table_pads_emoji_presentation_to_two_cells() {
        // 🏔️ is U+1F3D4 + U+FE0F. Natural column width must be 2 (not 1), so a
        // wider sibling cell still leaves a full cell of space after the glyph.
        let out = rendered("| A | B |\n|---|---|\n| \u{1F3D4}\u{FE0F} | xx |\n", 40);
        let data = out
            .lines()
            .find(|l| l.contains('\u{1F3D4}'))
            .expect("emoji data row");
        let emoji = "\u{1F3D4}\u{FE0F}";
        let idx = data.find(emoji).expect("emoji in row");
        let after = &data[idx + emoji.len()..];
        // Column budget for A is max(width("A"), width(emoji)) = 2, so after
        // the emoji there is no content pad — only the trailing cell space
        // before the border.
        assert!(
            after.starts_with(" \u{2502}"),
            "emoji column natural width is 2 cells: {data:?}"
        );
        // And the header cell for A is padded to that same 2-cell budget.
        let header = out
            .lines()
            .find(|l| l.contains('A') && l.contains('B'))
            .expect("header row");
        assert!(
            header.contains(" A  "),
            "header A cell must pad to emoji's 2-cell width: {header:?}"
        );
    }

    #[test]
    fn md_heading_emits_gutter_for_h1() {
        let out = rendered("# Title\n", 80);
        assert!(out.contains('\u{258C}'), "expected H1 gutter: {out}");
        assert!(out.contains("Title"));
    }

    #[test]
    fn md_plain_text_uses_theme_body_style() {
        let out = markdown_to_lines("plain assistant text\n", 80);
        assert_eq!(out[0].spans[0].style, theme::body_style());
    }

    #[test]
    fn md_blockquote_prefixes_each_line() {
        let out = rendered("> quoted text\n", 80);
        assert!(
            out.contains('\u{2502}'),
            "expected blockquote gutter: {out}"
        );
        assert!(out.contains("quoted text"));
    }

    #[test]
    fn md_link_appends_url_inline() {
        let out = rendered("[click](https://example.com)\n", 80);
        assert!(out.contains("click"));
        assert!(out.contains("https://example.com"));
    }

    #[test]
    fn md_strikethrough_passes_text_through() {
        // Style flag isn't visible in plain text join, but the text must
        // still render — proves the parser option is enabled.
        let out = rendered("~~gone~~\n", 80);
        assert!(out.contains("gone"));
    }

    #[test]
    fn md_task_list_renders_checkbox_glyphs() {
        let out = rendered("- [x] done\n- [ ] todo\n", 80);
        assert!(out.contains('\u{2611}'), "expected checked glyph: {out}");
        assert!(out.contains('\u{2610}'), "expected unchecked glyph: {out}");
    }

    #[test]
    fn md_ordered_list_renders_numbers_not_bullets() {
        let out = rendered("1. first\n2. second\n3. third\n", 80);
        assert!(out.contains("1. first"), "expected ordinal 1: {out}");
        assert!(out.contains("2. second"), "expected ordinal 2: {out}");
        assert!(out.contains("3. third"), "expected ordinal 3: {out}");
        assert!(
            !out.contains('\u{2022}'),
            "ordered list must not render bullets: {out}"
        );
    }

    #[test]
    fn md_ordered_list_honors_start_offset() {
        let out = rendered("5. five\n6. six\n", 80);
        assert!(out.contains("5. five"), "expected start at 5: {out}");
        assert!(out.contains("6. six"), "expected continuation 6: {out}");
    }

    #[test]
    fn md_unordered_list_still_renders_bullets() {
        let out = rendered("- one\n- two\n", 80);
        assert!(out.contains('\u{2022}'), "expected bullet glyph: {out}");
    }

    #[test]
    fn md_table_with_no_width_still_emits_lines() {
        // Defensive: zero width must not panic and must not emit infinite
        // padding. The truncation rule collapses every column to `…`.
        let out = markdown_to_lines("| A |\n|---|\n| 1 |\n", 0);
        assert!(!out.is_empty());
    }

    fn att(name: &str) -> PendingAttachment {
        PendingAttachment {
            path: std::path::PathBuf::from(format!("/tmp/{name}")),
            mime_type: "text/plain".to_string(),
            filename: name.to_string(),
            size_bytes: 1,
            source: crate::attachment::AttachmentSource::File,
        }
    }

    #[test]
    fn enqueue_dispatches_immediately_when_idle() {
        let mut s = state();
        s.enqueue_message("hello".to_string(), Vec::new()).unwrap();
        assert_eq!(s.queue_len(), 1);
        let msg = s
            .take_next_dispatchable()
            .expect("idle queue must dispatch");
        assert_eq!(msg.text, "hello");
        assert_eq!(s.queue_len(), 0);
    }

    #[test]
    fn select_queued_by_id_sets_selection() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("a".to_string(), Vec::new()).unwrap();
        s.enqueue_message("b".to_string(), Vec::new()).unwrap();
        let second = s.message_queue[1].id;
        assert!(s.select_queued_by_id(second));
        assert_eq!(s.queue_sel, Some(second));
        // Re-selecting the same id reports no change.
        assert!(!s.select_queued_by_id(second));
        // Unknown id is ignored.
        assert!(!s.select_queued_by_id(9999));
        assert_eq!(s.queue_sel, Some(second));
    }

    #[test]
    fn queue_scroll_by_clamps_at_zero() {
        let mut s = state();
        s.queue_scroll_by(-5);
        assert_eq!(s.queue_scroll, 0);
        s.queue_scroll_by(4);
        assert_eq!(s.queue_scroll, 4);
        s.queue_scroll_by(-10);
        assert_eq!(s.queue_scroll, 0);
    }

    #[test]
    fn no_dispatch_while_turn_in_flight() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("a".to_string(), Vec::new()).unwrap();
        s.enqueue_message("b".to_string(), Vec::new()).unwrap();
        assert!(s.take_next_dispatchable().is_none());
        assert_eq!(s.queue_len(), 2);
    }

    #[test]
    fn fifo_order_preserved() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("first".to_string(), Vec::new()).unwrap();
        s.enqueue_message("second".to_string(), Vec::new()).unwrap();
        s.turn_in_flight = false;
        assert_eq!(s.take_next_dispatchable().unwrap().text, "first");
        assert_eq!(s.take_next_dispatchable().unwrap().text, "second");
    }

    #[test]
    fn injection_jumps_ahead_of_pending() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("pending1".to_string(), Vec::new())
            .unwrap();
        s.enqueue_message("pending2".to_string(), Vec::new())
            .unwrap();
        s.inject_message("urgent".to_string(), Vec::new()).unwrap();
        s.turn_in_flight = false;
        assert_eq!(s.take_next_dispatchable().unwrap().text, "urgent");
        assert_eq!(s.take_next_dispatchable().unwrap().text, "pending1");
    }

    #[test]
    fn cancel_pauses_pending_but_injection_resumes() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("queued".to_string(), Vec::new()).unwrap();
        s.commit_turn(String::new(), false);
        assert!(s.queue_paused());
        assert!(
            s.take_next_dispatchable().is_none(),
            "paused queue must not dispatch pending items"
        );
        s.inject_message("override".to_string(), Vec::new())
            .unwrap();
        assert!(
            !s.queue_paused(),
            "an explicit inject (Ctrl+Enter) resumes the whole queue"
        );
        assert_eq!(
            s.take_next_dispatchable().unwrap().text,
            "override",
            "injected item dispatches first"
        );
        assert_eq!(
            s.take_next_dispatchable().unwrap().text,
            "queued",
            "pending then flows because the inject unpaused the queue"
        );
    }

    #[test]
    fn clean_completion_does_not_pause() {
        let mut s = state();
        s.turn_in_flight = true;
        s.commit_turn(String::new(), true);
        assert!(!s.queue_paused());
    }

    #[test]
    fn empty_enqueue_rejected() {
        let mut s = state();
        assert!(s.enqueue_message("   ".to_string(), Vec::new()).is_err());
        assert!(s.inject_message(String::new(), Vec::new()).is_err());
        assert_eq!(s.queue_len(), 0);
    }

    #[test]
    fn attachment_only_enqueue_accepted() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message(String::new(), vec![att("a.txt")])
            .unwrap();
        assert_eq!(s.queue_len(), 1);
    }

    #[test]
    fn queue_sidebar_open_tracks_contents() {
        let mut s = state();
        s.turn_in_flight = true;
        assert!(!s.queue_sidebar_open(), "empty queue → sidebar closed");
        s.enqueue_message("a".to_string(), Vec::new()).unwrap();
        assert!(s.queue_sidebar_open(), "non-empty queue → sidebar open");
        s.ensure_queue_selection();
        assert!(s.queue_sel.is_some(), "first enqueue seeds a selection");
        s.delete_selected_queued();
        assert!(
            !s.queue_sidebar_open(),
            "draining the queue closes the sidebar"
        );
    }

    #[test]
    fn delete_selected_removes_item() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("a".to_string(), Vec::new()).unwrap();
        s.enqueue_message("b".to_string(), Vec::new()).unwrap();
        s.ensure_queue_selection();
        s.delete_selected_queued();
        assert_eq!(s.queue_len(), 1);
    }

    #[test]
    fn edit_pull_removes_from_queue_and_returns_content() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("draft".to_string(), vec![att("x.txt")])
            .unwrap();
        s.ensure_queue_selection();
        let (text, atts) = s.take_selected_for_edit().expect("selected item");
        assert_eq!(text, "draft");
        assert_eq!(atts.len(), 1);
        assert_eq!(s.queue_len(), 0);
    }

    #[test]
    fn clear_queue_cmd_removes_one_by_index() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("a".to_string(), Vec::new()).unwrap();
        s.enqueue_message("b".to_string(), Vec::new()).unwrap();
        s.enqueue_message("c".to_string(), Vec::new()).unwrap();
        // 1-based: remove the second item ("b").
        s.clear_queue_cmd(Some(2));
        assert_eq!(s.queue_len(), 2);
        s.turn_in_flight = false;
        assert_eq!(s.take_next_dispatchable().unwrap().text, "a");
        assert_eq!(s.take_next_dispatchable().unwrap().text, "c");
    }

    #[test]
    fn clear_queue_cmd_none_clears_all() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("a".to_string(), Vec::new()).unwrap();
        s.enqueue_message("b".to_string(), Vec::new()).unwrap();
        s.clear_queue_cmd(None);
        assert_eq!(s.queue_len(), 0);
    }

    #[test]
    fn clear_queue_cmd_invalid_index_is_a_noop() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("a".to_string(), Vec::new()).unwrap();
        // Out of range and the Some(0) sentinel must not remove anything.
        s.clear_queue_cmd(Some(9));
        s.clear_queue_cmd(Some(0));
        assert_eq!(s.queue_len(), 1);
    }

    #[test]
    fn non_clean_commit_with_empty_queue_does_not_pause() {
        let mut s = state();
        s.turn_in_flight = true;
        s.commit_turn(String::new(), false);
        assert!(
            !s.queue_paused(),
            "cancel/fail with no queued backlog must not show queue-paused state"
        );
    }

    #[test]
    fn resume_queue_unpauses_and_reports_prior_state() {
        let mut s = state();
        s.enqueue_message("queued".to_string(), Vec::new()).unwrap();
        s.commit_turn(String::new(), false);
        assert!(s.queue_paused(), "non-clean turn end must pause");
        assert!(
            s.resume_queue(),
            "resume_queue returns true when it was paused"
        );
        assert!(!s.queue_paused());
        assert!(
            !s.resume_queue(),
            "resume_queue returns false when already running"
        );
    }

    #[test]
    fn resume_then_dispatch_after_auto_pause() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("queued".to_string(), Vec::new()).unwrap();
        // Turn cancelled/failed mid-flight -> auto-pause.
        s.commit_turn(String::new(), false);
        assert!(s.take_next_dispatchable().is_none(), "paused: no dispatch");
        s.resume_queue();
        assert_eq!(s.take_next_dispatchable().unwrap().text, "queued");
    }

    #[test]
    fn enter_during_cancel_pauses_queue() {
        let mut s = state();
        s.turn_in_flight = true;
        s.resume_queue();
        s.enqueue_message("hello".to_string(), Vec::new()).unwrap();
        s.commit_turn(String::new(), false);
        assert!(
            s.queue_paused(),
            "a plain-Enter submission mid-turn must not bypass the cancel auto-pause"
        );
        assert!(
            s.take_next_dispatchable().is_none(),
            "the cancelled turn pauses the queue; the backlog waits for a deliberate resume"
        );
    }

    #[test]
    fn inject_survives_cancel_auto_pause() {
        let mut s = state();
        s.turn_in_flight = true;
        s.inject_message("now".to_string(), Vec::new()).unwrap();
        s.commit_turn(String::new(), false);
        assert_eq!(
            s.take_next_dispatchable().unwrap().text,
            "now",
            "an inject is the only intent that survives a cancel"
        );
    }

    #[test]
    fn inject_resume_override_is_one_shot() {
        let mut s = state();
        s.turn_in_flight = true;
        s.inject_message("a".to_string(), Vec::new()).unwrap();
        s.commit_turn(String::new(), false);
        assert_eq!(s.take_next_dispatchable().unwrap().text, "a");
        s.turn_in_flight = true;
        s.enqueue_message("b".to_string(), Vec::new()).unwrap();
        s.commit_turn(String::new(), false);
        assert!(
            s.queue_paused(),
            "a stale inject override must not leak into the next cancelled turn"
        );
    }

    #[test]
    fn enter_cancelling_arms_watchdog_and_commit_disarms() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enter_cancelling();
        assert!(matches!(s.turn_status, TurnStatus::Cancelling));
        assert!(s.cancel_started_at.is_some());
        s.commit_turn(String::new(), false);
        assert!(matches!(s.turn_status, TurnStatus::Idle));
        assert!(
            s.cancel_started_at.is_none(),
            "commit must disarm the cancel watchdog"
        );
        assert!(!s.cancel_watchdog_expired());
    }

    #[test]
    fn cancel_watchdog_expires_after_bound() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enter_cancelling();
        assert!(!s.cancel_watchdog_expired(), "fresh cancel is not expired");
        s.cancel_started_at = Some(Instant::now() - CANCEL_WATCHDOG);
        assert!(
            s.cancel_watchdog_expired(),
            "a cancel with no TurnComplete past the bound must be reported stuck"
        );
    }

    #[test]
    fn idle_session_never_reports_stuck_cancel() {
        let mut s = state();
        s.cancel_started_at = Some(Instant::now() - CANCEL_WATCHDOG);
        assert!(
            !s.cancel_watchdog_expired(),
            "watchdog only fires while status is Cancelling"
        );
    }

    #[test]
    fn info_notice_set_and_cleared_without_touching_entries() {
        let mut s = state();
        let before = s.entries.len();
        s.set_info_notice("Detached: clipboard_123.png".to_string());
        assert_eq!(
            s.info_message.as_ref().map(|m| m.text.as_str()),
            Some("Detached: clipboard_123.png")
        );
        assert_eq!(
            s.entries.len(),
            before,
            "info notice must not enter history"
        );
        s.clear_info_notice();
        assert!(s.info_message.is_none());
        assert_eq!(s.entries.len(), before);
    }

    #[test]
    fn reset_clears_queue() {
        let mut s = state();
        s.turn_in_flight = true;
        s.enqueue_message("a".to_string(), Vec::new()).unwrap();
        s.queue_paused = true;
        s.copy_hit_regions.push(CopyHitRegion {
            rect: Rect::new(1, 1, 6, 1),
            text: "stale".to_string(),
            kind: CopyHitKind::Message,
            group: 0,
        });
        s.copy_feedback = Some(CopyFeedback {
            target: CopyFeedbackTarget::Overlay(Rect::new(1, 1, 8, 1)),
            shown_at: Instant::now(),
        });
        s.reset_for_session("sess-2".to_string(), None);
        assert_eq!(s.queue_len(), 0);
        assert!(!s.queue_paused());
        assert!(
            s.copy_hit_regions.is_empty(),
            "session reset must clear stale copy hit regions"
        );
        assert!(
            s.copy_feedback.is_none(),
            "session reset must clear stale copy feedback"
        );
    }

    #[test]
    fn toggle_queue_pause_flips_state() {
        let mut s = state();
        assert!(!s.queue_paused());
        assert!(s.toggle_queue_pause());
        assert!(s.queue_paused());
        assert!(!s.toggle_queue_pause());
        assert!(!s.queue_paused());
    }

    #[test]
    fn queue_cap_enforced() {
        let mut s = state();
        s.turn_in_flight = true;
        for i in 0..ChatState::QUEUE_CAP {
            s.enqueue_message(format!("m{i}"), Vec::new()).unwrap();
        }
        assert!(
            s.enqueue_message("overflow".to_string(), Vec::new())
                .is_err()
        );
    }

    #[test]
    fn page_and_jump_scroll_move_the_viewport() {
        let mut s = state();
        s.last_total_rows = 100;
        s.last_inner_height = 10;
        s.scroll_to_bottom();
        let bottom = s.scroll_offset;
        assert_eq!(bottom, 90);
        assert!(s.pinned_to_bottom);

        s.page_up();
        assert_eq!(s.scroll_offset, 80);
        assert!(!s.pinned_to_bottom);

        s.scroll_to_top();
        assert_eq!(s.scroll_offset, 0);
        assert!(!s.pinned_to_bottom);

        s.page_down();
        assert_eq!(s.scroll_offset, 10);

        s.scroll_to_bottom();
        assert_eq!(s.scroll_offset, bottom);
        assert!(s.pinned_to_bottom);
    }

    #[test]
    fn queue_sidebar_resize_clamps_to_bounds() {
        let mut s = state();
        for _ in 0..40 {
            s.widen_queue_sidebar();
        }
        assert_eq!(s.queue_sidebar_cols, ChatState::QUEUE_SIDEBAR_COLS_MAX);
        for _ in 0..40 {
            s.narrow_queue_sidebar();
        }
        assert_eq!(s.queue_sidebar_cols, ChatState::QUEUE_SIDEBAR_COLS_MIN);
    }

    #[test]
    fn queue_sidebar_narrow_then_widen_responds_immediately() {
        let mut s = state();
        s.narrow_queue_sidebar();
        s.narrow_queue_sidebar();
        let narrowed = s.queue_sidebar_width(200);
        s.widen_queue_sidebar();
        assert!(
            s.queue_sidebar_width(200) > narrowed,
            "one widen after narrowing must increase width, not burn a banked deficit"
        );
    }

    #[test]
    fn queue_sidebar_width_respects_absolute_clamps() {
        let s = state();
        let wide = s.queue_sidebar_width(400);
        assert!(
            wide <= ChatState::QUEUE_SIDEBAR_COLS_MAX,
            "sidebar exceeded absolute column cap"
        );
        // Narrow terminal: chat column keeps its minimum, sidebar shrinks.
        let tight = s.queue_sidebar_width(40);
        assert!(
            tight <= 40u16.saturating_sub(ChatState::QUEUE_CHAT_COLS_MIN),
            "sidebar starved the chat column on a narrow terminal"
        );
    }

    #[test]
    fn title_includes_short_session_hash() {
        let s = ChatState::new(
            "40be7731122334455".to_string(),
            "personal_code".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        assert_eq!(s.title(), "personal_code  40be773");
    }

    #[test]
    fn title_with_session_name_keeps_hash() {
        let mut s = ChatState::new(
            "40be7731122334455".to_string(),
            "personal_code".to_string(),
            crate::todo_tracker::TodoTrackerSettings::default(),
        );
        s.session_name = Some("my work".to_string());
        assert_eq!(s.title(), "personal_code  — my work  40be773");
    }

    #[test]
    fn first_message_captures_first_user_message_only() {
        let mut s = state();
        assert!(s.first_message.is_none());
        s.push_user_message(Some("the original ask".to_string()), Vec::new());
        s.push_user_message(Some("a follow up".to_string()), Vec::new());
        assert_eq!(s.first_message.as_deref(), Some("the original ask"));
    }

    #[test]
    fn first_message_ignores_empty_text() {
        let mut s = state();
        s.push_user_message(Some("   ".to_string()), Vec::new());
        assert!(s.first_message.is_none());
        s.push_user_message(Some("real".to_string()), Vec::new());
        assert_eq!(s.first_message.as_deref(), Some("real"));
    }

    #[test]
    fn reset_for_session_clears_first_message() {
        let mut s = state();
        s.push_user_message(Some("ask".to_string()), Vec::new());
        s.reset_for_session("sess-2".to_string(), None);
        assert!(s.first_message.is_none());
    }

    #[test]
    fn load_history_replays_transcript_and_seeds_first_message() {
        use crate::client::MessageEntry;
        let mut s = state();
        s.reset_for_session("sess-resume".to_string(), None);
        let before = s.entries.len();
        s.load_history(vec![
            MessageEntry {
                role: "user".to_string(),
                content: "first ask".to_string(),
            },
            MessageEntry {
                role: "assistant".to_string(),
                content: "reply".to_string(),
            },
            MessageEntry {
                role: "system".to_string(),
                content: "ignored".to_string(),
            },
            MessageEntry {
                role: "user".to_string(),
                content: "second ask".to_string(),
            },
        ]);
        // User + assistant + user replayed; system dropped.
        assert_eq!(s.entries.len(), before + 3);
        // First user message seeds the pinned recovery row.
        assert_eq!(s.first_message.as_deref(), Some("first ask"));
    }

    // ── Elicitation modal ────────────────────────────────────────

    fn single_elicitation() -> PendingElicitation {
        PendingElicitation {
            request_id: serde_json::json!("elicit-1"),
            session_id: "sess-1".to_string(),
            message: "Pick a fruit".to_string(),
            choices: vec![
                "Apple".to_string(),
                "Banana".to_string(),
                "Cherry".to_string(),
            ],
            multi: false,
            min_items: 1,
            max_items: 1,
            cursor: 0,
            selected: Vec::new(),
        }
    }

    fn multi_elicitation() -> PendingElicitation {
        PendingElicitation {
            request_id: serde_json::json!(42),
            session_id: "sess-1".to_string(),
            message: "Pick toppings".to_string(),
            choices: vec![
                "Cheese".to_string(),
                "Olives".to_string(),
                "Ham".to_string(),
            ],
            multi: true,
            min_items: 1,
            max_items: 2,
            cursor: 0,
            selected: vec![false, false, false],
        }
    }

    #[test]
    fn single_select_accept_content_uses_cursor_index() {
        let mut e = single_elicitation();
        e.cursor = 2;
        let content = e.accept_content().expect("single select always valid");
        assert_eq!(content, serde_json::json!({ "choice": "choice-2" }));
    }

    #[test]
    fn single_select_is_always_valid_when_choices_present() {
        let e = single_elicitation();
        assert!(e.selection_valid());
    }

    #[test]
    fn single_select_with_no_choices_is_invalid() {
        let mut e = single_elicitation();
        e.choices.clear();
        assert!(!e.selection_valid());
        assert!(e.accept_content().is_none());
    }

    #[test]
    fn multi_select_requires_min_items() {
        let e = multi_elicitation(); // min 1, nothing selected
        assert!(!e.selection_valid());
        assert!(e.accept_content().is_none());
    }

    #[test]
    fn multi_select_rejects_over_max_items() {
        let mut e = multi_elicitation(); // max 2
        e.selected = vec![true, true, true]; // 3 selected
        assert_eq!(e.selected_count(), 3);
        assert!(!e.selection_valid());
        assert!(e.accept_content().is_none());
    }

    #[test]
    fn multi_select_accept_content_lists_checked_indices() {
        let mut e = multi_elicitation();
        e.selected = vec![true, false, true]; // indices 0 and 2
        assert!(e.selection_valid());
        let content = e.accept_content().expect("2 within 1..=2");
        assert_eq!(
            content,
            serde_json::json!({ "choices": ["choice-0", "choice-2"] })
        );
    }

    #[test]
    fn elicitation_numeric_request_id_is_preserved() {
        let e = multi_elicitation();
        // Numeric ids must round-trip as numbers, not strings, so the
        // daemon can match the response to its outbound request.
        assert_eq!(e.request_id, serde_json::json!(42));
    }

    #[test]
    fn set_and_take_pending_elicitation_round_trip() {
        let mut s = state();
        assert!(s.pending_elicitation().is_none());
        s.set_pending_elicitation(single_elicitation());
        assert!(s.pending_elicitation().is_some());
        let taken = s.take_pending_elicitation().expect("was set");
        assert_eq!(taken.message, "Pick a fruit");
        assert!(s.pending_elicitation().is_none());
    }

    #[test]
    fn reset_for_session_clears_pending_elicitation() {
        let mut s = state();
        s.set_pending_elicitation(single_elicitation());
        s.reset_for_session("sess-2".to_string(), None);
        assert!(
            s.pending_elicitation().is_none(),
            "a session switch must drop any stale elicitation modal"
        );
    }

    // ── Inbound elicitation routing (ask_user intermittent-failure fix) ──

    /// Build an inbound `elicitation/create` request for `session_id` with a
    /// canonical single-select schema (the shape the daemon emits).
    fn inbound_single_elicitation(id: &str, session_id: &str) -> crate::client::RpcInboundRequest {
        crate::client::RpcInboundRequest {
            id: serde_json::json!(id),
            method: "elicitation/create".to_string(),
            params: serde_json::json!({
                "sessionId": session_id,
                "mode": "form",
                "message": "Pick one",
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "choice": {
                            "type": "string",
                            "oneOf": [
                                { "const": "choice-0", "title": "Yes" },
                                { "const": "choice-1", "title": "No" }
                            ]
                        }
                    }
                }
            }),
        }
    }

    fn test_chat() -> (Chat, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel::<String>(16);
        let rpc = Arc::new(RpcOutbound::new(tx));
        let client = Arc::new(RpcClient::with_rpc(rpc));
        (Chat::new(client, PaneKind::Chat), rx)
    }

    #[tokio::test]
    async fn elicitation_matching_active_session_installs_modal() {
        let (mut chat, mut rx) = test_chat();
        chat.phase = ChatPhase::Active(Box::new(state())); // session_id = "sess-1"

        chat.route_inbound_elicitation(inbound_single_elicitation("e1", "sess-1"));

        // Modal installed.
        match &chat.phase {
            ChatPhase::Active(s) => assert!(
                s.pending_elicitation().is_some(),
                "matching-session elicitation must install a modal"
            ),
            _ => panic!("expected Active phase"),
        }
        // Nothing deferred, and no auto-response was written.
        assert!(chat.deferred_elicitations.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "an installed elicitation must not be auto-answered"
        );
    }

    #[tokio::test]
    async fn elicitation_for_other_session_is_deferred_not_cancelled() {
        let (mut chat, mut rx) = test_chat();
        chat.phase = ChatPhase::Active(Box::new(state())); // active = "sess-1"

        chat.route_inbound_elicitation(inbound_single_elicitation("e1", "sess-OTHER"));

        assert_eq!(
            chat.deferred_elicitations.len(),
            1,
            "a non-matching elicitation must be deferred, not cancelled outright"
        );
        // Give the (non-)spawned responder a chance — nothing must be sent yet.
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "a deferred elicitation must not be answered during its grace window"
        );
    }

    #[tokio::test]
    async fn deferred_elicitation_installs_once_session_becomes_active() {
        let (mut chat, _rx) = test_chat();
        // No active session yet (still picking an agent) → defer.
        chat.route_inbound_elicitation(inbound_single_elicitation("e1", "sess-1"));
        assert_eq!(chat.deferred_elicitations.len(), 1);

        // Session comes up.
        chat.phase = ChatPhase::Active(Box::new(state())); // "sess-1"
        chat.drain_deferred_elicitations();

        assert!(
            chat.deferred_elicitations.is_empty(),
            "the deferred elicitation must be consumed once installable"
        );
        match &chat.phase {
            ChatPhase::Active(s) => assert!(s.pending_elicitation().is_some()),
            _ => panic!("expected Active"),
        }
    }

    #[tokio::test]
    async fn expired_deferred_elicitation_is_cancelled() {
        let (mut chat, mut rx) = test_chat();
        chat.phase = ChatPhase::Active(Box::new(state())); // active = "sess-1"

        // Defer an elicitation for a session this pane will never own, with an
        // already-expired arrival time.
        chat.deferred_elicitations.push(DeferredInboundRequest {
            req: inbound_single_elicitation("e1", "sess-GONE"),
            first_seen: Instant::now() - (ELICITATION_ROUTE_GRACE + Duration::from_millis(1)),
        });

        chat.drain_deferred_elicitations();

        assert!(
            chat.deferred_elicitations.is_empty(),
            "an expired deferral must be dropped from the retry buffer"
        );
        // A `{"action":"cancel"}` response must have been written.
        let line = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("expired deferral must emit a cancel response")
            .expect("writer channel open");
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["id"], serde_json::json!("e1"));
        assert_eq!(frame["result"]["action"], "cancel");
    }

    #[tokio::test]
    async fn unparseable_elicitation_is_cancelled_immediately() {
        let (mut chat, mut rx) = test_chat();
        chat.phase = ChatPhase::Active(Box::new(state()));

        let mut req = inbound_single_elicitation("e1", "sess-1");
        // Corrupt the schema so `ElicitationShape::from_schema` returns None.
        req.params["requestedSchema"] = serde_json::json!({ "type": "object" });

        chat.route_inbound_elicitation(req);

        assert!(
            chat.deferred_elicitations.is_empty(),
            "an unparseable elicitation must not be deferred"
        );
        let line = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("unparseable elicitation must emit a cancel response")
            .expect("writer channel open");
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["result"]["action"], "cancel");
    }
