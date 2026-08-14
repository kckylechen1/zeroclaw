//! Active chat pane rendering. Extracted from chat.rs.

use super::*;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};

// ── Active chat rendering ────────────────────────────────────────

pub(crate) fn carve_todo_area(
    tracker: &crate::todo_tracker::TodoTracker,
    area: Rect,
) -> (Rect, Option<Rect>) {
    if !tracker.wants_space() {
        return (area, None);
    }
    match tracker.location() {
        crate::todo_tracker::TodoLocation::Right => {
            let w = tracker.width().min(area.width / 2);
            let body = Rect::new(area.x, area.y, area.width.saturating_sub(w), area.height);
            let panel = Rect::new(area.x + body.width, area.y, w, area.height);
            (body, Some(panel))
        }
        crate::todo_tracker::TodoLocation::Left => {
            let w = tracker.width().min(area.width / 2);
            let panel = Rect::new(area.x, area.y, w, area.height);
            let body = Rect::new(
                area.x + w,
                area.y,
                area.width.saturating_sub(w),
                area.height,
            );
            (body, Some(panel))
        }
        crate::todo_tracker::TodoLocation::Bottom => {
            // Grow up to the configured cap (+2 rows for the bordered
            // block), but never exceed half the pane height.
            let want = (tracker.total() as u16 + 2).min(tracker.max_height());
            let h = want.min(area.height / 2);
            let body = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(h));
            let panel = Rect::new(area.x, area.y + body.height, area.width, h);
            (body, Some(panel))
        }
    }
}

pub(crate) fn render(f: &mut Frame, state: &mut ChatState, area: Rect, pane_kind: PaneKind) {
    // Carve the TodoWrite tracker's area first (outermost split), so the
    // rest of the pane (queue sidebar, transcript, input) lays out in the
    // remaining body. When the tracker wants no space, `body == area` and
    // the existing layout is untouched.
    let (area, todo_area) = carve_todo_area(&state.todo_tracker, area);
    if let Some(panel) = todo_area {
        state.todo_tracker.render(f, panel);
    }

    let area = if state.queue_sidebar_open() {
        let sidebar_w = state.queue_sidebar_width(area.width);
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(20), Constraint::Length(sidebar_w)])
            .split(area);
        render_queue_sidebar(f, state, cols[1]);
        cols[0]
    } else {
        area
    };

    let show_cursor = state.pending_approval().is_none() && state.pending_elicitation().is_none();
    let turn_status = state.turn_status.clone();
    let turn_started_at = state.turn_started_at;

    let _live_input_tokens: Option<u64> = state.context_input_tokens;

    // Transient info-bar messages (queue/attach notices, model-switch notes)
    // render at the app level via InfoBar from `state.info_message`. The paused
    // queue shows as ghost text in the empty input box below, so the chat pane
    // hands its full area to the input bar here.
    let input_area = area;

    let queue_paused_hint = if state.queue_paused() && state.queue_len() > 0 {
        Some(crate::i18n::t_args(
            "zc-queue-paused-ghost",
            &[("key", &resume_queue_chord_label())],
        ))
    } else {
        None
    };

    let conv_area = state.input_bar.render(
        f,
        input_area,
        state.turn_in_flight,
        show_cursor,
        &turn_status,
        turn_started_at,
        queue_paused_hint.as_deref(),
    );

    // Optional CWD line just above the input bar (bottom of conv_area).
    // Renders `<cwd> - (branch) (hash)`, all left-aligned; the branch and hash
    // segments are appended only when the daemon's git poll has resolved them.
    let actual_conv = if pane_kind == PaneKind::Acp
        && let Some(ref cwd) = state.cwd
    {
        if conv_area.height > 1 {
            let cwd_row = Rect::new(
                conv_area.x,
                conv_area.y + conv_area.height - 1,
                conv_area.width,
                1,
            );
            let mut line = format!(" {cwd}");
            if state.git_branch.is_some() || state.git_hash.is_some() {
                line.push_str(" -");
                if let Some(ref branch) = state.git_branch {
                    line.push_str(&format!(" ({branch})"));
                }
                if let Some(ref hash) = state.git_hash {
                    line.push_str(&format!(" ({hash})"));
                }
            }
            line.push(' ');
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(line, theme::dim_style())))
                    .alignment(Alignment::Left),
                cwd_row,
            );
            Rect::new(
                conv_area.x,
                conv_area.y,
                conv_area.width,
                conv_area.height - 1,
            )
        } else {
            conv_area
        }
    } else {
        conv_area
    };

    render_conversation(f, state, actual_conv);
    state.input_bar.render_autocomplete_popup(f);

    if state.pending_approval().is_some() {
        render_approval_overlay(f, state, area);
    }

    if state.pending_elicitation().is_some() {
        render_elicitation_overlay(f, state, area);
    }

    match &state.session_overlay {
        SessionOverlay::List {
            sessions,
            list_state,
        } => {
            render_session_list_overlay(
                f,
                area,
                sessions,
                list_state,
                crate::i18n::t("zc-chat-session-list-switch-title"),
            );
        }
        SessionOverlay::None => {}
    }

    // Model / model_provider picker overlay (drawn on top of content).
    match &state.model_picker {
        ModelPickerOverlay::Loading => {
            // The "Loading models…" status shows in the info bar; the overlay
            // exists only to block input until the catalog arrives. A modal box
            // with no rows would render nothing, so draw a titled placeholder.
            let title = crate::i18n::t("zc-model-catalog-loading");
            let placeholder = [String::new()];
            crate::widgets::PickerModal::new(&title, &placeholder, usize::MAX).render(f, area);
        }
        ModelPickerOverlay::Model(picker) => {
            crate::widgets::PickerModal::new(
                &crate::i18n::t("zc-model-picker-title"),
                &picker.items,
                picker.cursor,
            )
            .render(f, area);
        }
        ModelPickerOverlay::ConfiguredProviderStage(picker) => {
            crate::widgets::PickerModal::new(
                &crate::i18n::t("zc-model-provider-picker-title"),
                &picker.items,
                picker.cursor,
            )
            .render(f, area);
        }
        ModelPickerOverlay::None => {}
    }

    state.input_bar.render_explorer_overlay(f, area);
}

pub(crate) fn model_picker_overlay_area(
    model_picker: &ModelPickerOverlay,
    area: Rect,
) -> Option<Rect> {
    match model_picker {
        ModelPickerOverlay::Loading => {
            let title = crate::i18n::t("zc-model-catalog-loading");
            let placeholder = [String::new()];
            crate::widgets::PickerModal::area_for(&title, &placeholder, area)
        }
        ModelPickerOverlay::Model(picker) => crate::widgets::PickerModal::area_for(
            &crate::i18n::t("zc-model-picker-title"),
            &picker.items,
            area,
        ),
        ModelPickerOverlay::ConfiguredProviderStage(picker) => {
            crate::widgets::PickerModal::area_for(
                &crate::i18n::t("zc-model-provider-picker-title"),
                &picker.items,
                area,
            )
        }
        ModelPickerOverlay::None => None,
    }
}

pub(crate) fn resume_queue_chord_label() -> String {
    crate::keymap::ChatTabAction::PauseResumeQueue
        .default_chords()
        .first()
        .map(|c| c.display())
        .unwrap_or_else(|| "Alt+P".to_string())
}

/// Queue-management help entries shown whenever the queue sidebar is open —
/// both mid-turn and idle. Keeping this in one place stops the two call sites
/// from drifting apart. Every key label is derived from the keymap registry,
/// never hardcoded, so rebinds stay reflected in help.
pub(crate) fn queue_sidebar_help_entries() -> Vec<crate::widgets::HelpEntry> {
    use crate::keymap::ChatTabAction as A;
    use crate::widgets::HelpEntry as E;
    vec![
        E::key(
            chord_label_pair(A::QueueNavUp, A::QueueNavDown),
            crate::i18n::t("zc-queue-help-nav"),
        ),
        E::key(
            chord_label(A::QueueDelete),
            crate::i18n::t("zc-queue-help-delete"),
        ),
        E::key("/clear-queue", crate::i18n::t("zc-queue-help-clear")),
        E::key(
            chord_label(A::QueueEdit),
            crate::i18n::t("zc-queue-help-edit"),
        ),
        E::key(
            chord_label_pair(A::QueueWiden, A::QueueNarrow),
            crate::i18n::t("zc-queue-help-resize"),
        ),
    ]
}

/// Render an action's primary bound chord as a `&'static str` for help entries.
/// `HelpEntry::key` requires `'static`, and chord display is computed at
/// runtime, so the label is leaked — help is built once per popup open.
pub(crate) fn chord_label(action: crate::keymap::ChatTabAction) -> &'static str {
    let label = action
        .default_chords()
        .first()
        .map(|c| c.display())
        .unwrap_or_default();
    Box::leak(label.into_boxed_str())
}

/// Like `chord_label` but joins two actions' chords as `A/B` (e.g. the up/down
/// or widen/narrow pairs that share one help row).
pub(crate) fn chord_label_pair(
    a: crate::keymap::ChatTabAction,
    b: crate::keymap::ChatTabAction,
) -> &'static str {
    let render = |action: crate::keymap::ChatTabAction| {
        action
            .default_chords()
            .first()
            .map(|c| c.display())
            .unwrap_or_default()
    };
    Box::leak(format!("{}/{}", render(a), render(b)).into_boxed_str())
}

pub(crate) fn render_queue_sidebar(f: &mut Frame, state: &mut ChatState, area: Rect) {
    let title = crate::i18n::t_args(
        "zc-queue-title",
        &[("count", &state.queue_len().to_string())],
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::dim_style())
        .title(Span::styled(format!(" {title} "), theme::title_style()))
        .style(theme::fill_style());
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    state.queue_item_rects.clear();
    state.queue_sidebar_rect = None;
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    state.queue_sidebar_rect = Some(inner);

    // Build the row list, recording which rendered row index owns which queued
    // message id so a click can be mapped back to an item after scrolling.
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row_owner: Vec<Option<u64>> = Vec::new();

    if state.message_queue.is_empty() {
        rows.push(Line::from(Span::styled(
            crate::i18n::t("zc-queue-empty-list"),
            theme::dim_style(),
        )));
        row_owner.push(None);
    } else {
        for (idx, msg) in state.message_queue.iter().enumerate() {
            let selected = state.queue_sel == Some(msg.id);
            let marker = if selected { "▶ " } else { "  " };
            let head_style = if selected {
                theme::title_style()
            } else {
                theme::body_style()
            };
            let preview = first_line_preview(&msg.text, inner.width.saturating_sub(4) as usize);
            let tag = if msg.status == QueueItemStatus::Injected {
                format!(" {}", crate::i18n::t("zc-queue-item-injected"))
            } else {
                String::new()
            };
            rows.push(Line::from(vec![
                Span::styled(format!("{marker}{}.", idx + 1), head_style),
                Span::styled(format!(" {preview}"), head_style),
                Span::styled(tag, theme::dim_style()),
            ]));
            row_owner.push(Some(msg.id));
            for att in &msg.attachments {
                rows.push(Line::from(Span::styled(
                    format!("    📎 {}", att.filename),
                    theme::dim_style(),
                )));
                row_owner.push(Some(msg.id));
            }
        }
    }

    // Clamp the scroll offset to the content that overflows the inner height,
    // then record on-screen rects for the visible item rows.
    let total = rows.len() as u16;
    let max_scroll = total.saturating_sub(inner.height);
    if state.queue_scroll > max_scroll {
        state.queue_scroll = max_scroll;
    }
    let scroll = state.queue_scroll;
    for (i, owner) in row_owner.iter().enumerate() {
        let row_i = i as u16;
        if row_i < scroll {
            continue;
        }
        let screen_y = inner.y + (row_i - scroll);
        if screen_y >= inner.y + inner.height {
            break;
        }
        if let Some(id) = owner {
            state
                .queue_item_rects
                .push((*id, Rect::new(inner.x, screen_y, inner.width, 1)));
        }
    }

    // No soft wrap: a queued message renders on a single line that the pane
    // width hard-truncates. Wrapping made long messages spill onto extra rows
    // and pushed the queue out of alignment; the preview is already clipped to
    // the inner width above, and ratatui truncates anything still too wide.
    let para = Paragraph::new(rows)
        .style(theme::fill_style())
        .scroll((scroll, 0));
    f.render_widget(para, inner);
}

pub(crate) fn first_line_preview(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    let truncated = truncate_utf8(line, max.max(1));
    if truncated.len() < line.len() {
        format!("{truncated}…")
    } else {
        truncated.to_string()
    }
}

/// Extract the file extension from the `"path"` field of a tool's input JSON.
pub(crate) fn file_ext(input: &serde_json::Value) -> Option<&str> {
    let path = input.get("path")?.as_str()?;
    std::path::Path::new(path).extension()?.to_str()
}

/// Return a prefix of `s` no longer than `max_bytes`, guaranteed to end on a
/// valid UTF-8 char boundary. Never panics on multi-byte characters.
pub(crate) fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub(crate) fn render_tool_entry(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    input_json: &str,
    result: Option<&str>,
    is_selected: bool,
) {
    let sel_mod = if is_selected {
        Modifier::REVERSED
    } else {
        Modifier::empty()
    };
    lines.push(Line::from(vec![Span::styled(
        format!("[tool: {name}] "),
        theme::tool_label_style().add_modifier(sel_mod),
    )]));

    let parsed: Option<serde_json::Value> = match name {
        "file_edit" | "file_write" => serde_json::from_str(input_json).ok(),
        _ => None,
    };

    let body_start = lines.len();
    match name {
        "file_edit" => {
            let input = parsed.as_ref();
            let old = input
                .and_then(|v| v.get("old_string"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = input
                .and_then(|v| v.get("new_string"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let path = input.and_then(|v| v.get("path")).and_then(|v| v.as_str());
            let ext = input.and_then(|v| file_ext(v));
            let start_line = path
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|content| {
                    content
                        .find(old)
                        .map(|idx| content[..idx].bytes().filter(|b| *b == b'\n').count() + 1)
                })
                .unwrap_or(1);
            lines.extend(diff::diff_lines(old, new, ext, start_line));
        }
        "file_write" => {
            let input = parsed.as_ref();
            let content = input
                .and_then(|v| v.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ext = input.and_then(|v| file_ext(v));
            lines.extend(diff::write_lines(content, ext));
        }
        _ => {
            let truncated = if input_json.len() > 120 {
                format!("{}…", truncate_utf8(input_json, 120))
            } else {
                input_json.to_string()
            };
            lines.push(Line::from(Span::styled(
                format!("  {truncated}"),
                theme::dim_style().add_modifier(sel_mod),
            )));
        }
    }

    if let Some(res) = result {
        let truncated = if res.len() > 200 {
            format!("{}…", truncate_utf8(res, 200))
        } else {
            res.to_string()
        };
        lines.push(Line::from(Span::styled(
            format!("  → {truncated}"),
            theme::dim_style().add_modifier(sel_mod),
        )));
    }

    // Apply REVERSED to body lines from diff_lines/write_lines too.
    if is_selected {
        for line in &mut lines[body_start..] {
            let spans = std::mem::take(&mut line.spans);
            line.spans = spans
                .into_iter()
                .map(|s| s.patch_style(Style::default().add_modifier(Modifier::REVERSED)))
                .collect();
        }
    }
}

/// Render a single committed entry into `lines`.
/// Extracted so both the incremental-append and full-rebuild paths in
/// `rebuild_lines` share identical rendering logic.
pub(crate) fn render_entry_into(
    entry: &ChatEntry,
    is_selected: bool,
    show_thoughts: bool,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    let sel_mod = if is_selected {
        Modifier::REVERSED
    } else {
        Modifier::empty()
    };
    match entry {
        ChatEntry::UserMessage { text, attachments } => {
            let label_span = Span::styled(
                format!("{} ", crate::i18n::t("zc-chat-label-you")),
                theme::user_label_style().add_modifier(sel_mod),
            );
            let body_style = theme::body_style().add_modifier(sel_mod);
            let mut text_lines: Vec<&str> = match text {
                Some(t) => t.split('\n').collect(),
                None => Vec::new(),
            };
            if text_lines.is_empty() {
                text_lines.push("");
            }
            for (idx, line_text) in text_lines.iter().enumerate() {
                let mut spans = Vec::new();
                if idx == 0 {
                    spans.push(label_span.clone());
                }
                spans.push(Span::styled((*line_text).to_string(), body_style));
                lines.push(Line::from(spans));
            }
            if !attachments.is_empty() {
                let label = attachments
                    .iter()
                    .map(|a| a.as_ref())
                    .collect::<Vec<&str>>()
                    .join(", ");
                lines.push(Line::from(Span::styled(
                    format!(" [{label}]"),
                    theme::warn_style().add_modifier(Modifier::ITALIC | sel_mod),
                )));
            }
        }
        ChatEntry::AgentMessage(text) => {
            lines.push(Line::from(vec![Span::styled(
                format!("{} ", crate::i18n::t("zc-chat-label-agent")),
                theme::agent_label_style().add_modifier(sel_mod),
            )]));
            let md_lines = markdown_to_lines(text.as_ref(), width);
            for mut line in md_lines {
                if is_selected {
                    line = Line::from(
                        line.spans
                            .into_iter()
                            .map(|s| {
                                s.patch_style(Style::default().add_modifier(Modifier::REVERSED))
                            })
                            .collect::<Vec<_>>(),
                    );
                }
                lines.push(line);
            }
        }
        ChatEntry::AgentThought(text) => {
            if show_thoughts {
                lines.push(Line::from(vec![
                    Span::styled("(thinking) ", theme::thought_style().add_modifier(sel_mod)),
                    Span::styled(text.to_string(), theme::dim_style().add_modifier(sel_mod)),
                ]));
            }
        }
        ChatEntry::SystemMessage(text) => {
            for line_text in text.lines() {
                lines.push(Line::from(Span::styled(
                    line_text.to_string(),
                    theme::warn_style().add_modifier(Modifier::ITALIC | sel_mod),
                )));
            }
        }
        ChatEntry::Tool {
            name,
            input_json,
            result,
            ..
        } => {
            render_tool_entry(
                lines,
                name.as_ref(),
                input_json.as_ref(),
                result.as_deref().map(|s| s as &str),
                is_selected,
            );
        }
    }
}

/// Locate the `[Copy]` label within a code-fence bar line. Returns the label's
/// starting column (display cells from line start) and its trimmed width in
/// cells, or `None` if the line has no copy label.
pub(crate) fn label_cells(line: &Line<'static>, copy_lbl: &str) -> Option<(u16, u16)> {
    use unicode_width::UnicodeWidthStr;
    let mut col = 0u16;
    for span in &line.spans {
        let content = span.content.as_ref();
        if content == copy_lbl {
            let lead = copy_lbl.len() - copy_lbl.trim_start().len();
            let trimmed = copy_lbl.trim();
            return Some((col + lead as u16, UnicodeWidthStr::width(trimmed) as u16));
        }
        col += UnicodeWidthStr::width(content) as u16;
    }
    None
}

pub(crate) fn message_copy_label() -> String {
    crate::i18n::t("zc-chat-copy-message")
}

pub(crate) fn message_copied_label() -> String {
    crate::i18n::t("zc-chat-copy-message-copied")
}

/// Recover the fence language token from a code-fence header bar line. The
/// header's first span is `┌─ lang ─────`; the ` code ` fallback label and an
/// empty info string both yield `None` so the rebuilt fence stays unlabelled.
pub(crate) fn header_fence_lang(line: &Line<'static>) -> Option<String> {
    let first = line.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
    let token = first
        .trim_start_matches('\u{250c}')
        .trim_matches('\u{2500}')
        .trim();
    if token.is_empty() || token == "code" {
        None
    } else {
        Some(token.to_string())
    }
}

/// Return the code body for clipboard copy without markdown fences.
/// Users pasting into a terminal expect raw commands, not fenced blocks.
pub(crate) fn fenced_text(_lang: Option<&str>, body: &str) -> String {
    body.to_string()
}

/// Wrapped screen-row count for a single cached line at the given width.
pub(crate) fn wrapped_rows(line: &Line<'static>, width: u16) -> u16 {
    Paragraph::new(vec![borrow_line(line)])
        .wrap(Wrap { trim: false })
        .line_count(width) as u16
}

/// Build a `[Copy]` region if its global wrapped row is on-screen.
pub(crate) fn copy_region(
    global_row: u16,
    col: u16,
    cells: u16,
    scroll: u16,
    body: Rect,
    text: &str,
    group: usize,
) -> Option<CopyHitRegion> {
    if global_row < scroll || global_row >= scroll + body.height {
        return None;
    }
    Some(CopyHitRegion {
        rect: Rect::new(body.x + col, body.y + (global_row - scroll), cells, 1),
        text: text.to_string(),
        kind: CopyHitKind::Code,
        group,
    })
}

pub(crate) fn centered_message_copy_rect(label: &str, anchor: Rect, body: Rect) -> Option<Rect> {
    use unicode_width::UnicodeWidthStr;

    if anchor.height == 0 || body.height == 0 || body.width == 0 {
        return None;
    }
    let cells = UnicodeWidthStr::width(label) as u16;
    if cells == 0 || cells > body.width {
        return None;
    }
    let row = anchor.y;
    if row < body.y || row >= body.y.saturating_add(body.height) {
        return None;
    }

    let x = body.x.saturating_add(body.width.saturating_sub(cells) / 2);
    Some(Rect::new(x, row, cells, 1))
}

pub(crate) fn centered_copy_feedback_rect(label: &str, anchor: Rect) -> Option<Rect> {
    use unicode_width::UnicodeWidthStr;

    let cells = UnicodeWidthStr::width(label) as u16;
    if cells == 0 || anchor.height == 0 {
        return None;
    }
    let center = anchor.x.saturating_add(anchor.width / 2);
    let x = center.saturating_sub(cells / 2);
    Some(Rect::new(x, anchor.y, cells, 1))
}

pub(crate) fn borrow_line<'a>(line: &'a Line<'static>) -> Line<'a> {
    let spans: Vec<Span<'a>> = line
        .spans
        .iter()
        .map(|s| Span::styled(s.content.as_ref(), s.style))
        .collect();
    let mut out = Line::from(spans).style(line.style);
    if let Some(a) = line.alignment {
        out = out.alignment(a);
    }
    out
}

pub(crate) fn render_conversation(f: &mut Frame, state: &mut ChatState, area: Rect) {
    state.refresh_title_hit_rects(area);
    state.expire_copy_feedback();

    // Width must be computed before cache rebuild — table column budgets
    // depend on it, and a width change invalidates cached layouts.
    let inner_width = area.width.saturating_sub(2);

    // ── Rebuild cached lines only when entries changed ────────
    if state.dirty != LinesDirty::Clean || state.cached_render_width != inner_width {
        state.rebuild_lines(inner_width);
    }

    // Determine transient overlays (live streaming / approval) up front from
    // cheap state reads. Transient frames append uncached lines and must use
    // the full-buffer path; idle/scroll frames render only the viewport slice.
    let has_stream_text = !state.streaming_text.is_empty();
    let has_stream_thought = state.show_thoughts && !state.streaming_thought.is_empty();
    let has_approval = state.pending_approval().is_some();
    let transient = has_stream_text || has_stream_thought || has_approval;

    // Reserve a pinned top row inside the panel for the session's first user
    // message — a recovery reminder that stays put across scroll and reload.
    let show_first = state
        .first_message
        .as_deref()
        .is_some_and(|m| !m.is_empty());
    let first_row_h: u16 = if show_first && area.height > 2 { 1 } else { 0 };

    let inner_height = area.height.saturating_sub(2).saturating_sub(first_row_h);

    let block = theme::panel_block(&format!(" {} ", state.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if first_row_h == 1 {
        let first_row = Rect::new(inner.x, inner.y, inner.width, 1);
        let msg = state.first_message.as_deref().unwrap_or_default();
        let line = Line::from(Span::styled(msg.to_string(), theme::dim_style()));
        f.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), first_row);
    }

    // Conversation paragraph fills the inner area below the pinned row.
    let body_area = Rect::new(
        inner.x,
        inner.y + first_row_h,
        inner.width,
        inner.height.saturating_sub(first_row_h),
    );

    // Build the full line buffer (history + transient overlays) only on
    // transient frames; idle/scroll frames never materialize the whole
    // history and instead slice the viewport below.
    let transient_lines: Vec<Line<'static>> = if transient {
        let mut lines: Vec<Line<'static>> = state.cached_lines.clone();
        if has_stream_text {
            lines.push(Line::from(vec![Span::styled(
                format!("{} ", crate::i18n::t("zc-chat-label-agent")),
                theme::agent_label_style(),
            )]));
            lines.extend(markdown_to_lines(&state.streaming_text, inner_width));
        }
        if has_stream_thought {
            lines.push(Line::from(vec![
                Span::styled("(thinking) ", theme::thought_style()),
                Span::styled(state.streaming_thought.clone(), theme::dim_style()),
            ]));
        }
        if has_approval {
            for _ in 0..APPROVAL_OVERLAY_HEIGHT {
                lines.push(Line::default());
            }
        }
        lines
    } else {
        Vec::new()
    };

    let total_rows = if transient {
        Paragraph::new(transient_lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner_width) as u16
    } else {
        state.cached_total_rows
    };
    let max_scroll = total_rows.saturating_sub(inner_height);
    let scroll = if state.pinned_to_bottom {
        max_scroll
    } else {
        state.scroll_offset.min(max_scroll)
    };

    // Non-transient frames (idle, scrolling) render only the viewport slice so
    // per-frame work stays O(visible) instead of O(history). Transient frames
    // (live streaming, approval overlay) append uncached lines and keep the
    // full-buffer path.
    let (render_lines, render_scroll) = if transient {
        (transient_lines, scroll)
    } else {
        state.visible_line_slice(scroll, inner_height)
    };

    let p = Paragraph::new(render_lines)
        .wrap(Wrap { trim: false })
        .scroll((render_scroll, 0));
    f.render_widget(p, body_area);
    capture_transcript_snapshot(f, state, body_area);
    render_transcript_selection(f, state);

    state.last_total_rows = total_rows;
    state.last_inner_height = inner_height;
    state.scroll_offset = scroll;

    // Project each entry's line range into screen coords. Off-viewport
    // ranges get no rect.
    let body_x = body_area.x;
    let body_y = body_area.y;
    let body_w = inner_width;
    let body_h = inner_height;
    state.entry_rects.clear();
    for &(entry_idx, screen_lo, screen_hi, content_width) in &state.cached_screen_ranges {
        let visible_lo = screen_lo.max(scroll);
        let visible_hi = screen_hi.min(scroll + body_h);
        if visible_hi <= visible_lo {
            continue;
        }
        // Width follows the entry's rendered text, not the full panel, so a
        // click in the blank margin beside a short message misses every rect
        // and clears the highlight.
        let rect = Rect::new(
            body_x,
            body_y + (visible_lo - scroll),
            content_width.min(body_w),
            visible_hi - visible_lo,
        );
        state.entry_rects.push((entry_idx, rect));
    }

    let body_rect = Rect::new(body_x, body_y, body_w, body_h);
    state.rebuild_copy_regions(inner_width, scroll, body_rect);
    if state.in_browse_mode() {
        state.rebuild_message_copy_region(body_rect);
    } else {
        render_transcript_copy_overlay(f, state);
    }
    render_copy_feedback(f, state);
    render_message_copy_overlay(f, state, body_rect);
    let mut scrollbar_state = ScrollbarState::new(total_rows as usize)
        .position(scroll as usize)
        .viewport_content_length(inner_height as usize);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None),
        area,
        &mut scrollbar_state,
    );
    // Scrollbar paints in `area.right() - 1`; mirror that.
    if area.height > 2 {
        state.scrollbar_track_rect = Some(Rect::new(
            area.x + area.width.saturating_sub(1),
            area.y + 1,
            1,
            area.height - 2,
        ));
    } else {
        state.scrollbar_track_rect = None;
    }
}

pub(crate) fn capture_transcript_snapshot(f: &mut Frame, state: &mut ChatState, body: Rect) {
    use unicode_width::UnicodeWidthStr;

    let cells = {
        let buffer = f.buffer_mut();
        let mut cells = Vec::with_capacity(usize::from(body.width) * usize::from(body.height));
        for y in body.y..body.y.saturating_add(body.height) {
            let mut column = 0;
            while column < body.width {
                let symbol = buffer[(body.x + column, y)].symbol().to_string();
                let width = (UnicodeWidthStr::width(symbol.as_str()) as u16)
                    .max(1)
                    .min(body.width - column);
                cells.push(TranscriptCell {
                    symbol,
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
        }
        cells
    };
    state.set_transcript_snapshot(TranscriptSnapshot { area: body, cells });
}

pub(crate) fn render_transcript_selection(f: &mut Frame, state: &ChatState) {
    let (Some(snapshot), Some(selection)) =
        (&state.transcript_snapshot, state.transcript_selection)
    else {
        return;
    };
    if !selection.dragged {
        return;
    }

    let buffer = f.buffer_mut();
    for row in 0..snapshot.area.height {
        for column in 0..snapshot.area.width {
            if snapshot.selection_contains(selection, CellPoint { column, row }) {
                buffer[(snapshot.area.x + column, snapshot.area.y + row)]
                    .set_style(theme::selected_bg_style());
            }
        }
    }
}

pub(crate) fn render_transcript_copy_overlay(f: &mut Frame, state: &mut ChatState) {
    state
        .copy_hit_regions
        .retain(|region| region.kind != CopyHitKind::Transcript);

    let Some(snapshot) = &state.transcript_snapshot else {
        return;
    };
    let Some(selection) = state.transcript_selection else {
        return;
    };
    let Some(text) = snapshot.selected_text(selection) else {
        return;
    };
    let Some(anchor) = snapshot.selection_anchor_rect(selection) else {
        return;
    };
    let label = message_copy_label();
    let Some(rect) = centered_message_copy_rect(&label, anchor, snapshot.area) else {
        return;
    };

    state.copy_hit_regions.push(CopyHitRegion {
        rect,
        text,
        kind: CopyHitKind::Transcript,
        group: 0,
    });
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label,
            theme::accent_style().add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        rect,
    );
}

pub(crate) fn render_message_copy_overlay(f: &mut Frame, state: &ChatState, body: Rect) {
    let Some(region) = state.message_copy_region(body) else {
        return;
    };
    f.render_widget(Clear, region.rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            message_copy_label(),
            theme::accent_style().add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        region.rect,
    );
}

pub(crate) fn render_copy_feedback(f: &mut Frame, state: &ChatState) {
    let Some(feedback) = state.copy_feedback else {
        return;
    };

    match feedback.target {
        CopyFeedbackTarget::Code(group) => {
            let label = message_copied_label();
            for region in state
                .copy_hit_regions
                .iter()
                .filter(|region| region.kind == CopyHitKind::Code && region.group == group)
            {
                if let Some(rect) = centered_copy_feedback_rect(&label, region.rect) {
                    render_copied_label(f, &label, rect);
                }
            }
        }
        CopyFeedbackTarget::Overlay(rect) => {
            render_copied_label(f, &message_copied_label(), rect);
        }
    }
}

pub(crate) fn render_copied_label(f: &mut Frame, label: &str, rect: Rect) {
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label.to_string(),
            theme::success_style().add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        rect,
    );
}

pub(crate) fn render_approval_overlay(f: &mut Frame, state: &ChatState, area: Rect) {
    let pa = match state.pending_approval() {
        Some(p) => p,
        None => return,
    };

    // Anchor to the bottom of the given area.
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(APPROVAL_OVERLAY_HEIGHT),
        ])
        .split(area);
    let overlay_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(5),
            Constraint::Min(60),
            Constraint::Percentage(5),
        ])
        .split(vert[1])[1];

    f.render_widget(Clear, overlay_area);

    let is_edit_tool = matches!(pa.tool_name.as_str(), "file_edit" | "file_write");
    let allow = crate::i18n::t("zc-chat-approval-action-allow");
    let always = crate::i18n::t("zc-chat-approval-action-always");
    let reject = crate::i18n::t("zc-chat-approval-action-reject");
    let edit = crate::i18n::t("zc-chat-approval-action-edit");
    let keys = if is_edit_tool {
        format!("Enter={allow}  a={always}  Ctrl+D={reject}  e={edit}")
    } else {
        format!("Enter={allow}  a={always}  Ctrl+D={reject}")
    };

    // For file_edit/file_write, strip the bulk content fields — the diff
    // preview in the conversation already shows old/new content.
    let summary = if is_edit_tool {
        strip_content_fields(&pa.arguments_summary)
    } else {
        pa.arguments_summary.clone()
    };

    let secs = pa.timeout_secs.to_string();
    let title = crate::i18n::t_args(
        "zc-chat-approval-title",
        &[("tool", &pa.tool_name), ("secs", &secs)],
    );
    let text = if summary.is_empty() {
        format!("{title}\n\n  {keys}")
    } else {
        format!("{title}\n\n  {summary}\n\n  {keys}")
    };

    let fill = theme::fill_style();
    let p = Paragraph::new(text)
        .style(fill)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(" Approval Required ", theme::warn_style()))
                .border_style(theme::approval_border_style())
                .style(fill),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(p, overlay_area);
}

pub(crate) fn render_elicitation_overlay(f: &mut Frame, state: &ChatState, area: Rect) {
    let e = match state.pending_elicitation() {
        Some(e) => e,
        None => return,
    };

    // Body lines: message (wrapped by the List items below it is not, so
    // we keep the message in the block title area) + one row per choice +
    // a key-hint footer. Budget: 2 border + 1 message + N choices + 1
    // footer, clamped to the area height.
    let choice_rows = e.choices.len() as u16;
    let desired = choice_rows.saturating_add(5); // borders + msg + footer + pad
    let max_h = area.height.saturating_sub(2).max(3);
    let overlay_h = desired.min(max_h).max(3);

    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(overlay_h)])
        .split(area);
    let overlay_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(5),
            Constraint::Min(60),
            Constraint::Percentage(5),
        ])
        .split(vert[1])[1];

    f.render_widget(Clear, overlay_area);

    let fill = theme::fill_style();
    let title = if e.multi {
        let n = e.selected_count();
        format!(
            " Choose ({n} selected, need {}..={}) ",
            e.min_items, e.max_items
        )
    } else {
        String::from(" Choose one ")
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(title, theme::warn_style()))
        .border_style(theme::approval_border_style())
        .style(fill);
    let inner = block.inner(overlay_area);
    f.render_widget(block, overlay_area);

    // Split inner: message line(s), choice list, footer hint.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let msg = Paragraph::new(e.message.clone())
        .style(fill)
        .wrap(Wrap { trim: true });
    f.render_widget(msg, chunks[0]);

    let items: Vec<ListItem> = e
        .choices
        .iter()
        .enumerate()
        .map(|(i, title)| {
            let checkbox = if e.multi {
                if e.selected.get(i).copied().unwrap_or(false) {
                    "[x] "
                } else {
                    "[ ] "
                }
            } else {
                ""
            };
            let line = format!("{checkbox}{title}");
            let style = if i == e.cursor {
                theme::selected_style()
            } else {
                fill
            };
            ListItem::new(Line::from(Span::styled(line, style)))
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(e.cursor.min(e.choices.len().saturating_sub(1))));
    let list = List::new(items).style(fill);
    f.render_stateful_widget(list, chunks[1], &mut list_state);

    let hint = if e.multi {
        "↑/↓ move  Space toggle  Enter confirm  Esc cancel"
    } else {
        "↑/↓ move  Enter confirm  Esc cancel"
    };
    let footer = Paragraph::new(Span::styled(hint, theme::dim_style())).style(fill);
    f.render_widget(footer, chunks[2]);
}

/// compact when a diff preview is already shown in the conversation.
pub(crate) fn strip_content_fields(summary: &str) -> String {
    let mut s = summary;
    for key in &["old_string", "new_string", "content"] {
        // Key appears mid-string as ", key: …"
        if let Some(i) = s.find(&format!(", {key}:")) {
            s = &s[..i];
        } else if s.starts_with(&format!("{key}:")) {
            s = "";
        }
    }
    s.trim_end_matches([',', ' ']).to_string()
}
