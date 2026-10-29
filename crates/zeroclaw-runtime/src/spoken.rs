//! Deterministic spoken-text presentation; never a prompt or personality source.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// Preserve text and order while removing Markdown presentation and raw UI HTML.
/// Links keep their labels; images keep their alternative text. Tables become
/// rows of separated cells. No model or paraphrase; status labels use the configured UI locale.
#[must_use]
pub fn render(markdown: &str) -> String {
    let mut out = String::new();
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut hidden_inline: Option<String> = None;
    let mut html_block: Option<String> = None;
    for event in Parser::new_ext(markdown, options) {
        // Raw HTML arrives as line fragments. Keep extractor state by converting
        // the entire parser-delimited block, including comments and attributes.
        match &event {
            Event::Start(Tag::HtmlBlock) => {
                html_block = Some(String::new());
                continue;
            }
            Event::Html(html) if html_block.is_some() => {
                if let Some(block) = &mut html_block {
                    block.push_str(html);
                }
                continue;
            }
            Event::End(TagEnd::HtmlBlock) => {
                if let Some(block) = html_block.take() {
                    out.push('\n');
                    out.push_str(&html_text(&block));
                    out.push('\n');
                }
                continue;
            }
            _ => {}
        }
        if let Event::InlineHtml(html) = &event {
            let tag = html.trim().trim_start_matches('<');
            let closing = tag.starts_with('/');
            let name = tag
                .trim_start_matches('/')
                .split(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if closing && hidden_inline.as_deref() == Some(name.as_str()) {
                hidden_inline = None;
                continue;
            }
            if !closing && matches!(name.as_str(), "script" | "style" | "head") {
                hidden_inline = Some(name);
                continue;
            }
        }
        if hidden_inline.is_some() {
            continue;
        }
        match event {
            Event::Text(text) | Event::Code(text) => out.push_str(&text),
            Event::Html(html) => out.push_str(&html_text(&html)),
            Event::InlineHtml(html) => {
                let tag = html.trim().to_ascii_lowercase();
                if matches!(
                    tag.as_str(),
                    "<br>" | "<br/>" | "<br />" | "<hr>" | "<hr/>" | "<hr />"
                ) {
                    out.push('\n');
                } else {
                    out.push_str(&html_text(&html));
                }
            }
            Event::TaskListMarker(checked) => {
                out.push_str(&crate::i18n::get_required_cli_string(if checked {
                    "spoken-task-complete"
                } else {
                    "spoken-task-pending"
                }));
                out.push(' ');
            }
            Event::Start(Tag::Strikethrough) => {
                out.push_str(&crate::i18n::get_required_cli_string("spoken-deleted"));
                out.push(' ');
            }
            Event::End(TagEnd::Strikethrough) => out.push_str("; "),
            Event::SoftBreak | Event::HardBreak | Event::Rule => out.push('\n'),
            Event::End(TagEnd::TableCell) => out.push_str("; "),
            Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::CodeBlock
                | TagEnd::Item
                | TagEnd::TableHead
                | TagEnd::TableRow,
            ) => out.push('\n'),
            // Remaining tags carry presentation, not text.
            _ => {}
        }
    }
    out.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// nanohtml2text strips script/style content and decodes entities. Supply word
// boundaries for structural tags it otherwise treats as generic inline tags.
fn html_text(html: &str) -> String {
    let mut spaced = String::new();
    for fragment in html.split_inclusive('>') {
        let block = fragment.rsplit_once('<').is_some_and(|(_, tag)| {
            let name = tag
                .trim_start_matches('/')
                .split(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
                .next()
                .unwrap_or_default();
            matches!(
                name.to_ascii_lowercase().as_str(),
                "div"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
                    | "table"
                    | "tr"
                    | "td"
                    | "th"
                    | "blockquote"
            )
        });
        if block {
            spaced.push(' ');
        }
        spaced.push_str(fragment);
        if block {
            spaced.push(' ');
        }
    }
    nanohtml2text::html2text(&spaced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoken_preserves_words_numbers_negation_and_table_cells() {
        let input = "# Result\n\n**Do not** pay `12`. Read [details](https://example.test).\n\n| Item | Count |\n|---|---|\n| 苹果 | 0 |\n\n- One\n- Two\n";
        assert_eq!(
            render(input),
            "Result\nDo not pay 12. Read details.\nItem; Count;\n苹果; 0;\nOne\nTwo"
        );
    }

    #[test]
    fn spoken_handles_empty_unicode_code_and_ui_markup() {
        for input in ["", " \n\t", "<!-- UI -->\n"] {
            assert_eq!(render(input), "");
        }
        assert_eq!(
            render("你好，**朋友**。\n\n```text\n0 != 1\n```"),
            "你好，朋友。\n0 != 1"
        );
        assert_eq!(
            render("an_unmatched_name and 2 * 3"),
            "an_unmatched_name and 2 * 3"
        );
        assert_eq!(render("![a quiet lake](lake.png)"), "a quiet lake");
    }
}

#[cfg(test)]
mod preservation_tests {
    use super::*;
    #[test]
    fn html_prose_and_breaks_survive_without_script_or_style() {
        assert_eq!(render("<div>Do not pay 12.</div>"), "Do not pay 12.");
        assert_eq!(render("do<br>not"), "do\nnot");
        assert_eq!(render("<div>do</div><div>not</div>"), "do not");
        assert_eq!(render("<script>never speak this</script>\n"), "");
        assert_eq!(render("<style>never speak this</style>\n"), "");
        assert_eq!(
            render("before <script>never speak this</script> after"),
            "before after"
        );
    }
    #[test]
    fn task_and_strike_status_are_not_silently_erased() {
        let complete = crate::i18n::get_required_cli_string("spoken-task-complete");
        let pending = crate::i18n::get_required_cli_string("spoken-task-pending");
        let deleted = crate::i18n::get_required_cli_string("spoken-deleted");
        assert_ne!(complete, pending);
        assert_eq!(
            render("- [x] Paid\n- [ ] Paid"),
            format!("{complete} Paid\n{pending} Paid")
        );
        assert_eq!(
            render("~~pay 12~~ pay 0"),
            format!("{deleted} pay 12; pay 0")
        );
    }
}

#[cfg(test)]
mod block_tests {
    use super::render;
    #[test]
    fn multiline_script_block_is_not_spoken() {
        assert_eq!(
            render("Before.\n\n<script>\nnever speak this\n</script>\n\nAfter."),
            "Before.\nAfter."
        );
        assert_eq!(render("<script>\nunclosed script"), "");
    }
    #[test]
    fn multiline_style_block_is_not_spoken() {
        assert_eq!(
            render("<style>\nbody { color: red; }\n</style>\n\nVisible."),
            "Visible."
        );
    }
    #[test]
    fn multiline_comment_block_is_not_spoken() {
        assert_eq!(render("<!--\nprivate comment\n-->\n\nVisible."), "Visible.");
    }
    #[test]
    fn multiline_prose_block_preserves_boundaries_and_split_attributes() {
        assert_eq!(
            render("<div\nclass='plain'>\nDo not\npay 12.\n</div>\n\nAfter."),
            "Do not pay 12.\nAfter."
        );
    }
}
