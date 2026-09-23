#[cfg(test)]
use super::*;

/// Canonical 1x1 PNG payload: 68 characters, a multiple of four, standard
/// alphabet, no padding. Every accept case below uses it.
const CANONICAL_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGMAAQAABQAB";

const TEN_MB: usize = 10 * 1024 * 1024;

// Every test in this block fails to compile before the change: the
// splitter and its rejection enum did not exist.

#[test]
fn split_data_uri_accepts_canonical_payload() {
    let uri = format!("data:image/png;base64,{CANONICAL_PNG_B64}");
    let (media_type, payload) =
        split_base64_image_data_uri(&uri, TEN_MB).expect("canonical PNG data URI accepted");
    assert_eq!(media_type, "image/png");
    assert_eq!(payload, CANONICAL_PNG_B64);
}

#[test]
fn split_data_uri_accepts_uppercase_media_type_and_extra_parameters() {
    // The allowlist comparison is case-insensitive, and the header may
    // carry parameters before `;base64`.
    let uri = format!("data:IMAGE/PNG;charset=binary;base64,{CANONICAL_PNG_B64}");
    let (media_type, payload) =
        split_base64_image_data_uri(&uri, TEN_MB).expect("upper-case media type accepted");
    // Returned verbatim — the caller lowercases it once when it builds a
    // wire block.
    assert_eq!(media_type, "IMAGE/PNG");
    assert_eq!(payload, CANONICAL_PNG_B64);
}

#[test]
fn split_data_uri_accepts_every_allowlisted_media_type() {
    for mime in ALLOWED_IMAGE_MIME_TYPES {
        let uri = format!("data:{mime};base64,{CANONICAL_PNG_B64}");
        let (media_type, _) = split_base64_image_data_uri(&uri, TEN_MB)
            .unwrap_or_else(|reason| panic!("{mime} rejected: {reason}"));
        assert_eq!(media_type, *mime);
    }
}

#[test]
fn split_data_uri_accepts_well_formed_padding() {
    // `AA==` has its final-quartet padding bits clear; so does `AAA=`.
    let two_pad = split_base64_image_data_uri("data:image/png;base64,AA==", TEN_MB);
    assert_eq!(two_pad.map(|(_, payload)| payload), Ok("AA=="));
    let one_pad = split_base64_image_data_uri("data:image/png;base64,AAA=", TEN_MB);
    assert_eq!(one_pad.map(|(_, payload)| payload), Ok("AAA="));
}

#[test]
fn split_data_uri_rejects_non_data_uris() {
    for candidate in [
        "/tmp/screenshot.png",
        r"C:\Users\leo\shot.png",
        "http://example.com/a.png",
        "https://example.com/a.png",
        // A `data:` prefix with no comma has no payload to split.
        "data:image/png;base64",
    ] {
        assert_eq!(
            split_base64_image_data_uri(candidate, TEN_MB),
            Err(ImageDataUriRejection::NotADataUri),
            "expected {candidate} to be rejected as a non-data URI"
        );
    }
}

#[test]
fn split_data_uri_rejects_missing_base64_declaration() {
    // Matched case-sensitively, as `normalize_data_uri` already does.
    assert_eq!(
        split_base64_image_data_uri("data:image/png,AAAA", TEN_MB),
        Err(ImageDataUriRejection::NotBase64Encoded)
    );
    assert_eq!(
        split_base64_image_data_uri("data:image/png;BASE64,AAAA", TEN_MB),
        Err(ImageDataUriRejection::NotBase64Encoded)
    );
}

#[test]
fn split_data_uri_rejects_media_types_outside_the_allowlist() {
    for mime in ["image/svg+xml", "image/bmp", "application/pdf", ""] {
        let uri = format!("data:{mime};base64,{CANONICAL_PNG_B64}");
        assert_eq!(
            split_base64_image_data_uri(&uri, TEN_MB),
            Err(ImageDataUriRejection::UnsupportedMediaType),
            "expected {mime} to be rejected"
        );
    }
}

#[test]
fn split_data_uri_rejects_malformed_base64() {
    for payload in [
        // Empty payload.
        "",
        // Not a multiple of four. Preparation always emits canonical
        // padded base64, so a payload this shape cannot be a real image
        // and Anthropic's decoder would reject it.
        "iVBORw0KGgo",
        "/9j/4AAQSkZJRgABAQEAYABgAAD",
        // Characters outside the standard alphabet.
        "AAA-",
        "AA=A",
        // More than two padding characters.
        "AB==CD==",
        // Final-quartet padding bits set: both fail a strict decoder even
        // though the length and alphabet are fine.
        "AB==",
        "AAB=",
    ] {
        let uri = format!("data:image/png;base64,{payload}");
        assert_eq!(
            split_base64_image_data_uri(&uri, TEN_MB),
            Err(ImageDataUriRejection::MalformedBase64),
            "expected payload {payload:?} to be rejected"
        );
    }
}

#[test]
fn split_data_uri_rejects_payloads_over_the_ceiling() {
    let uri = format!("data:image/png;base64,{CANONICAL_PNG_B64}");
    assert_eq!(
        split_base64_image_data_uri(&uri, CANONICAL_PNG_B64.len() - 1),
        Err(ImageDataUriRejection::TooLarge)
    );
    // Exactly at the ceiling is accepted.
    assert!(split_base64_image_data_uri(&uri, CANONICAL_PNG_B64.len()).is_ok());
}

#[test]
fn split_data_uri_rejections_carry_a_short_reason() {
    assert_eq!(
        ImageDataUriRejection::TooLarge.to_string(),
        "image payload exceeds the per-image ceiling"
    );
    assert_eq!(
        ImageDataUriRejection::MalformedBase64.to_string(),
        "malformed base64 payload"
    );
}

#[test]
fn strip_media_markers_replaces_image_local_path() {
    let input = "Look at [IMAGE:/zeroclaw-data/workspace/telegram_files/photo_1.jpg]";
    assert_eq!(strip_media_markers(input), "Look at [media attachment]");
}

#[test]
fn strip_media_markers_replaces_image_data_uri() {
    let input = "Inline [IMAGE:data:image/png;base64,abcd]";
    assert_eq!(strip_media_markers(input), "Inline [media attachment]");
}

#[test]
fn strip_media_markers_replaces_all_supported_kinds() {
    // Mirrors `ATTACHMENT_KINDS` in
    // `crates/zeroclaw-channels/src/util.rs`, which is the source of
    // truth for which marker spellings inbound channels can produce.
    let input = "[IMAGE:/a.jpg] [PHOTO:/b.jpg] [DOCUMENT:/c.pdf] [FILE:/d.zip] [VIDEO:/e.mp4] [VOICE:/f.ogg] [AUDIO:/g.wav]";
    let expected = "[media attachment] [media attachment] [media attachment] [media attachment] [media attachment] [media attachment] [media attachment]";
    assert_eq!(strip_media_markers(input), expected);
}

#[test]
fn strip_media_markers_is_case_insensitive() {
    // Channel parsers uppercase the kind before comparing, so by the time
    // a marker reaches conversation history it is normally upper-case —
    // but accept lower/mixed case too so we don't depend on that
    // invariant downstream.
    let input = "[image:/a.jpg] [Photo:/b.jpg] [video:/c.mp4]";
    let expected = "[media attachment] [media attachment] [media attachment]";
    assert_eq!(strip_media_markers(input), expected);
}

#[test]
fn strip_media_markers_leaves_plain_text_untouched() {
    let input = "No markers here, just text with [brackets] and (parens).";
    assert_eq!(strip_media_markers(input), input);
}

#[test]
fn strip_media_markers_preserves_unrelated_brackets() {
    // Markers that don't match the media kinds are left alone.
    let input = "Use [TODO:foo] and [NOTE:bar] but replace [IMAGE:/x.jpg]";
    assert_eq!(
        strip_media_markers(input),
        "Use [TODO:foo] and [NOTE:bar] but replace [media attachment]"
    );
}

// ── loadable audio markers degrade; other media kinds keep their paths ──

#[test]
fn strip_unplayable_audio_markers_replaces_loadable_audio_path() {
    let (out, n) = strip_unplayable_audio_markers("hear this [AUDIO:/tmp/clip.wav] now");
    assert_eq!(out, "hear this [media attachment] now");
    assert_eq!(n, 1);
}

#[test]
fn strip_unplayable_audio_markers_degrades_audio_kinds_only() {
    // The delivery contract: DOCUMENT/FILE/VIDEO/PHOTO paths stay
    // model-visible so the agent can hand them to file tools or copy them
    // into outbound reply markers; only the audio kinds degrade.
    let input = "[PHOTO:/a.jpg] [DOCUMENT:/b.pdf] [FILE:/c.zip] [VIDEO:/d.mp4] [VOICE:/e.ogg] [AUDIO:/f.wav]";
    let (out, n) = strip_unplayable_audio_markers(input);
    assert_eq!(
        out,
        "[PHOTO:/a.jpg] [DOCUMENT:/b.pdf] [FILE:/c.zip] [VIDEO:/d.mp4] [media attachment] [media attachment]"
    );
    assert_eq!(n, 2);
}

#[test]
fn audio_marker_kinds_is_subset_of_media_marker_kinds() {
    for kind in AUDIO_MARKER_KINDS {
        assert!(
            MEDIA_MARKER_KINDS.contains(kind),
            "audio kind {kind} missing from the full marker vocabulary"
        );
    }
}

#[test]
fn strip_unplayable_audio_markers_leaves_image_marker_untouched() {
    // `[IMAGE:...]` is handled by `parse_image_markers`; the audio
    // stripper must never touch it (that would drop a resolvable image).
    let (out, n) = strip_unplayable_audio_markers("[IMAGE:/a.png] and [AUDIO:/b.wav]");
    assert_eq!(out, "[IMAGE:/a.png] and [media attachment]");
    assert_eq!(n, 1);
}

#[test]
fn strip_unplayable_audio_markers_preserves_non_loadable_payloads() {
    // Placeholders, prose, a bare filename, and the no-transcription
    // `[Audio: attached]` note are harmless literal text — keep them.
    for input in [
        "[AUDIO:...]",
        "[VOICE:<clip>]",
        "[Audio: attached]",
        "[AUDIO:example.wav]",
    ] {
        let (out, n) = strip_unplayable_audio_markers(input);
        assert_eq!(out, input, "should preserve non-loadable marker: {input}");
        assert_eq!(
            n, 0,
            "non-loadable marker must not count as stripped: {input}"
        );
    }
}

#[test]
fn strip_unplayable_audio_markers_is_case_insensitive() {
    let (out, n) = strip_unplayable_audio_markers("[Audio:/tmp/clip.wav]");
    assert_eq!(out, "[media attachment]");
    assert_eq!(n, 1);
}

#[test]
fn strip_unplayable_audio_markers_handles_data_uri_and_url() {
    let (out, n) = strip_unplayable_audio_markers(
        "[VOICE:data:audio/ogg;base64,AAAA] and [AUDIO:https://x/y.mp3]",
    );
    assert_eq!(out, "[media attachment] and [media attachment]");
    assert_eq!(n, 2);
}

#[tokio::test]
async fn prepare_messages_strips_tool_result_audio_marker() {
    // The reported failure: a tool result surfaces an audio path. With no
    // images in history, prep must still strip the marker so the raw
    // filesystem path never reaches the provider as literal text.
    let history = vec![
        ChatMessage::user("call the tool and tell me what you hear"),
        ChatMessage::tool("[AUDIO:/tmp/clip.wav] recorded 3:00 PM"),
    ];
    let cfg = MultimodalConfig::default();
    let prepared = prepare_messages_for_provider(&history, &cfg).await.unwrap();
    let tool_msg = prepared
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("tool message survives prep");
    assert!(
        !tool_msg.content.contains("/tmp/clip.wav"),
        "raw audio path must not reach the provider: {}",
        tool_msg.content
    );
    assert!(tool_msg.content.contains("[media attachment]"));
    assert!(!prepared.contains_images);
}

#[tokio::test]
async fn prepare_messages_preserves_document_marker_for_delivery() {
    // A tool result that surfaces a document path must reach the provider
    // intact: the agent copies that path into an outbound reply marker to
    // deliver the file, and file tools read it on request. Only the audio
    // kinds degrade.
    let history = vec![
        ChatMessage::user("send me the report"),
        ChatMessage::tool("[DOCUMENT:/workspace/report.pdf] generated, and [AUDIO:/tmp/note.wav]"),
    ];
    let cfg = MultimodalConfig::default();
    let prepared = prepare_messages_for_provider(&history, &cfg).await.unwrap();
    let tool_msg = prepared
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("tool message survives prep");
    assert!(
        tool_msg
            .content
            .contains("[DOCUMENT:/workspace/report.pdf]"),
        "document path must stay model-visible for delivery: {}",
        tool_msg.content
    );
    assert!(
        !tool_msg.content.contains("/tmp/note.wav"),
        "audio path alongside it must still degrade: {}",
        tool_msg.content
    );
}

#[tokio::test]
async fn prepare_messages_strips_audio_but_keeps_image_marker() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("shot.png");
    std::fs::write(&path, [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']).unwrap();
    let history = vec![ChatMessage::user(format!(
        "look [IMAGE:{}] and hear [AUDIO:/tmp/clip.wav]",
        path.display()
    ))];
    let cfg = MultimodalConfig::default();
    let prepared = prepare_messages_for_provider(&history, &cfg).await.unwrap();
    let content = &prepared.messages[0].content;
    assert!(
        !content.contains("/tmp/clip.wav"),
        "audio path must be stripped: {content}"
    );
    assert!(content.contains("[media attachment]"));
    // The image marker is still normalized to a data URI alongside it.
    assert!(prepared.contains_images, "image still inlined: {content}");
}

#[test]
fn parse_image_markers_extracts_multiple_markers() {
    let input = "Check this [IMAGE:/tmp/a.png] and this [IMAGE:https://example.com/b.jpg]";
    let (cleaned, refs) = parse_image_markers(input);

    assert_eq!(cleaned, "Check this  and this");
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0], "/tmp/a.png");
    assert_eq!(refs[1], "https://example.com/b.jpg");
}

#[test]
fn is_windows_unc_path_accepts_shares_and_rejects_others() {
    assert!(is_windows_unc_path(r"\\server\share\pic.png"));
    assert!(is_windows_unc_path(r"\\server\share\sub\pic.png"));
    // Verbatim / device prefixes are not plain shares.
    assert!(!is_windows_unc_path(r"\\?\C:\Users\me\a.png"));
    assert!(!is_windows_unc_path(r"\\?\UNC\server\share\a.png"));
    assert!(!is_windows_unc_path(r"\\.\PhysicalDrive0"));
    // Needs both a server and a further segment.
    assert!(!is_windows_unc_path(r"\\server"));
    assert!(!is_windows_unc_path(r"\\"));
    // Non-UNC inputs.
    assert!(!is_windows_unc_path("/home/me/a.png"));
    assert!(!is_windows_unc_path(r"C:\Users\me\a.png"));
}

#[test]
fn parse_image_markers_extracts_unc_path() {
    // Regression for theWindows follow-up: `image_info` unwraps the
    // verbatim-UNC prefix (`\\?\UNC\…`) to a plain `\\server\share\…`
    // path, which must be treated as a loadable image reference (not left
    // as literal text) so the image reaches vision models.
    let input = r"File: [IMAGE:\\server\share\pic.png]";
    let (_, refs) = parse_image_markers(input);
    assert_eq!(refs.len(), 1, "UNC marker should be extracted as a ref");
    assert_eq!(refs[0], r"\\server\share\pic.png");
}

#[test]
fn validate_mime_rejects_bmp_but_accepts_provider_supported_types() {
    for mime in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
        assert!(
            validate_mime("src", mime).is_ok(),
            "{mime} should be allowed"
        );
    }
    // BMP is detectable but unsupported by vision providers; it must be
    // rejected here so it never breaks the whole provider request.
    let err = validate_mime("src", "image/bmp").unwrap_err();
    assert_eq!(multimodal_error_kind(&err), "unsupported_mime");
}

#[test]
fn parse_image_markers_collapses_line_wrapped_path() {
    // Terminal-wrapped paste: a long path split across two rows with
    // leading indentation should be recovered into the original path.
    let input = "from the logs whether the agent emits\n  [IMAGE:/home/zeroclaw_user/.zeroclaw/workspace/signal_i\n  nbound/attachment.jpg] (which the\n  channel resolves)";
    let (_, refs) = parse_image_markers(input);
    assert_eq!(refs.len(), 1);
    assert_eq!(
        refs[0],
        "/home/zeroclaw_user/.zeroclaw/workspace/signal_inbound/attachment.jpg"
    );
}

#[test]
fn parse_image_markers_leaves_placeholder_markers_as_literal_text() {
    // Illustrative markdown like `[IMAGE:...]` or `[IMAGE:<path>]`
    // (e.g. in agent-authored prose the user quotes back) is not a
    // loadable reference and must stay as literal text — otherwise the
    // multimodal loader errors every turn the conversation replays.
    let input = "example: `[IMAGE:...]` or `[IMAGE:<path>]` or `[IMAGE:example.png]`";
    let (cleaned, refs) = parse_image_markers(input);
    assert!(
        refs.is_empty(),
        "no placeholder should be treated as a loadable ref, got: {refs:?}"
    );
    assert!(cleaned.contains("[IMAGE:...]"));
    assert!(cleaned.contains("[IMAGE:<path>]"));
    assert!(cleaned.contains("[IMAGE:example.png]"));
}

#[test]
fn parse_image_markers_preserves_spaces_in_path() {
    // Spaces within a single-line marker are legitimate (paths can
    // contain spaces) and must survive unchanged.
    let input = "look at [IMAGE:/tmp/my photos/beetle.png] please";
    let (_, refs) = parse_image_markers(input);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0], "/tmp/my photos/beetle.png");
}

#[test]
fn parse_image_markers_keeps_invalid_empty_marker() {
    let input = "hello [IMAGE:] world";
    let (cleaned, refs) = parse_image_markers(input);

    assert_eq!(cleaned, "hello [IMAGE:] world");
    assert!(refs.is_empty());
}

#[tokio::test]
async fn prepare_messages_normalizes_local_image_to_data_uri() {
    let temp = tempfile::tempdir().unwrap();
    let image_path = temp.path().join("sample.png");

    // Minimal PNG signature bytes are enough for MIME detection.
    std::fs::write(
        &image_path,
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'],
    )
    .unwrap();

    let messages = vec![ChatMessage::user(format!(
        "Please inspect this screenshot [IMAGE:{}]",
        image_path.display()
    ))];

    let prepared = prepare_messages_for_provider(&messages, &MultimodalConfig::default())
        .await
        .unwrap();

    assert!(prepared.contains_images);
    assert_eq!(prepared.messages.len(), 1);

    let (cleaned, refs) = parse_image_markers(&prepared.messages[0].content);
    assert_eq!(cleaned, "Please inspect this screenshot");
    assert_eq!(refs.len(), 1);
    assert!(refs[0].starts_with("data:image/png;base64,"));
}

#[tokio::test]
async fn prepare_messages_normalizes_tool_message_local_image_to_data_uri() {
    let temp = tempfile::tempdir().unwrap();
    let image_path = temp.path().join("tool-sample.png");

    std::fs::write(
        &image_path,
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'],
    )
    .unwrap();

    let messages = vec![ChatMessage::tool(format!(
        "<tool_result name=\"image_gen\">\nGenerated image [IMAGE:{}]\n</tool_result>",
        image_path.display()
    ))];

    let prepared = prepare_messages_for_provider(&messages, &MultimodalConfig::default())
        .await
        .unwrap();

    assert!(prepared.contains_images);
    assert_eq!(prepared.messages.len(), 1);
    assert_eq!(prepared.messages[0].role, "tool");

    let (cleaned, refs) = parse_image_markers(&prepared.messages[0].content);
    assert!(cleaned.contains("<tool_result name=\"image_gen\">"));
    assert!(cleaned.contains("Generated image"));
    assert_eq!(refs.len(), 1);
    assert!(refs[0].starts_with("data:image/png;base64,"));
}

#[tokio::test]
async fn prepare_messages_keeps_native_tool_result_json_valid_when_over_the_image_cap() {
    // Trim inside the tool envelope so retained markers cannot be appended
    // after its closing brace. The default cap retains four of five images.
    let temp = tempfile::tempdir().unwrap();
    let mut markers = Vec::new();
    let mut expected_uris = Vec::new();
    for index in 0..5_u8 {
        let image_path = temp.path().join(format!("shot-{index}.png"));
        let bytes = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', index];
        std::fs::write(&image_path, bytes).unwrap();
        expected_uris.push(format!("data:image/png;base64,{}", STANDARD.encode(bytes)));
        markers.push(format!("[IMAGE:{}]", image_path.display()));
    }

    let native_tool_content = serde_json::json!({
        "tool_call_id": "tc-overflow",
        "tool_name": "screenshot",
        "status": "ok",
        "content": format!("captured five {}", markers.join(" ")),
    })
    .to_string();

    let config = MultimodalConfig::default();
    let prepared =
        prepare_messages_for_provider(&[ChatMessage::tool(native_tool_content)], &config)
            .await
            .expect("preparation should succeed for an over-cap native tool result");

    assert_eq!(prepared.messages[0].role, "tool");
    let value: serde_json::Value = serde_json::from_str(&prepared.messages[0].content)
        .expect("an over-cap tool result must still be valid JSON");

    assert_eq!(
        value.get("tool_call_id").and_then(|v| v.as_str()),
        Some("tc-overflow"),
        "tool_call_id must survive trimming so the provider can emit a native tool result"
    );
    assert_eq!(
        value.get("tool_name").and_then(|v| v.as_str()),
        Some("screenshot"),
        "other envelope metadata must survive trimming"
    );
    assert_eq!(
        value.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "non-content envelope fields must survive trimming unchanged"
    );

    let inner = value
        .get("content")
        .and_then(|v| v.as_str())
        .expect("content must remain a JSON string");
    assert!(
        inner.contains("captured five"),
        "surrounding text must survive trimming"
    );

    let (_, refs) = parse_image_markers(inner);
    assert_eq!(
        refs.len(),
        config.max_images,
        "exactly the budgeted images are retained, and they live inside `content`"
    );
    assert_eq!(
        refs,
        expected_uris[1..],
        "the newest four images survive in order"
    );
}

#[tokio::test]
async fn prepare_messages_preserves_native_tool_result_json_shape() {
    let temp = tempfile::tempdir().unwrap();
    let image_path = temp.path().join("native-tool-result.png");
    std::fs::write(
        &image_path,
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'],
    )
    .unwrap();

    let native_tool_content = serde_json::json!({
        "tool_call_id": "tc1",
        "content": format!("see attached [IMAGE:{}]", image_path.display().to_string()),
    })
    .to_string();

    let messages = vec![ChatMessage::tool(native_tool_content)];

    let prepared = prepare_messages_for_provider(&messages, &MultimodalConfig::default())
        .await
        .expect("preparation should succeed for native tool-result JSON");

    assert!(prepared.contains_images);
    assert_eq!(prepared.messages.len(), 1);
    assert_eq!(prepared.messages[0].role, "tool");

    let value: serde_json::Value = serde_json::from_str(&prepared.messages[0].content)
        .expect("prepared tool message must remain valid JSON");

    assert_eq!(
        value.get("tool_call_id").and_then(|v| v.as_str()),
        Some("tc1"),
        "tool_call_id must survive multimodal preprocessing unchanged"
    );

    let inner = value
        .get("content")
        .and_then(|v| v.as_str())
        .expect("content must remain a JSON string");
    assert!(
        inner.contains("see attached"),
        "surrounding text in tool content should survive normalization"
    );
    assert!(
        inner.contains("data:image/png;base64,"),
        "local image path inside tool content should be rewritten to a data URI"
    );
    assert!(
        !inner.contains("native-tool-result.png"),
        "raw local path must not leak after normalization"
    );
}

#[tokio::test]
async fn prepare_messages_preserves_native_tool_json_when_image_is_skipped() {
    let native_tool_content = serde_json::json!({
        "tool_call_id": "tc1",
        "content": "generated screenshot [IMAGE:https://example.com/missing.png]",
    })
    .to_string();

    let prepared = prepare_messages_for_provider(
        &[ChatMessage::tool(native_tool_content)],
        &MultimodalConfig::default(),
    )
    .await
    .expect("skipped native tool image should not fail message preparation");

    assert!(!prepared.contains_images);
    assert_eq!(prepared.messages.len(), 1);

    let value: serde_json::Value = serde_json::from_str(&prepared.messages[0].content)
        .expect("native tool result must remain valid JSON");
    assert_eq!(
        value.get("tool_call_id").and_then(|v| v.as_str()),
        Some("tc1")
    );

    let inner = value
        .get("content")
        .and_then(|v| v.as_str())
        .expect("content should remain a JSON string");
    assert!(inner.contains("generated screenshot"));
    assert!(inner.contains("1 attached image(s) could not be loaded"));
    assert!(!inner.contains("[IMAGE:"));
    assert!(!inner.contains("https://example.com/missing.png"));
}

#[tokio::test]
async fn prepare_messages_preserves_native_tool_json_with_mixed_images() {
    let temp = tempfile::tempdir().unwrap();
    let image_path = temp.path().join("mixed-native-tool-result.png");
    std::fs::write(
        &image_path,
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'],
    )
    .unwrap();

    let native_tool_content = serde_json::json!({
        "tool_call_id": "tc1",
        "content": format!(
            "generated [IMAGE:{}] and [IMAGE:https://example.com/missing.png]",
            image_path.display()
        ),
    })
    .to_string();

    let prepared = prepare_messages_for_provider(
        &[ChatMessage::tool(native_tool_content)],
        &MultimodalConfig::default(),
    )
    .await
    .expect("valid native tool image should survive while bad ref is skipped");

    assert!(prepared.contains_images);
    assert_eq!(prepared.messages.len(), 1);

    let value: serde_json::Value = serde_json::from_str(&prepared.messages[0].content)
        .expect("native tool result must remain valid JSON");
    assert_eq!(
        value.get("tool_call_id").and_then(|v| v.as_str()),
        Some("tc1")
    );

    let inner = value
        .get("content")
        .and_then(|v| v.as_str())
        .expect("content should remain a JSON string");
    assert!(inner.contains("generated"));
    assert!(inner.contains("data:image/png;base64,"));
    assert!(inner.contains("1 of 2 attached image(s) could not be loaded"));
    assert!(!inner.contains("mixed-native-tool-result.png"));
    assert!(!inner.contains("https://example.com/missing.png"));
}

#[tokio::test]
async fn prepare_messages_strips_stale_native_tool_result_images() {
    let temp = tempfile::tempdir().unwrap();
    let image_path = temp.path().join("stale-native-tool-result.png");
    std::fs::write(
        &image_path,
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'],
    )
    .unwrap();

    let native_tool_content = serde_json::json!({
        "tool_call_id": "tc1",
        "content": format!("generated screenshot [IMAGE:{}]", image_path.display().to_string()),
    })
    .to_string();

    let messages = vec![
        ChatMessage::tool(native_tool_content),
        ChatMessage {
            role: "assistant".to_string(),
            content: "I generated the screenshot.".to_string(),
        },
        ChatMessage::user("What happened next?".to_string()),
    ];

    let prepared = prepare_messages_for_provider(&messages, &MultimodalConfig::default())
        .await
        .expect("preparation should strip stale tool images without loading them");

    assert!(
        !prepared.contains_images,
        "stale tool-result images should not keep the request in vision mode"
    );

    let value: serde_json::Value = serde_json::from_str(&prepared.messages[0].content)
        .expect("stale native tool result should remain valid JSON");
    assert_eq!(
        value.get("tool_call_id").and_then(|v| v.as_str()),
        Some("tc1")
    );

    let inner = value
        .get("content")
        .and_then(|v| v.as_str())
        .expect("content should remain a JSON string");
    assert!(inner.contains("generated screenshot"));
    assert!(!inner.contains("[IMAGE:"));
    assert!(!inner.contains("data:image"));
    assert!(!inner.contains("stale-native-tool-result.png"));
}

#[tokio::test]
async fn prepare_messages_strips_stale_prompt_tool_result_images() {
    let temp = tempfile::tempdir().unwrap();
    let image_path = temp.path().join("stale-prompt-tool-result.png");
    std::fs::write(
        &image_path,
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'],
    )
    .unwrap();

    let messages = vec![
        ChatMessage::user(format!(
            "[Tool results]\n<tool_result name=\"image_gen\">Generated [IMAGE:{}]</tool_result>",
            image_path.display()
        )),
        ChatMessage {
            role: "assistant".to_string(),
            content: "I generated the screenshot.".to_string(),
        },
        ChatMessage::user("Continue.".to_string()),
    ];

    let prepared = prepare_messages_for_provider(&messages, &MultimodalConfig::default())
        .await
        .expect("preparation should strip stale prompt-mode tool images");

    assert!(!prepared.contains_images);
    assert!(prepared.messages[0].content.contains("[Tool results]"));
    assert!(prepared.messages[0].content.contains("Generated"));
    assert!(!prepared.messages[0].content.contains("[IMAGE:"));
    assert!(!prepared.messages[0].content.contains("data:image"));
    assert!(
        !prepared.messages[0]
            .content
            .contains("stale-prompt-tool-result.png")
    );
}

#[tokio::test]
async fn prepare_messages_strips_stale_tool_image_while_normalizing_current_user_image() {
    let temp = tempfile::tempdir().unwrap();
    let stale_path = temp.path().join("stale-tool-result.png");
    let fresh_path = temp.path().join("fresh-user-image.png");
    let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    std::fs::write(&stale_path, png).unwrap();
    std::fs::write(&fresh_path, png).unwrap();

    let native_tool_content = serde_json::json!({
        "tool_call_id": "tc1",
        "content": format!("generated screenshot [IMAGE:{}]", stale_path.display().to_string()),
    })
    .to_string();

    let messages = vec![
        ChatMessage::tool(native_tool_content),
        ChatMessage {
            role: "assistant".to_string(),
            content: "I generated the screenshot.".to_string(),
        },
        ChatMessage::user(format!(
            "Now inspect this [IMAGE:{}]",
            fresh_path.display().to_string()
        )),
    ];

    let prepared = prepare_messages_for_provider(&messages, &MultimodalConfig::default())
        .await
        .expect("preparation should strip stale tool images and normalize current user image");

    assert!(prepared.contains_images);

    let value: serde_json::Value = serde_json::from_str(&prepared.messages[0].content)
        .expect("stale native tool result should remain valid JSON");
    let inner = value
        .get("content")
        .and_then(|v| v.as_str())
        .expect("content should remain a JSON string");
    assert!(inner.contains("generated screenshot"));
    assert!(!inner.contains("[IMAGE:"));
    assert!(!inner.contains("data:image"));
    assert!(!inner.contains("stale-tool-result.png"));

    let (cleaned, refs) = parse_image_markers(&prepared.messages[2].content);
    assert_eq!(cleaned, "Now inspect this");
    assert_eq!(refs.len(), 1);
    assert!(refs[0].starts_with("data:image/png;base64,"));
    assert!(
        !prepared.messages[2]
            .content
            .contains("fresh-user-image.png")
    );
}

#[test]
fn count_image_markers_ignores_stale_tool_results() {
    let messages = vec![
        ChatMessage::tool("[IMAGE:/tmp/stale-tool.png]\nGenerated".to_string()),
        ChatMessage {
            role: "assistant".to_string(),
            content: "Done.".to_string(),
        },
        ChatMessage::user("Next question".to_string()),
    ];

    assert_eq!(count_image_markers(&messages), 0);

    let messages = vec![
        ChatMessage::user("Create an image".to_string()),
        ChatMessage::tool("[IMAGE:/tmp/latest-tool.png]\nGenerated".to_string()),
    ];

    assert_eq!(count_image_markers(&messages), 1);
}

#[test]
fn count_latest_user_image_markers_scopes_to_newest_user_message() {
    // No user messages at all -> zero.
    assert_eq!(count_latest_user_image_markers(&[]), 0);

    // The newest user message carries the image -> counted (the user just
    // sent it; the vision router surfaces a capability error).
    let just_sent = vec![
        ChatMessage::user("hi".to_string()),
        ChatMessage {
            role: "assistant".to_string(),
            content: "hello".to_string(),
        },
        ChatMessage::user("look at this [IMAGE:/tmp/a.png]".to_string()),
    ];
    assert_eq!(count_latest_user_image_markers(&just_sent), 1);

    // An earlier user message carried an image, but the newest user message
    // is plain text -> zero. This is the poison-prevention case: the carried
    // over marker must NOT keep re-triggering the capability error.
    let carried_over = vec![
        ChatMessage::user("look at this [IMAGE:/tmp/a.png]".to_string()),
        ChatMessage::user("what is WAL?".to_string()),
    ];
    assert_eq!(count_latest_user_image_markers(&carried_over), 0);
    // The history-wide count still sees the carried-over marker, which is
    // why the router must distinguish the two.
    assert_eq!(count_user_image_markers(&carried_over), 1);

    // A trailing tool-result carrier does not mask the real latest user
    // message (its markers are not user-sent and must not be counted here).
    let trailing_tool_result = vec![
        ChatMessage::user("inspect [IMAGE:/tmp/a.png]".to_string()),
        ChatMessage::tool("[IMAGE:/tmp/tool.png]\nGenerated".to_string()),
    ];
    assert_eq!(count_latest_user_image_markers(&trailing_tool_result), 1);
}

#[tokio::test]
async fn prepare_messages_trims_excess_images_from_older_messages() {
    // 3 messages, each with 1 image — max is 2.
    // The oldest message's image should be stripped.
    let messages = vec![
        ChatMessage::user("[IMAGE:/tmp/old.png]\nOld caption".to_string()),
        ChatMessage::user("[IMAGE:/tmp/mid.png]\nMid caption".to_string()),
        ChatMessage::user("[IMAGE:/tmp/new.png]\nNew caption".to_string()),
    ];

    // Should not error — instead trims oldest. (Will error on
    // normalize_image_reference for the surviving images since
    // /tmp/mid.png and /tmp/new.png don't exist, but the trimming
    // itself should succeed.)
    let trimmed = trim_old_images(&messages, 2);
    assert_eq!(trimmed.len(), 3);

    // Oldest message should have image stripped
    let (_, refs0) = parse_image_markers(&trimmed[0].content);
    assert!(refs0.is_empty(), "oldest image should be stripped");
    assert!(trimmed[0].content.contains("Old caption"));

    // Newer messages keep their images
    let (_, refs1) = parse_image_markers(&trimmed[1].content);
    assert_eq!(refs1.len(), 1);
    let (_, refs2) = parse_image_markers(&trimmed[2].content);
    assert_eq!(refs2.len(), 1);
}

#[test]
fn trim_old_images_replaces_image_only_message() {
    // A message with only an image and no text should get a placeholder.
    let messages = vec![
        ChatMessage::user("[IMAGE:/tmp/old.png]".to_string()),
        ChatMessage::user("[IMAGE:/tmp/new.png]\nKeep this".to_string()),
    ];

    let trimmed = trim_old_images(&messages, 1);
    assert_eq!(trimmed[0].content, "[image removed from history]");
    assert!(trimmed[1].content.contains("[IMAGE:/tmp/new.png]"));
}

#[test]
fn trim_old_images_partially_trims_a_multi_image_message() {
    // A single message has 3 images and the budget is 1, so exactly 2 must
    // be dropped. Evicting the message as a unit would remove all three and
    // leave zero images, spending none of the budget the operator allowed.
    let messages = vec![
        ChatMessage::user(
            "[IMAGE:/tmp/a.png]\n[IMAGE:/tmp/b.png]\n[IMAGE:/tmp/c.png]\nThree pics".to_string(),
        ),
        ChatMessage::user("Just text, no images".to_string()),
    ];

    let trimmed = trim_old_images(&messages, 1);
    assert_eq!(trimmed.len(), 2);
    // The newest image in the message survives; the two older ones go.
    let (_, refs0) = parse_image_markers(&trimmed[0].content);
    assert_eq!(refs0, vec!["/tmp/c.png".to_string()]);
    assert!(trimmed[0].content.contains("Three pics"));
    // Second message unchanged
    assert_eq!(trimmed[1].content, "Just text, no images");
}

#[test]
fn trim_old_images_keeps_newest_two_of_three_in_one_message() {
    // Three images against a declared budget of two: exactly the newest two
    // survive, so the request uses the whole budget instead of undershooting.
    let messages = vec![
        ChatMessage::user(
            "[IMAGE:/tmp/a.png]\n[IMAGE:/tmp/b.png]\n[IMAGE:/tmp/c.png]\nThree pics".to_string(),
        ),
        ChatMessage::user("Just text, no images".to_string()),
    ];

    let trimmed = trim_old_images(&messages, 2);
    assert_eq!(trimmed.len(), 2);
    let (cleaned, refs0) = parse_image_markers(&trimmed[0].content);
    assert_eq!(
        refs0,
        vec!["/tmp/b.png".to_string(), "/tmp/c.png".to_string()],
        "the newest two of three survive a budget of two"
    );
    assert!(cleaned.contains("Three pics"));
    assert_eq!(trimmed[1].content, "Just text, no images");
}

#[test]
fn trim_old_images_drops_exactly_the_overflow() {
    // The invariant the cap exists to enforce: whatever the per-message
    // distribution, the survivors equal the budget rather than undershoot.
    let messages = vec![
        ChatMessage::user("[IMAGE:/tmp/a.png]\n[IMAGE:/tmp/b.png]\nPair".to_string()),
        ChatMessage::user("[IMAGE:/tmp/c.png]\nSingle".to_string()),
        ChatMessage::user("[IMAGE:/tmp/d.png]\n[IMAGE:/tmp/e.png]\nAnother pair".to_string()),
    ];

    for max_images in 1..=5 {
        let trimmed = trim_old_images(&messages, max_images);
        assert_eq!(
            count_image_markers(&trimmed),
            max_images,
            "max_images={max_images} must keep exactly that many images"
        );
    }
}

#[test]
fn trim_old_images_keeps_the_newest_images_across_messages() {
    let messages = vec![
        ChatMessage::user("[IMAGE:/tmp/a.png]\n[IMAGE:/tmp/b.png]\nOld".to_string()),
        ChatMessage::user("[IMAGE:/tmp/c.png]\n[IMAGE:/tmp/d.png]\nNew".to_string()),
    ];

    let trimmed = trim_old_images(&messages, 3);

    // Oldest single image evicted; everything newer survives.
    let (_, refs0) = parse_image_markers(&trimmed[0].content);
    assert_eq!(refs0, vec!["/tmp/b.png".to_string()]);
    let (_, refs1) = parse_image_markers(&trimmed[1].content);
    assert_eq!(
        refs1,
        vec!["/tmp/c.png".to_string(), "/tmp/d.png".to_string()]
    );
}

#[test]
fn trim_old_images_preserves_unrelated_text_around_kept_images() {
    // Only the evicted marker goes; prose before, between, and after the
    // retained image is untouched.
    let messages = vec![ChatMessage::user(
        "before [IMAGE:/tmp/a.png] middle [IMAGE:/tmp/b.png] after".to_string(),
    )];

    let trimmed = trim_old_images(&messages, 1);

    let (cleaned, refs) = parse_image_markers(&trimmed[0].content);
    assert_eq!(refs, vec!["/tmp/b.png".to_string()]);
    assert!(
        cleaned.contains("before"),
        "leading text survives: {cleaned}"
    );
    assert!(
        cleaned.contains("middle"),
        "middle text survives: {cleaned}"
    );
    assert!(
        cleaned.contains("after"),
        "trailing text survives: {cleaned}"
    );
}

#[test]
fn trim_old_images_zero_no_image_and_within_budget_bounds() {
    // Zero budget evicts every image but keeps the text, using the same
    // removed-history placeholder for an image-only message.
    let zero_budget = vec![
        ChatMessage::user("[IMAGE:/tmp/a.png]\nCaption A".to_string()),
        ChatMessage::user("[IMAGE:/tmp/b.png]".to_string()),
    ];
    let trimmed = trim_old_images(&zero_budget, 0);
    for message in &trimmed {
        let (_, refs) = parse_image_markers(&message.content);
        assert!(refs.is_empty(), "zero budget must drop every image");
    }
    assert_eq!(trimmed[0].content, "Caption A");
    assert_eq!(trimmed[1].content, "[image removed from history]");

    // No images at all: the message is returned unchanged.
    let no_images = vec![ChatMessage::user("plain text".to_string())];
    let trimmed = trim_old_images(&no_images, 4);
    assert_eq!(trimmed[0].content, "plain text");

    // Within budget: no eviction and no rewrite.
    let within_budget = vec![
        ChatMessage::user("[IMAGE:/tmp/a.png]\nA".to_string()),
        ChatMessage::user("[IMAGE:/tmp/b.png]\nB".to_string()),
    ];
    let trimmed = trim_old_images(&within_budget, 2);
    assert_eq!(trimmed[0].content, within_budget[0].content);
    assert_eq!(trimmed[1].content, within_budget[1].content);
}

#[test]
fn trim_old_images_skips_assistant_messages() {
    // Assistant messages with image markers should not be counted or stripped.
    let messages = vec![
        ChatMessage {
            role: "assistant".to_string(),
            content: "[IMAGE:/tmp/assistant.png]\nAssistant generated".to_string(),
        },
        ChatMessage::user("[IMAGE:/tmp/user1.png]\nFirst".to_string()),
        ChatMessage::user("[IMAGE:/tmp/user2.png]\nSecond".to_string()),
    ];

    let trimmed = trim_old_images(&messages, 1);
    // Assistant message untouched (not counted toward limit)
    assert!(trimmed[0].content.contains("[IMAGE:/tmp/assistant.png]"));
    // Oldest user image stripped
    let (_, refs1) = parse_image_markers(&trimmed[1].content);
    assert!(refs1.is_empty());
    assert!(trimmed[1].content.contains("First"));
    // Newest user image kept
    let (_, refs2) = parse_image_markers(&trimmed[2].content);
    assert_eq!(refs2.len(), 1);
}

#[test]
fn trim_old_images_counts_latest_tool_messages() {
    let messages = vec![
        ChatMessage::user("[IMAGE:/tmp/user-old.png]\nOldest".to_string()),
        ChatMessage::tool("[IMAGE:/tmp/tool-new.png]\nGenerated".to_string()),
    ];

    let trimmed = trim_old_images(&messages, 1);
    let (_, refs0) = parse_image_markers(&trimmed[0].content);
    assert!(refs0.is_empty(), "oldest user image should be stripped");
    assert!(trimmed[0].content.contains("Oldest"));

    let (_, refs1) = parse_image_markers(&trimmed[1].content);
    assert_eq!(refs1.len(), 1);
}

#[test]
fn trim_old_images_no_trimming_when_under_limit() {
    let messages = vec![
        ChatMessage::user("[IMAGE:/tmp/a.png]\nCaption A".to_string()),
        ChatMessage::user("[IMAGE:/tmp/b.png]\nCaption B".to_string()),
    ];

    let trimmed = trim_old_images(&messages, 5);
    // Nothing should change — both images are under the limit
    assert_eq!(trimmed[0].content, messages[0].content);
    assert_eq!(trimmed[1].content, messages[1].content);
}

#[test]
fn trim_old_images_no_trimming_when_exactly_at_limit() {
    let messages = vec![
        ChatMessage::user("[IMAGE:/tmp/a.png]\nA".to_string()),
        ChatMessage::user("[IMAGE:/tmp/b.png]\nB".to_string()),
    ];

    let trimmed = trim_old_images(&messages, 2);
    assert_eq!(trimmed[0].content, messages[0].content);
    assert_eq!(trimmed[1].content, messages[1].content);
}

#[test]
fn trim_old_images_empty_messages() {
    let trimmed = trim_old_images(&[], 4);
    assert!(trimmed.is_empty());
}

#[test]
fn trim_old_images_interleaved_roles() {
    // Realistic conversation: user sends image, assistant replies, user sends
    // another image, etc. Only user messages should be candidates for trimming.
    let messages = vec![
        ChatMessage::user("[IMAGE:/tmp/1.png]\nLook at this".to_string()),
        ChatMessage {
            role: "assistant".to_string(),
            content: "I see a photo.".to_string(),
        },
        ChatMessage::user("[IMAGE:/tmp/2.png]\nWhat about this?".to_string()),
        ChatMessage {
            role: "assistant".to_string(),
            content: "That's a chart.".to_string(),
        },
        ChatMessage::user("[IMAGE:/tmp/3.png]\nAnd this one".to_string()),
    ];

    let trimmed = trim_old_images(&messages, 2);
    assert_eq!(trimmed.len(), 5);
    // Oldest user image stripped
    let (_, refs0) = parse_image_markers(&trimmed[0].content);
    assert!(refs0.is_empty());
    assert!(trimmed[0].content.contains("Look at this"));
    // Assistant messages untouched
    assert_eq!(trimmed[1].content, "I see a photo.");
    assert_eq!(trimmed[3].content, "That's a chart.");
    // Two newest user images kept
    let (_, refs2) = parse_image_markers(&trimmed[2].content);
    assert_eq!(refs2.len(), 1);
    let (_, refs4) = parse_image_markers(&trimmed[4].content);
    assert_eq!(refs4.len(), 1);
}

#[test]
fn trim_old_images_strips_multiple_oldest_messages() {
    // 5 user images, max 1 — should strip the first 4 messages' images.
    let messages: Vec<ChatMessage> = (1..=5)
        .map(|i| ChatMessage::user(format!("[IMAGE:/tmp/{i}.png]\nCaption {i}")))
        .collect();

    let trimmed = trim_old_images(&messages, 1);
    assert_eq!(trimmed.len(), 5);
    for (i, msg) in trimmed.iter().enumerate().take(4) {
        let (_, refs) = parse_image_markers(&msg.content);
        assert!(refs.is_empty(), "message {i} should have images stripped");
        assert!(msg.content.contains(&format!("Caption {}", i + 1)));
    }
    // Only the last message keeps its image
    let (_, refs_last) = parse_image_markers(&trimmed[4].content);
    assert_eq!(refs_last.len(), 1);
}

#[tokio::test]
async fn prepare_messages_trims_then_normalizes_surviving_images() {
    // End-to-end: 3 images, max 2. After trimming the oldest, the two
    // surviving images should be normalized (base64-encoded) successfully.
    let temp = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for name in ["old.png", "mid.png", "new.png"] {
        let p = temp.path().join(name);
        // Minimal valid PNG (1x1 white pixel)
        let png_data = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
            0x77, 0x53, 0xDE, // 1x1 RGB
            0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, // IDAT chunk
            0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xE2, 0x21,
            0xBC, 0x33, // IDAT data + CRC
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, // IEND chunk
            0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(&p, png_data).unwrap();
        paths.push(p);
    }

    let messages = vec![
        ChatMessage::user(format!("[IMAGE:{}]\nOld", paths[0].display().to_string())),
        ChatMessage::user(format!("[IMAGE:{}]\nMid", paths[1].display().to_string())),
        ChatMessage::user(format!("[IMAGE:{}]\nNew", paths[2].display().to_string())),
    ];

    let config = MultimodalConfig {
        max_images: 2,
        max_image_size_mb: 5,
        allow_remote_fetch: false,
        ..Default::default()
    };

    let result = prepare_messages_for_provider(&messages, &config)
        .await
        .expect("should succeed after trimming");

    assert!(result.contains_images);
    assert_eq!(result.messages.len(), 3);
    // First message should have image stripped, text preserved
    assert!(!result.messages[0].content.contains("data:image"));
    assert!(result.messages[0].content.contains("Old"));
    // Second and third should have base64-encoded images
    assert!(result.messages[1].content.contains("data:image"));
    assert!(result.messages[2].content.contains("data:image"));
}

#[tokio::test]
async fn prepare_messages_caps_a_single_multi_image_message_to_newest_two() {
    // End-to-end boundary: one message carries three distinct local images
    // and the declared budget is two. Normalization inlines all three, then
    // the cap must drop only the oldest marker and leave the newest two as
    // normalized data URIs alongside the unrelated text.
    let temp = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    let mut expected_uris = Vec::new();
    for (index, name) in ["a.png", "b.png", "c.png"].iter().enumerate() {
        let path = temp.path().join(name);
        let bytes = [
            0x89,
            b'P',
            b'N',
            b'G',
            b'\r',
            b'\n',
            0x1a,
            b'\n',
            index as u8,
        ];
        std::fs::write(&path, bytes).unwrap();
        expected_uris.push(format!("data:image/png;base64,{}", STANDARD.encode(bytes)));
        paths.push(path);
    }

    let messages = vec![ChatMessage::user(format!(
        "[IMAGE:{}]\n[IMAGE:{}]\n[IMAGE:{}]\nThree pics",
        paths[0].display(),
        paths[1].display(),
        paths[2].display()
    ))];

    let config = MultimodalConfig {
        max_images: 2,
        max_image_size_mb: 5,
        max_image_turns: 0, // isolate the count cap from age trimming
        ..Default::default()
    };

    let result = prepare_messages_for_provider(&messages, &config)
        .await
        .expect("preparation should cap a single over-budget message");

    assert!(result.contains_images);
    assert_eq!(result.messages.len(), 1);
    let (cleaned, refs) = parse_image_markers(&result.messages[0].content);
    assert_eq!(refs.len(), 2, "exactly the declared budget survives");
    assert_eq!(
        refs,
        expected_uris[1..],
        "the newest two images survive in order"
    );
    assert!(cleaned.contains("Three pics"), "text survives the cap");
}

#[tokio::test]
async fn prepare_messages_caps_to_newest_successful_images() {
    let temp = tempfile::tempdir().unwrap();
    // Minimal valid PNG (1x1 RGB pixel).
    let png_data = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xE2, 0x21, 0xBC, 0x33, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    // Nine distinct valid image files across nine user messages, max 4.
    let mut messages = Vec::new();
    for i in 0..9 {
        let p = temp.path().join(format!("img{i}.png"));
        std::fs::write(&p, png_data).unwrap();
        messages.push(ChatMessage::user(format!(
            "[IMAGE:{}]\nImage {i}",
            p.display()
        )));
    }

    let config = MultimodalConfig {
        max_images: 4,
        max_image_size_mb: 5,
        allow_remote_fetch: false,
        max_image_turns: 0, // disable age-based trimming to isolate the cap
        ..Default::default()
    };

    let result = prepare_messages_for_provider(&messages, &config)
        .await
        .expect("should succeed");

    // Output is capped to exactly max_images...
    let surviving = result
        .messages
        .iter()
        .filter(|m| m.content.contains("data:image"))
        .count();
    assert_eq!(surviving, 4, "output should keep exactly max_images");

    // ...and it is the newest four that survive; the oldest five are stripped.
    for (i, m) in result.messages.iter().enumerate() {
        if i < 5 {
            assert!(
                !m.content.contains("data:image"),
                "oldest message {i} should be capped out"
            );
            assert!(m.content.contains(&format!("Image {i}")));
        } else {
            assert!(
                m.content.contains("data:image"),
                "newest message {i} should survive the cap"
            );
        }
    }
}

#[tokio::test]
async fn prepare_messages_skips_remote_url_when_disabled() {
    let messages = vec![ChatMessage::user(
        "Look [IMAGE:https://example.com/img.png]".to_string(),
    )];

    let result = prepare_messages_for_provider(&messages, &MultimodalConfig::default())
        .await
        .expect("disabled remote image should be skipped");

    assert!(!result.contains_images);
    assert_eq!(result.messages.len(), 1);
    assert!(result.messages[0].content.contains("Look"));
    assert!(
        result.messages[0]
            .content
            .contains("1 attached image(s) could not be loaded")
    );
    assert!(
        !result.messages[0]
            .content
            .contains("https://example.com/img.png")
    );
}

#[tokio::test]
async fn prepare_messages_skips_oversized_local_image() {
    let temp = tempfile::tempdir().unwrap();
    let image_path = temp.path().join("big.png");

    let bytes = vec![0u8; 1024 * 1024 + 1];
    std::fs::write(&image_path, bytes).unwrap();

    let messages = vec![ChatMessage::user(format!(
        "[IMAGE:{}]",
        image_path.display()
    ))];
    let config = MultimodalConfig {
        max_images: 4,
        max_image_size_mb: 1,
        allow_remote_fetch: false,
        ..Default::default()
    };

    let result = prepare_messages_for_provider(&messages, &config)
        .await
        .expect("oversized local image should be skipped");

    assert!(!result.contains_images);
    assert_eq!(result.messages.len(), 1);
    assert!(
        result.messages[0]
            .content
            .contains("1 attached image(s) could not be loaded")
    );
    assert!(
        !result.messages[0]
            .content
            .contains(image_path.to_string_lossy().as_ref())
    );
}

#[tokio::test]
async fn prepare_messages_keeps_successful_images_when_some_are_skipped() {
    let temp = tempfile::tempdir().unwrap();
    let image_path = temp.path().join("ok.png");
    std::fs::write(
        &image_path,
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'],
    )
    .unwrap();

    let messages = vec![ChatMessage::user(format!(
        "Look [IMAGE:{}] and [IMAGE:https://example.com/missing.png]",
        image_path.display()
    ))];

    let result = prepare_messages_for_provider(&messages, &MultimodalConfig::default())
        .await
        .expect("valid local image should survive while remote image is skipped");

    assert!(result.contains_images);
    assert!(
        result.messages[0]
            .content
            .contains("data:image/png;base64,")
    );
    assert!(
        result.messages[0]
            .content
            .contains("1 of 2 attached image(s) could not be loaded")
    );
    assert!(
        !result.messages[0]
            .content
            .contains("https://example.com/missing.png")
    );
}

#[tokio::test]
async fn skipped_images_do_not_consume_image_budget() {
    let temp = tempfile::tempdir().unwrap();
    let image_path = temp.path().join("older-valid.png");
    std::fs::write(
        &image_path,
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'],
    )
    .unwrap();

    let messages = vec![
        ChatMessage::user(format!(
            "Older valid image [IMAGE:{}]",
            image_path.display()
        )),
        ChatMessage::user("Newer broken image [IMAGE:https://example.com/missing.png]".to_string()),
    ];
    let config = MultimodalConfig {
        max_images: 1,
        max_image_size_mb: 5,
        allow_remote_fetch: false,
        ..Default::default()
    };

    let result = prepare_messages_for_provider(&messages, &config)
        .await
        .expect("broken image should not evict an older valid image");

    assert!(result.contains_images);
    assert!(
        result.messages[0]
            .content
            .contains("data:image/png;base64,")
    );
    assert!(result.messages[1].content.contains("Newer broken image"));
    assert!(
        result.messages[1]
            .content
            .contains("1 attached image(s) could not be loaded")
    );
    assert!(
        !result.messages[1]
            .content
            .contains("https://example.com/missing.png")
    );
}

#[test]
fn extract_ollama_image_payload_supports_data_uris() {
    let payload = extract_ollama_image_payload("data:image/png;base64,abcd==")
        .expect("payload should be extracted");
    assert_eq!(payload, "abcd==");
}

#[test]
fn parse_image_markers_strips_markers_leaving_caption() {
    let input = "[IMAGE:/tmp/photo.jpg]\n\nDescribe this screenshot";
    let (cleaned, refs) = parse_image_markers(input);
    assert_eq!(cleaned, "Describe this screenshot");
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0], "/tmp/photo.jpg");
}

#[test]
fn parse_image_markers_image_only_message_becomes_empty() {
    let input = "[IMAGE:/tmp/photo.jpg]";
    let (cleaned, refs) = parse_image_markers(input);
    assert!(
        cleaned.is_empty(),
        "expected empty string, got: {cleaned:?}"
    );
    assert_eq!(refs.len(), 1);
}
