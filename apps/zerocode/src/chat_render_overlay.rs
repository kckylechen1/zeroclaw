//! Session-list overlay and transcript chrome helpers for the chat pane
//! (code-block borders, table grids, highlighted code bodies).

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};

use crate::client::SessionEntry;
use crate::theme;

// ── Session overlay rendering ─────────────────────────────────────

/// Compute the overlay rect for the session list picker.
/// Kept in sync with `render_session_list_overlay` so mouse hit-testing
/// can use the same geometry without storing extra state.
pub(crate) fn session_list_overlay_area(area: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Min(8),
            Constraint::Percentage(20),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(15),
            Constraint::Min(40),
            Constraint::Percentage(15),
        ])
        .split(vert[1])[1]
}

pub(crate) fn render_session_list_overlay(
    f: &mut Frame,
    area: Rect,
    sessions: &[SessionEntry],
    list_state: &ListState,
    title: String,
) {
    let overlay_area = session_list_overlay_area(area);

    f.render_widget(Clear, overlay_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(title, theme::overlay_border_style()))
        .border_style(theme::overlay_border_style())
        .style(theme::fill_style());

    let inner = block.inner(overlay_area);
    f.render_widget(block, overlay_area);

    let items: Vec<ListItem> = sessions
        .iter()
        .map(|s| {
            let name = s.name.as_deref().unwrap_or(&s.session_id);
            let agent = s.agent_alias.as_deref().unwrap_or("?");
            let label = format!("{name}  ({agent}, {} msgs)", s.message_count);
            ListItem::new(Span::styled(label, theme::body_style()))
        })
        .collect();

    let list = List::new(items).highlight_style(theme::list_highlight_style());
    // Copy state to pass as mutable.
    let mut ls = *list_state;
    f.render_stateful_widget(list, inner, &mut ls);
}

pub(crate) fn emit_code_block_body(lines: &mut Vec<Line<'static>>, text: &str, lang: Option<&str>) {
    let body = text.strip_suffix('\n').unwrap_or(text);
    if body.is_empty() {
        return;
    }
    let plain_fg = theme::active().body;
    let highlighted = lang.and_then(|token| crate::diff::highlight_code(body, token, plain_fg));
    match highlighted {
        Some(hl) => {
            for line in hl {
                let mut spans = vec![Span::styled("  ".to_string(), theme::code_block_style())];
                spans.extend(line.spans);
                lines.push(Line::from(spans));
            }
        }
        None => {
            for code_line in body.split('\n') {
                lines.push(Line::from(Span::styled(
                    format!("  {code_line}"),
                    theme::code_block_style(),
                )));
            }
        }
    }
}

/// Builds one full-width code-block border bar: `corner_l`, an optional left
/// label (the language), then dashes wrapping a centered `[Copy]`, then
/// `corner_r`. Header and footer share this so their geometry can never drift.
pub(crate) fn code_block_bar(
    width: u16,
    corner_l: char,
    corner_r: char,
    label: Option<&str>,
) -> Line<'static> {
    let label = label.unwrap_or("");
    let copy_lbl = " [Copy] ";
    let label_len = label.chars().count();
    let copy_len = copy_lbl.chars().count();
    let inner = (width as usize).saturating_sub(2);
    let left_total = inner.saturating_sub(copy_len) / 2;
    let right = inner.saturating_sub(copy_len).saturating_sub(left_total);
    let left_dashes = left_total.saturating_sub(label_len);
    Line::from(vec![
        Span::styled(
            format!("{corner_l}{label}{}", "\u{2500}".repeat(left_dashes)),
            theme::dim_style(),
        ),
        Span::styled(
            copy_lbl.to_string(),
            theme::accent_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{}{corner_r}", "\u{2500}".repeat(right)),
            theme::dim_style(),
        ),
    ])
}

pub(crate) fn render_table(
    rows: Vec<Vec<String>>,
    alignments: Vec<pulldown_cmark::Alignment>,
    width: u16,
) -> Vec<Line<'static>> {
    use pulldown_cmark::Alignment as MdAlign;

    if rows.is_empty() {
        return Vec::new();
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }

    // Normalise: pad short rows so every row has `cols` cells.
    let mut grid: Vec<Vec<String>> = rows;
    for row in &mut grid {
        while row.len() < cols {
            row.push(String::new());
        }
    }

    // Natural width per column = longest cell.
    let mut natural: Vec<usize> = vec![0; cols];
    for row in &grid {
        for (i, cell) in row.iter().enumerate() {
            natural[i] = natural[i].max(crate::display_width::display_width(cell.as_str()));
        }
    }

    // Frame budget: `│` borders (cols+1) + one-cell padding either side
    // of each cell (cols * 2).
    let frame = (cols + 1) + cols * 2;
    let avail = (width as usize).saturating_sub(frame);
    let total_natural: usize = natural.iter().sum();

    let widths: Vec<usize> = if total_natural <= avail || total_natural == 0 {
        natural.clone()
    } else {
        // Scale each column proportionally. Floor at 1 cell so columns
        // don't vanish; the renderer collapses 1–3 cell columns to `…`.
        natural
            .iter()
            .map(|n| ((*n * avail) / total_natural).max(1))
            .collect()
    };

    fn truncate_to(s: &str, budget: usize) -> String {
        if budget == 0 {
            return String::new();
        }
        let full_width = crate::display_width::display_width(s);
        if full_width <= budget {
            return s.to_string();
        }
        // Cell needs truncation but budget is too narrow to convey any
        // content + ellipsis — collapse to a single `…`.
        if budget < 2 {
            return "\u{2026}".to_string();
        }
        let mut acc = String::new();
        let mut used = 0usize;
        // Walk graphemes so presentation sequences (⚠️, 🏔️) stay intact.
        for (_offset, grapheme, w) in crate::display_width::grapheme_widths(s) {
            if used + w + 1 > budget {
                acc.push('\u{2026}');
                return acc;
            }
            acc.push_str(grapheme);
            used += w;
            if used == budget {
                return acc;
            }
        }
        acc
    }

    fn pad_cell(s: &str, budget: usize, align: MdAlign) -> String {
        let w = crate::display_width::display_width(s);
        let slack = budget.saturating_sub(w);
        match align {
            MdAlign::Right => format!("{}{}", " ".repeat(slack), s),
            MdAlign::Center => {
                let left = slack / 2;
                let right = slack - left;
                format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
            }
            MdAlign::None | MdAlign::Left => format!("{}{}", s, " ".repeat(slack)),
        }
    }

    let border = |left: &str, mid: &str, right: &str| -> Line<'static> {
        let mut s = String::from(left);
        for (i, w) in widths.iter().enumerate() {
            s.push_str(&"\u{2500}".repeat(w + 2));
            if i + 1 < widths.len() {
                s.push_str(mid);
            }
        }
        s.push_str(right);
        Line::from(Span::styled(s, theme::dim_style()))
    };

    let render_row = |cells: &[String]| -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled("\u{2502}".to_string(), theme::dim_style()));
        for (i, cell) in cells.iter().enumerate() {
            let budget = widths[i];
            let trimmed = truncate_to(cell, budget);
            let align = alignments.get(i).copied().unwrap_or(MdAlign::None);
            let padded = pad_cell(&trimmed, budget, align);
            spans.push(Span::raw(format!(" {padded} ")));
            spans.push(Span::styled("\u{2502}".to_string(), theme::dim_style()));
        }
        Line::from(spans)
    };

    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(border("\u{250C}", "\u{252C}", "\u{2510}"));
    let mut iter = grid.into_iter();
    if let Some(header) = iter.next() {
        out.push(render_row(&header));
        out.push(border("\u{251C}", "\u{253C}", "\u{2524}"));
    }
    for row in iter {
        out.push(render_row(&row));
    }
    out.push(border("\u{2514}", "\u{2534}", "\u{2518}"));
    out
}
