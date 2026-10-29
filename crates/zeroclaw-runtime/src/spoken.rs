//! Deterministic spoken-text presentation; never a prompt or personality source.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// Preserve text and order while removing Markdown presentation and raw UI HTML.
/// Links keep their labels; images keep their alternative text. Tables become
/// rows of separated cells. No model or paraphrase; status labels use the configured UI locale.
#[must_use]
pub fn render(markdown: &str) -> String {
    // Deliberately conservative, binary policy over original bytes only.
    if markdown.as_bytes().windows(2).any(|pair| {
        pair[0] == b'<' && (pair[1].is_ascii_alphabetic() || matches!(pair[1], b'/' | b'!'))
    }) {
        return crate::i18n::get_required_cli_string("voice-html-omitted");
    }
    let mut out = String::new();
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Text(text) | Event::Code(text) => out.push_str(&text),
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
        for input in ["", " \n\t"] {
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
mod binary_policy_tests {
    use super::render;

    #[test]
    fn original_markup_always_replaces_the_entire_reply() {
        let placeholder = crate::i18n::get_required_cli_string("voice-html-omitted");
        assert!(!placeholder.is_empty());
        for input in [
            "<div>Do not pay 12.</div>",
            "do<br>not",
            "<div>do</div><div>not</div>",
            "<!-- UI -->\n",
            "Before.\n\n<script>\nnever speak this\n</script>\n\nAfter.",
            "<style>\nbody { color: red; }\n</style>\n\nVisible.",
            "<!--\nprivate comment\n-->\n\nVisible.",
            "<div\nclass='plain'>\nDo not\npay 12.\n</div>\n\nAfter.",
            "Before <script>\n\n<div>never speak this</div>\n\n</script> After",
            "Before <span title='<https://example.test>'>hidden</span> After",
            "Before <span title='<a@example.test>'>hidden</span> After",
            "Before <span title='>'>hidden</span> After",
            "Before <span><b>hidden</b> hidden</span> After",
            "Before <script>const x = '<div'; </script> After",
            "Before <script>const x = '<div>'; </script> After",
            "<div>hidden",
            "<script>\nunclosed script",
            "<!-- hidden",
            "<span title='unfinished",
            "<div/>hidden",
            "</",
            "<!",
            "<A",
            "Visit <https://example.test> then stop.",
            "Mail <a@example.test> then stop.",
            "Visit <https://example.test/a'b> then stop.",
            "`<div>`",
            "\\<div>",
        ] {
            assert_eq!(render(input), placeholder, "input: {input:?}");
        }
        for tag in [
            "span",
            "div",
            "script",
            "style",
            "custom-element",
            "foo_bar",
            "foo.bar",
        ] {
            for body in ["hidden", "\n\n<div>hidden</div>\n\n"] {
                assert_eq!(
                    render(&format!("Before <{tag}>{body}</{tag}> After")),
                    placeholder
                );
            }
        }
    }

    #[test]
    fn without_original_marker_uses_markdown_only() {
        for (input, expected) in [
            ("a < b", "a < b"),
            ("2 < 3 and 4 > 1", "2 < 3 and 4 > 1"),
            ("<", "<"),
            ("你好 **朋友**", "你好 朋友"),
            ("&lt;span&gt;hidden&lt;/span&gt;", "<span>hidden</span>"),
        ] {
            assert_eq!(render(input), expected);
        }
    }
}
