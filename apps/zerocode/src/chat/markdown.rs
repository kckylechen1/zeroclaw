//! Markdown → ratatui lines. Extracted from chat.rs.

use pulldown_cmark::{Event as MdEvent, Options as MdOptions, Parser as MdParser, Tag, TagEnd};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::chat_render_overlay::{code_block_bar, emit_code_block_body, render_table};
use crate::theme;

pub(crate) fn markdown_to_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    use pulldown_cmark::{Alignment as MdAlign, HeadingLevel};

    let mut opts = MdOptions::empty();
    opts.insert(MdOptions::ENABLE_TABLES);
    opts.insert(MdOptions::ENABLE_STRIKETHROUGH);
    opts.insert(MdOptions::ENABLE_TASKLISTS);
    let parser = MdParser::new_ext(text, opts);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut in_bold = false;
    let mut in_italic = false;
    let mut in_strike = false;
    let mut in_code_block = false;
    let mut code_block_text: String = String::new();
    let mut code_block_lang: Option<String> = None;
    let mut heading_level: Option<HeadingLevel> = None;
    let mut blockquote_depth: u32 = 0;
    let mut link_url: Option<String> = None;

    // Stack of enclosing lists. `Some(next)` is an ordered list whose next item
    // renders `next.` and then increments; `None` is a bullet list. The stack
    // depth drives per-level indentation so nested lists step inward.
    let mut list_stack: Vec<Option<u64>> = Vec::new();

    // Table state. While non-`None`, text/inline events accumulate into the
    // current cell instead of the live `current_spans` line.
    struct TableBuf {
        alignments: Vec<MdAlign>,
        rows: Vec<Vec<String>>,
        in_header: bool,
        current_row: Vec<String>,
        current_cell: Option<String>,
    }
    let mut table: Option<TableBuf> = None;

    let push_line = |lines: &mut Vec<Line<'static>>, spans: &mut Vec<Span<'static>>| {
        if !spans.is_empty() {
            lines.push(Line::from(std::mem::take(spans)));
        }
    };

    let blockquote_gutter = |depth: u32| -> Vec<Span<'static>> {
        (0..depth)
            .map(|_| Span::styled("\u{2502} ", theme::dim_style()))
            .collect()
    };

    for event in parser {
        // While inside a table cell, route inline events into the cell
        // buffer. The table only lays out at TagEnd::Table.
        if let Some(t) = table.as_mut()
            && let Some(cell) = t.current_cell.as_mut()
        {
            match &event {
                MdEvent::Text(s) | MdEvent::Code(s) => {
                    cell.push_str(s);
                    continue;
                }
                MdEvent::SoftBreak | MdEvent::HardBreak => {
                    cell.push(' ');
                    continue;
                }
                _ => {}
            }
        }

        match event {
            MdEvent::Start(Tag::Strong) => in_bold = true,
            MdEvent::End(TagEnd::Strong) => in_bold = false,
            MdEvent::Start(Tag::Emphasis) => in_italic = true,
            MdEvent::End(TagEnd::Emphasis) => in_italic = false,
            MdEvent::Start(Tag::Strikethrough) => in_strike = true,
            MdEvent::End(TagEnd::Strikethrough) => in_strike = false,
            MdEvent::Start(Tag::Heading { level, .. }) => {
                push_line(&mut lines, &mut current_spans);
                lines.push(Line::default());
                heading_level = Some(level);
                if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                    current_spans.push(Span::styled("\u{258C} ", theme::accent_style()));
                }
            }
            MdEvent::End(TagEnd::Heading(_)) => {
                push_line(&mut lines, &mut current_spans);
                lines.push(Line::default());
                heading_level = None;
            }
            MdEvent::Start(Tag::BlockQuote(_)) => {
                push_line(&mut lines, &mut current_spans);
                blockquote_depth += 1;
            }
            MdEvent::End(TagEnd::BlockQuote(_)) => {
                push_line(&mut lines, &mut current_spans);
                blockquote_depth = blockquote_depth.saturating_sub(1);
            }
            MdEvent::Start(Tag::Link { dest_url, .. }) => {
                link_url = Some(dest_url.to_string());
            }
            MdEvent::End(TagEnd::Link) => {
                if let Some(url) = link_url.take() {
                    current_spans.push(Span::styled(
                        format!(" ({url})"),
                        theme::dim_style().add_modifier(Modifier::ITALIC),
                    ));
                }
            }
            MdEvent::Start(Tag::CodeBlock(kind)) => {
                push_line(&mut lines, &mut current_spans);
                in_code_block = true;
                code_block_text.clear();
                code_block_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    pulldown_cmark::CodeBlockKind::Indented => None,
                };

                // Header bar: ┌─ lang ──── [Copy] ────┐
                let lang_display = code_block_lang.clone().unwrap_or_default();
                let label = if lang_display.is_empty() {
                    " code ".to_string()
                } else {
                    format!(" {} ", lang_display.as_str())
                };
                lines.push(code_block_bar(
                    width,
                    '\u{250c}',
                    '\u{2510}',
                    Some(&format!("\u{2500}{label}")),
                ));
            }
            MdEvent::End(TagEnd::CodeBlock) => {
                push_line(&mut lines, &mut current_spans);
                in_code_block = false;

                emit_code_block_body(&mut lines, &code_block_text, code_block_lang.as_deref());

                // Footer bar: └──── [Copy] ────┘
                lines.push(code_block_bar(width, '\u{2514}', '\u{2518}', None));

                // Accumulated code text is ready for clipboard copy;
                // the Copy action is handled by the chat pane.
                code_block_text.clear();
                code_block_lang = None;
            }
            MdEvent::Start(Tag::List(start)) => {
                push_line(&mut lines, &mut current_spans);
                list_stack.push(start);
            }
            MdEvent::End(TagEnd::List(_)) => {
                push_line(&mut lines, &mut current_spans);
                list_stack.pop();
            }
            MdEvent::Start(Tag::Item) => {
                push_line(&mut lines, &mut current_spans);
                current_spans.extend(blockquote_gutter(blockquote_depth));
                let depth = list_stack.len().saturating_sub(1);
                current_spans.push(Span::styled("  ".repeat(depth + 1), theme::dim_style()));
                let marker = match list_stack.last_mut() {
                    Some(Some(next)) => {
                        let label = format!("{next}. ");
                        *next += 1;
                        label
                    }
                    _ => "\u{2022} ".to_string(),
                };
                current_spans.push(Span::styled(marker, theme::dim_style()));
            }
            MdEvent::End(TagEnd::Item) if !current_spans.is_empty() => {
                push_line(&mut lines, &mut current_spans);
            }
            MdEvent::Start(Tag::Paragraph) if blockquote_depth > 0 && current_spans.is_empty() => {
                current_spans.extend(blockquote_gutter(blockquote_depth));
            }
            MdEvent::Start(Tag::Paragraph) => {}
            MdEvent::End(TagEnd::Paragraph) if !current_spans.is_empty() => {
                push_line(&mut lines, &mut current_spans);
            }
            MdEvent::TaskListMarker(checked) => {
                let glyph = if checked { "\u{2611} " } else { "\u{2610} " };
                current_spans.push(Span::styled(glyph, theme::accent_style()));
            }
            // ── Tables ──────────────────────────────────────────
            MdEvent::Start(Tag::Table(alignments)) => {
                push_line(&mut lines, &mut current_spans);
                table = Some(TableBuf {
                    alignments,
                    rows: Vec::new(),
                    in_header: false,
                    current_row: Vec::new(),
                    current_cell: None,
                });
            }
            MdEvent::Start(Tag::TableHead) => {
                if let Some(t) = table.as_mut() {
                    t.in_header = true;
                    t.current_row.clear();
                }
            }
            MdEvent::End(TagEnd::TableHead) => {
                if let Some(t) = table.as_mut() {
                    let row = std::mem::take(&mut t.current_row);
                    t.rows.push(row);
                    t.in_header = false;
                }
            }
            MdEvent::Start(Tag::TableRow) => {
                if let Some(t) = table.as_mut() {
                    t.current_row.clear();
                }
            }
            MdEvent::End(TagEnd::TableRow) => {
                if let Some(t) = table.as_mut() {
                    let row = std::mem::take(&mut t.current_row);
                    t.rows.push(row);
                }
            }
            MdEvent::Start(Tag::TableCell) => {
                if let Some(t) = table.as_mut() {
                    t.current_cell = Some(String::new());
                }
            }
            MdEvent::End(TagEnd::TableCell) => {
                if let Some(t) = table.as_mut()
                    && let Some(cell) = t.current_cell.take()
                {
                    t.current_row.push(cell);
                }
            }
            MdEvent::End(TagEnd::Table) => {
                if let Some(t) = table.take() {
                    lines.extend(render_table(t.rows, t.alignments, width));
                }
            }
            MdEvent::Text(t) => {
                let owned = t.to_string();
                if in_code_block {
                    code_block_text.push_str(&owned);
                } else {
                    let mut style = theme::body_style();
                    if let Some(level) = heading_level {
                        style = match level {
                            HeadingLevel::H1 | HeadingLevel::H2 => {
                                theme::heading_style().add_modifier(Modifier::BOLD)
                            }
                            _ => theme::heading_style(),
                        };
                    }
                    if in_bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if in_italic {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if in_strike {
                        style = style.add_modifier(Modifier::CROSSED_OUT);
                    }
                    if link_url.is_some() {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    current_spans.push(Span::styled(owned, style));
                }
            }
            MdEvent::Code(t) => {
                current_spans.push(Span::styled(t.to_string(), theme::code_inline_style()));
            }
            MdEvent::SoftBreak => {
                current_spans.push(Span::raw(" "));
            }
            MdEvent::HardBreak => {
                push_line(&mut lines, &mut current_spans);
                if blockquote_depth > 0 {
                    current_spans.extend(blockquote_gutter(blockquote_depth));
                }
            }
            _ => {}
        }
    }

    if !current_spans.is_empty() {
        lines.push(Line::from(current_spans));
    }

    // Fallback: if parsing produced nothing, return raw text.
    if lines.is_empty() && !text.is_empty() {
        lines.push(Line::from(Span::styled(
            text.to_string(),
            theme::body_style(),
        )));
    }

    lines
}
