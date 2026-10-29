//! Deterministic spoken-text presentation; never a prompt or personality source.
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};

/// Preserve text and order while removing Markdown presentation and raw UI HTML.
/// Links keep their labels; images keep their alternative text. Tables become
/// rows of separated cells. No model or paraphrase; status labels use the configured UI locale.
#[must_use]
pub fn render(markdown: &str) -> String {
    let mut out = String::new();
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    // One omission pass over source bytes, independent of Markdown's inline /
    // block event boundaries. No HTML content is ever converted into speech.
    let speakable = omit_html(markdown);
    for event in Parser::new_ext(&speakable, options) {
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
    // Entity/escape decoding can expose literal tags; apply the same omission
    // rule to the final text too. The caller still performs its PlainText scan.
    omit_html(&out).lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Omit raw HTML elements and their contents, retaining only text outside them.
/// Unclosed/ambiguous elements conservatively suppress the remaining suffix.
/// This is an exclusion filter, not an HTML-to-prose renderer.
fn omit_html(input: &str) -> String {
    let mut out = String::new();
    let mut open: Vec<String> = Vec::new();
    let mut rest = input;
    loop {
        // Raw-text bodies are never tokenized as nested markup.
        if let Some(tag) = open.last().filter(|tag| is_raw_text(tag)) {
            let closer = format!("</{tag}");
            let Some(at) = rest.to_ascii_lowercase().find(&closer) else { return out; };
            rest = &rest[at..];
        }
        let Some(at) = rest.find('<') else { break; };
        if open.is_empty() { out.push_str(&rest[..at]); }
        rest = &rest[at..];
        if rest.starts_with("<!--") {
            let Some(end) = rest.find("-->") else { return out; };
            rest = &rest[end + 3..];
            if open.is_empty() { out.push(' '); }
            continue;
        }
        // Recognize Markdown autolinks before interpreting attribute quotes.
        if let Some(end) = rest.find('>')
            && Parser::new(&rest[..end + 1]).any(|event| matches!(event,
                Event::Start(Tag::Link { link_type: LinkType::Autolink | LinkType::Email, .. })
            ))
        {
            if open.is_empty() { out.push_str(&rest[..end + 1]); }
            rest = &rest[end + 1..];
            continue;
        }
        let bytes = rest.as_bytes();
        let closing = bytes.get(1) == Some(&b'/');
        let start = if closing { 2 } else { 1 };
        let Some(first) = bytes.get(start) else {
            if open.is_empty() { out.push_str(rest); }
            return out;
        };
        if !first.is_ascii_alphabetic() && !matches!(first, b'!' | b'?') {
            if open.is_empty() { out.push('<'); }
            rest = &rest[1..];
            continue;
        }
        // A quoted '>' belongs to an attribute, never closes the tag.
        let mut quote = None;
        let mut end = None;
        for (index, byte) in bytes.iter().copied().enumerate().skip(start) {
            match (quote, byte) {
                (Some(q), b) if q == b => quote = None,
                (None, b'\'' | b'"') => quote = Some(byte),
                (None, b'>') => { end = Some(index); break; }
                _ => {}
            }
        }
        let Some(end) = end else { return out; };
        let name_end = bytes[start..end].iter()
            .position(|b| b.is_ascii_whitespace() || *b == b'/')
            .map_or(end, |n| start + n);
        let name = rest[start..name_end].to_ascii_lowercase();
        let raw_text = open.last().is_some_and(|tag| is_raw_text(tag));
        if closing {
            // In raw-text elements only their own end tag can end omission.
            if (!raw_text || open.last() == Some(&name))
                && let Some(index) = open.iter().rposition(|tag| tag == &name)
            { open.truncate(index); }
        } else if !raw_text && first.is_ascii_alphabetic() && !name.is_empty() && !matches!(name.as_str(), "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta" | "param" | "source" | "track" | "wbr") {
            if name == "plaintext" { return out; }
            // A slash on a non-void HTML element does not make it self-closing.
            open.push(name);
        }
        rest = &rest[end + 1..];
        if open.is_empty() { out.push(' '); }
    }
    if open.is_empty() { out.push_str(rest); }
    out
}

fn is_raw_text(tag: &str) -> bool {
    matches!(tag, "script" | "style" | "textarea" | "title" | "xmp" | "iframe" | "noembed" | "noframes")
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
    fn html_content_is_omitted_under_the_v1_policy() {
        assert_eq!(render("<div>Do not pay 12.</div>"), "");
        assert_eq!(render("do<br>not"), "do not");
        assert_eq!(render("<div>do</div><div>not</div>"), "");
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
    fn multiline_prose_is_not_spoken() {
        assert_eq!(
            render("<div\nclass='plain'>\nDo not\npay 12.\n</div>\n\nAfter."),
            "After."
        );
    }
}


#[cfg(test)]
mod omission_tests {
    use super::render;
    #[test]
    fn inline_elements_omit_all_contents_and_keep_surrounding_prose() {
        for tag in ["span", "div", "script", "style", "custom-element", "foo_bar", "foo.bar"] {
            assert_eq!(render(&format!("Before <{tag}>never speak this</{tag}> After")), "Before After");
        }
        assert_eq!(render("Before <span><b>hidden</b> hidden</span> After"), "Before After");
    }
    #[test]
    fn mixed_inline_block_content_stays_omitted() {
        for tag in ["script", "style", "span"] {
            assert_eq!(render(&format!("Before <{tag}>\n\n<div>never speak this</div>\n\n</{tag}> After")), "Before After");
        }
        assert_eq!(render("Before <script>const x = '<div>'; </script> After"), "Before After");
        assert_eq!(render("Before <script>const x = '<div'; </script> After"), "Before After");
    }
    #[test]
    fn unclosed_elements_and_comments_omit_the_suffix() {
        for tail in ["<div>hidden", "<script>\n<div>hidden</div>", "<!-- hidden", "<span title='unfinished", "<div/>hidden"] {
            assert_eq!(render(&format!("Before {tail}")), "Before");
        }
    }
    #[test]
    fn quoted_angles_comments_and_decoded_tags_are_not_speech() {
        assert_eq!(render("Before <span title='>'>hidden</span> After"), "Before After");
        assert_eq!(render("Before <!--\nhidden\n--> After"), "Before After");
        assert_eq!(render("Before &lt;span&gt;hidden&lt;/span&gt; After"), "Before After");
        assert_eq!(render("2 < 3 and 4 > 1"), "2 < 3 and 4 > 1");
        assert_eq!(render("Visit <https://example.test> then stop."), "Visit https://example.test then stop.");
        assert_eq!(render("Mail <a@example.test> then stop."), "Mail a@example.test then stop.");
        assert_eq!(render("Visit <https://example.test/a'b> then stop."), "Visit https://example.test/a'b then stop.");
    }
}
