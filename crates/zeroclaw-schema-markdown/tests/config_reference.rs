//! The real config schema rendered through the public presentation boundary.

use zeroclaw_schema_markdown::{field_table_for_path, generate};

#[test]
fn index_links_each_section_to_its_anchor() {
    let schema = schemars::schema_for!(zeroclaw_config::schema::Config);
    let md = generate(&schema.to_value());

    // Every section in the index table must be a link to the detail
    // heading below. mdBook slugs a `## `<key>`` heading to the bare key,
    // so the index cell links `[`<key>`](#<key>)`. A plain `` `<key>` ``
    // cell (no link) is the regression this guards against.
    let mut linked = 0usize;
    for line in md.lines() {
        // Index rows look like: | [`acp`](#acp) | ... |
        if let Some(rest) = line.strip_prefix("| [`") {
            let key = rest.split('`').next().unwrap_or("");
            assert!(
                line.contains(&format!("](#{key})")),
                "index row for `{key}` is not linked to its anchor: {line}"
            );
            // The matching detail heading must exist verbatim.
            assert!(
                md.contains(&format!("# `{key}`")),
                "no detail heading for indexed section `{key}`"
            );
            linked += 1;
        }
    }
    assert!(linked > 10, "expected many linked sections, got {linked}");
}

#[test]
fn config_fields_descends_array_section_to_entry_struct() {
    // `mcp.servers` is a Vec<McpServerConfig> (schemars `items`), not a map.
    // The path walker must step into the element struct so a `[[mcp.servers]]`
    // list section renders its entry fields instead of erroring with
    // "no fields to render".
    let schema = schemars::schema_for!(zeroclaw_config::schema::Config);
    let table = field_table_for_path(&schema.to_value(), "mcp.servers", false, None)
        .expect("mcp.servers should resolve to its entry struct fields");
    for field in [
        "transport",
        "command",
        "url",
        "headers",
        "tool_timeout_secs",
    ] {
        assert!(
            table.contains(&format!("<code>{field}</code>")),
            "mcp.servers field table missing `{field}`"
        );
    }
}

#[test]
fn docker_workspace_root_contract_appears_in_generated_reference() {
    let schema = schemars::schema_for!(zeroclaw_config::schema::Config);
    let md = generate(&schema.to_value());
    let row = md
        .lines()
        .find(|line| line.contains("`allowed_workspace_roots`"))
        .expect("generated config reference should include Docker workspace roots");

    assert!(row.contains("fail-closed"));
    assert!(row.contains("workspace must exist and canonicalize"));
    assert!(row.contains("every configured root must also exist and canonicalize"));
    assert!(row.contains("one invalid entry rejects the command"));
    assert!(row.contains("empty list permits any canonical workspace"));
}

#[test]
fn cron_section_exposes_uses_memory_field() {
    // Regression test: the generated config reference must expose
    // per-cron-job fields (including `uses_memory`) instead of only the
    // top-level `[cron]` summary. `cron` is a `HashMap<String, CronJobDecl>`
    // — the generator must descend into `additionalProperties` to render
    // the CronJobDecl fields.
    let schema = schemars::schema_for!(zeroclaw_config::schema::Config);
    let md = generate(&schema.to_value());

    // The cron section heading with <alias> must exist.
    assert!(
        md.contains("## `cron.<alias>`"),
        "cron section should have a `cron.<alias>` subheading"
    );

    // `uses_memory` must appear as a field in the cron section.
    assert!(
        md.contains("uses_memory"),
        "cron section should expose the `uses_memory` field"
    );

    // Other CronJobDecl fields should also be present.
    for field in [
        "name", "job_type", "schedule", "enabled", "prompt", "command",
    ] {
        assert!(
            md.contains(field),
            "cron section should expose the `{field}` field"
        );
    }
}
