#[cfg(test)]
use super::*;
use zeroclaw_config::autonomy::AutonomyLevel;
use zeroclaw_config::policy::SecurityPolicy;

fn test_security() -> Arc<SecurityPolicy> {
    Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    })
}

fn test_tool_with_base_url(
    base_url: String,
    email: Option<String>,
    api_token: &str,
    allowed_actions: Vec<&str>,
) -> JiraTool {
    JiraTool::new(
        base_url,
        email,
        api_token.into(),
        allowed_actions.into_iter().map(String::from).collect(),
        test_security(),
        30,
    )
}

fn test_tool(allowed_actions: Vec<&str>) -> JiraTool {
    test_tool_with_base_url(
        "https://test.atlassian.net".into(),
        Some("test@example.com".into()),
        "test-token",
        allowed_actions,
    )
}

fn test_tool_server(allowed_actions: Vec<&str>) -> JiraTool {
    test_tool_with_base_url(
        "https://internal-jira.company.com".into(),
        None,
        "pat-token-abc",
        allowed_actions,
    )
}

fn basic_auth_header(email: &str, token: &str) -> String {
    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"));
    format!("Basic {encoded}")
}

fn basic_search_issue(key: &str) -> Value {
    json!({
        "key": key,
        "fields": {
            "summary": "Fix bug",
            "status": { "name": "In Progress" },
            "priority": { "name": "High" },
            "assignee": { "displayName": "Jane" },
            "created": "2024-01-15T10:00:00.000Z",
            "updated": "2024-03-01T12:00:00.000Z"
        }
    })
}

// ── API version / auth mode tests ───────────────────────────────────────

#[test]
fn cloud_tool_uses_api_v3() {
    let tool = test_tool(vec!["get_ticket"]);
    assert_eq!(tool.api_version(), "3");
    assert!(tool.is_cloud());
}

#[test]
fn server_tool_uses_api_v2() {
    let tool = test_tool_server(vec!["get_ticket"]);
    assert_eq!(tool.api_version(), "2");
    assert!(!tool.is_cloud());
}

#[test]
fn tool_name_is_jira() {
    assert_eq!(test_tool(vec!["get_ticket"]).name(), "jira");
}

// ── Request shape tests ─────────────────────────────────────────────────

#[tokio::test]
async fn cloud_search_uses_basic_auth_v3_endpoint_and_next_page_token() {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let auth = basic_auth_header("test@example.com", "test-token");
    let fields = json!([
        "summary", "priority", "status", "assignee", "created", "updated"
    ]);

    let first_body = json!({
        "jql": "project = PROJ",
        "maxResults": 2,
        "fields": fields
    });
    Mock::given(method("POST"))
        .and(path("/rest/api/3/search/jql"))
        .and(header("authorization", auth.as_str()))
        .and(body_json(&first_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issues": [basic_search_issue("PROJ-1")],
            "isLast": false,
            "nextPageToken": "page-2"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let second_body = json!({
        "jql": "project = PROJ",
        "maxResults": 1,
        "fields": fields,
        "nextPageToken": "page-2"
    });
    Mock::given(method("POST"))
        .and(path("/rest/api/3/search/jql"))
        .and(header("authorization", auth.as_str()))
        .and(body_json(&second_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issues": [basic_search_issue("PROJ-2")],
            "isLast": true
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(
        server.uri(),
        Some("test@example.com".into()),
        "test-token",
        vec!["search_tickets"],
    );
    let result = tool
        .execute(json!({
            "action": "search_tickets",
            "jql": "project = PROJ",
            "max_results": 2
        }))
        .await
        .unwrap();

    assert!(result.success, "unexpected error: {:?}", result.error);
    let output: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output.as_array().unwrap().len(), 2);
    server.verify().await;
}

#[tokio::test]
async fn server_search_uses_bearer_auth_v2_endpoint_and_start_at() {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let fields = json!([
        "summary", "priority", "status", "assignee", "created", "updated"
    ]);

    let first_body = json!({
        "jql": "project = PROJ",
        "startAt": 0,
        "maxResults": 2,
        "fields": fields
    });
    Mock::given(method("POST"))
        .and(path("/rest/api/2/search"))
        .and(header("authorization", "Bearer pat-token-abc"))
        .and(body_json(&first_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issues": [basic_search_issue("PROJ-1")],
            "total": 2
        })))
        .expect(1)
        .mount(&server)
        .await;

    let second_body = json!({
        "jql": "project = PROJ",
        "startAt": 1,
        "maxResults": 1,
        "fields": fields
    });
    Mock::given(method("POST"))
        .and(path("/rest/api/2/search"))
        .and(header("authorization", "Bearer pat-token-abc"))
        .and(body_json(&second_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issues": [basic_search_issue("PROJ-2")],
            "total": 2
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(server.uri(), None, "pat-token-abc", vec!["search_tickets"]);
    let result = tool
        .execute(json!({
            "action": "search_tickets",
            "jql": "project = PROJ",
            "max_results": 2
        }))
        .await
        .unwrap();

    assert!(result.success, "unexpected error: {:?}", result.error);
    let output: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output.as_array().unwrap().len(), 2);
    server.verify().await;
}

#[tokio::test]
async fn cloud_comment_posts_adf_body_to_v3_endpoint() {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let comment = "This is **important**.\n- Check the logs";
    let expected_body = json!({ "body": build_adf(comment, &HashMap::new()) });
    let auth = basic_auth_header("test@example.com", "test-token");

    Mock::given(method("POST"))
        .and(path("/rest/api/3/issue/PROJ-1/comment"))
        .and(header("authorization", auth.as_str()))
        .and(body_json(&expected_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "10000",
            "author": { "displayName": "Jane" },
            "created": "2024-01-15T10:00:00.000Z"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(
        server.uri(),
        Some("test@example.com".into()),
        "test-token",
        vec!["comment_ticket"],
    );
    let result = tool
        .execute(json!({
            "action": "comment_ticket",
            "issue_key": "PROJ-1",
            "comment": comment
        }))
        .await
        .unwrap();

    assert!(result.success, "unexpected error: {:?}", result.error);
    server.verify().await;
}

#[tokio::test]
async fn server_comment_posts_plain_text_body_to_v2_endpoint() {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let comment = "Hi @john@company.com, this is **important**.\n- Check the logs";
    let expected_body = json!({ "body": comment });

    Mock::given(method("POST"))
        .and(path("/rest/api/2/issue/PROJ-1/comment"))
        .and(header("authorization", "Bearer pat-token-abc"))
        .and(body_json(&expected_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "10001",
            "author": { "displayName": "Jane" },
            "created": "2024-01-15T10:00:00.000Z"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(server.uri(), None, "pat-token-abc", vec!["comment_ticket"]);
    let result = tool
        .execute(json!({
            "action": "comment_ticket",
            "issue_key": "PROJ-1",
            "comment": comment
        }))
        .await
        .unwrap();

    assert!(result.success, "unexpected error: {:?}", result.error);
    server.verify().await;
}

#[test]
fn parameters_schema_has_required_action() {
    let schema = test_tool(vec!["get_ticket"]).parameters_schema();
    let required = schema["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("action")));
}

#[test]
fn parameters_schema_defines_all_actions() {
    let schema = test_tool(vec!["get_ticket"]).parameters_schema();
    let actions = schema["properties"]["action"]["enum"].as_array().unwrap();
    let action_strs: Vec<&str> = actions.iter().filter_map(|v| v.as_str()).collect();
    assert!(action_strs.contains(&"get_ticket"));
    assert!(action_strs.contains(&"search_tickets"));
    assert!(action_strs.contains(&"comment_ticket"));
}

#[test]
fn parameters_schema_describes_cloud_and_server_comment_modes() {
    let schema = test_tool(vec!["comment_ticket"]).parameters_schema();
    let description = schema["properties"]["comment"]["description"]
        .as_str()
        .unwrap();

    assert!(description.contains("Jira Cloud mode"));
    assert!(description.contains("Atlassian Document Format"));
    assert!(description.contains("Jira Server/Data Center mode"));
    assert!(description.contains("plain text"));
}

#[tokio::test]
async fn execute_missing_action_returns_error() {
    let result = test_tool(vec!["get_ticket"])
        .execute(json!({}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("action"));
}

#[tokio::test]
async fn execute_unknown_action_returns_error() {
    let result = test_tool(vec!["get_ticket"])
        .execute(json!({"action": "delete_ticket"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("Unknown action"));
}

#[tokio::test]
async fn execute_disallowed_action_returns_error() {
    let result = test_tool(vec!["get_ticket"])
        .execute(json!({"action": "comment_ticket"}))
        .await
        .unwrap();
    assert!(!result.success);
    let err = result.error.unwrap();
    assert!(err.contains("not enabled"));
    assert!(err.contains("allowed_actions"));
}

#[tokio::test]
async fn execute_get_ticket_missing_key_returns_error() {
    let result = test_tool(vec!["get_ticket"])
        .execute(json!({"action": "get_ticket"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("issue_key"));
}

#[tokio::test]
async fn execute_search_tickets_missing_jql_returns_error() {
    let result = test_tool(vec!["get_ticket", "search_tickets"])
        .execute(json!({"action": "search_tickets"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("jql"));
}

#[tokio::test]
async fn execute_comment_ticket_missing_key_returns_error() {
    let result = test_tool(vec!["get_ticket", "comment_ticket"])
        .execute(json!({"action": "comment_ticket", "comment": "hello"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("issue_key"));
}

#[tokio::test]
async fn execute_comment_ticket_missing_comment_returns_error() {
    let result = test_tool(vec!["get_ticket", "comment_ticket"])
        .execute(json!({"action": "comment_ticket", "issue_key": "PROJ-1"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("comment"));
}

#[tokio::test]
async fn execute_comment_ticket_empty_comment_returns_error() {
    let result = test_tool(vec!["get_ticket", "comment_ticket"])
        .execute(json!({"action": "comment_ticket", "issue_key": "PROJ-1", "comment": "   "}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("comment"));
}

#[tokio::test]
async fn execute_comment_blocked_in_readonly_mode() {
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    });
    let tool = JiraTool::new(
        "https://test.atlassian.net".into(),
        Some("test@example.com".into()),
        "token".into(),
        vec!["get_ticket".into(), "comment_ticket".into()],
        security,
        30,
    );
    let result = tool
        .execute(json!({
            "action": "comment_ticket",
            "issue_key": "PROJ-1",
            "comment": "hello"
        }))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("read-only"));
}

// ── myself action ────────────────────────────────────────────────────────

#[test]
fn parameters_schema_includes_myself_action() {
    let schema = test_tool(vec!["myself"]).parameters_schema();
    let actions = schema["properties"]["action"]["enum"].as_array().unwrap();
    let action_strs: Vec<&str> = actions.iter().filter_map(|v| v.as_str()).collect();
    assert!(action_strs.contains(&"myself"));
}

#[tokio::test]
async fn execute_myself_disallowed_returns_error() {
    let result = test_tool(vec!["get_ticket"])
        .execute(json!({"action": "myself"}))
        .await
        .unwrap();
    assert!(!result.success);
    let err = result.error.unwrap();
    assert!(err.contains("not enabled"));
    assert!(err.contains("allowed_actions"));
}

#[tokio::test]
async fn execute_myself_not_blocked_in_readonly_mode() {
    // myself is a Read operation — the security policy should not block it.
    // The call will fail at the HTTP level (no real server), not at the
    // policy level, so the error must NOT contain "read-only".
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    });
    let tool = JiraTool::new(
        "https://test.atlassian.net".into(),
        Some("test@example.com".into()),
        "token".into(),
        vec!["myself".into()],
        security,
        30,
    );
    let result = tool.execute(json!({"action": "myself"})).await.unwrap();
    assert!(!result.success);
    assert!(!result.error.as_deref().unwrap_or("").contains("read-only"));
}

// ── Issue key validation ──────────────────────────────────────────────────

#[test]
fn validate_issue_key_accepts_valid_keys() {
    assert!(validate_issue_key("PROJ-1").is_ok());
    assert!(validate_issue_key("PROJ-123").is_ok());
    assert!(validate_issue_key("AB-99").is_ok());
    assert!(validate_issue_key("MYPROJECT-1000").is_ok());
    assert!(validate_issue_key("proj-1").is_ok());
    assert!(validate_issue_key("proj-123").is_ok());
}

#[test]
fn validate_issue_key_rejects_path_traversal() {
    assert!(validate_issue_key("../../etc/passwd").is_err());
    assert!(validate_issue_key("../other").is_err());
}

#[test]
fn validate_issue_key_rejects_malformed() {
    assert!(validate_issue_key("PROJ").is_err()); // no number
    assert!(validate_issue_key("PROJ-").is_err()); // empty number
    assert!(validate_issue_key("-123").is_err()); // no project
    assert!(validate_issue_key("PROJ-12x").is_err()); // non-digit in number
}

// ── ADF builder unit tests ────────────────────────────────────────────────

#[test]
fn build_adf_plain_text() {
    let adf = build_adf("Hello world", &HashMap::new());
    assert_eq!(adf["type"], "doc");
    assert_eq!(adf["version"], 1);
    let para = &adf["content"][0];
    assert_eq!(para["type"], "paragraph");
    assert_eq!(para["content"][0]["text"], "Hello world");
}

#[test]
fn build_adf_bold() {
    let adf = build_adf("**bold**", &HashMap::new());
    let text_node = &adf["content"][0]["content"][0];
    assert_eq!(text_node["text"], "bold");
    assert_eq!(text_node["marks"][0]["type"], "strong");
}

#[test]
fn build_adf_unmatched_bold_is_literal() {
    let adf = build_adf("**no closing", &HashMap::new());
    let text = &adf["content"][0]["content"][0]["text"];
    assert!(text.as_str().unwrap().contains("**no closing"));
}

#[test]
fn build_adf_bullet_list() {
    let adf = build_adf("- first\n- second", &HashMap::new());
    let list = &adf["content"][0];
    assert_eq!(list["type"], "bulletList");
    assert_eq!(list["content"].as_array().unwrap().len(), 2);
    assert_eq!(list["content"][0]["type"], "listItem");
}

#[test]
fn build_adf_mention_resolved() {
    let mut mentions = HashMap::new();
    mentions.insert(
        "john@company.com".to_string(),
        ("acc-123".to_string(), "John Doe".to_string()),
    );
    let adf = build_adf("Hi @john@company.com done", &mentions);
    let content = &adf["content"][0]["content"];
    let mention = content
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["type"] == "mention")
        .unwrap();
    assert_eq!(mention["attrs"]["id"], "acc-123");
    assert_eq!(mention["attrs"]["text"], "@John Doe");
}

#[test]
fn build_adf_unresolved_mention_rendered_as_plain_text() {
    let adf = build_adf("Hi @unknown@example.com", &HashMap::new());
    let text = &adf["content"][0]["content"][0]["text"];
    assert!(text.as_str().unwrap().contains("@unknown@example.com"));
}

#[test]
fn extract_emails_finds_at_prefixed_emails() {
    let emails = extract_emails("Hello @john@company.com and @jane@corp.io done");
    assert_eq!(emails, vec!["john@company.com", "jane@corp.io"]);
}

#[test]
fn extract_emails_deduplicates() {
    let emails = extract_emails("@a@b.com @a@b.com");
    assert_eq!(emails.len(), 1);
}

#[test]
fn extract_emails_deduplicates_non_adjacent() {
    let emails = extract_emails("@a@b.com @c@d.com @a@b.com");
    assert_eq!(emails, vec!["a@b.com", "c@d.com"]);
}

#[test]
fn extract_emails_strips_trailing_punctuation() {
    let emails = extract_emails("@john@company.com,");
    assert_eq!(emails, vec!["john@company.com"]);
}

#[test]
fn extract_emails_strips_leading_punctuation() {
    let emails = extract_emails("@(john@company.com)");
    assert_eq!(emails, vec!["john@company.com"]);
}

#[test]
fn shape_basic_search_extracts_expected_fields() {
    let raw = json!({
        "key": "PROJ-1",
        "fields": {
            "summary": "Fix bug",
            "status": { "name": "In Progress" },
            "priority": { "name": "High" },
            "assignee": { "displayName": "Jane" },
            "created": "2024-01-15T10:00:00.000Z",
            "updated": "2024-03-01T12:00:00.000Z"
        }
    });
    let shaped = shape_basic_search(&raw);
    assert_eq!(shaped["key"], "PROJ-1");
    assert_eq!(shaped["summary"], "Fix bug");
    assert_eq!(shaped["status"], "In Progress");
    assert_eq!(shaped["priority"], "High");
    assert_eq!(shaped["assignee"], "Jane");
    assert_eq!(shaped["created"], "2024-01-15");
    assert_eq!(shaped["updated"], "2024-03-01");
}

#[test]
fn shape_changelog_extracts_key_and_changelog() {
    let raw = json!({
        "key": "PROJ-42",
        "changelog": { "histories": [] },
        "fields": {}
    });
    let shaped = shape_changelog(&raw);
    assert_eq!(shaped["key"], "PROJ-42");
    assert!(shaped.get("changelog").is_some());
    assert!(shaped.get("fields").is_none());
}

#[test]
fn shape_comment_response_extracts_id_author_created() {
    let raw = json!({
        "id": "12345",
        "author": { "displayName": "Alice", "accountId": "abc" },
        "created": "2024-06-01T09:00:00.000Z",
        "body": { "type": "doc" },
        "self": "https://internal.url"
    });
    let shaped = shape_comment_response(&raw);
    assert_eq!(shaped["id"], "12345");
    assert_eq!(shaped["author"], "Alice");
    assert_eq!(shaped["created"], "2024-06-01");
    assert!(shaped.get("body").is_none());
    assert!(shaped.get("self").is_none());
}

// ── date_prefix helper ─────────────────────────────────────────────────

#[test]
fn date_prefix_normal_date_string() {
    assert_eq!(date_prefix("2024-01-15T10:00:00.000Z"), "2024-01-15");
}

#[test]
fn date_prefix_empty_string() {
    assert_eq!(date_prefix(""), "");
}

#[test]
fn date_prefix_short_string() {
    assert_eq!(date_prefix("2024"), "2024");
}

#[test]
fn date_prefix_exactly_ten_chars() {
    assert_eq!(date_prefix("2024-01-15"), "2024-01-15");
}

#[test]
fn shape_basic_uses_o1_comment_lookup() {
    // Verify that comments are matched by ID, not by position.
    let raw = json!({
        "key": "PROJ-1",
        "fields": {
            "summary": "s", "priority": {"name":"P"}, "status": {"name":"S"},
            "assignee": {"displayName":"A"},
            "created": "2024-01-01T00:00:00.000Z",
            "updated": "2024-01-01T00:00:00.000Z",
            "comment": {
                "comments": [
                    { "id": "2", "author": {"displayName":"Bob"}, "created": "2024-01-02T00:00:00.000Z" },
                    { "id": "1", "author": {"displayName":"Alice"}, "created": "2024-01-01T00:00:00.000Z" }
                ]
            }
        },
        "renderedFields": {
            "description": "",
            "comment": {
                "comments": [
                    { "id": "1", "body": "Alice's body" },
                    { "id": "2", "body": "Bob's body" }
                ]
            }
        }
    });
    let shaped = shape_basic(&raw);
    // Comment with id "2" (Bob) should get Bob's rendered body, not Alice's
    assert_eq!(shaped["comments"][0]["author"], "Bob");
    assert_eq!(shaped["comments"][0]["body"], "Bob's body");
    assert_eq!(shaped["comments"][1]["author"], "Alice");
    assert_eq!(shaped["comments"][1]["body"], "Alice's body");
}

// ── list_projects action ────────────────────────────────────────────────

#[test]
fn parameters_schema_includes_list_projects_action() {
    let schema = test_tool(vec!["list_projects"]).parameters_schema();
    let actions = schema["properties"]["action"]["enum"].as_array().unwrap();
    let action_strs: Vec<&str> = actions.iter().filter_map(|v| v.as_str()).collect();
    assert!(action_strs.contains(&"list_projects"));
}

#[tokio::test]
async fn execute_list_projects_disallowed_returns_error() {
    let result = test_tool(vec!["get_ticket"])
        .execute(json!({"action": "list_projects"}))
        .await
        .unwrap();
    assert!(!result.success);
    let err = result.error.unwrap();
    assert!(err.contains("not enabled"));
    assert!(err.contains("allowed_actions"));
}

#[tokio::test]
async fn execute_list_projects_not_blocked_in_readonly_mode() {
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    });
    let tool = JiraTool::new(
        "https://127.0.0.1:1".into(),
        Some("test@example.com".into()),
        "token".into(),
        vec!["list_projects".into()],
        security,
        30,
    );
    let result = tool
        .execute(json!({"action": "list_projects"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(
        !result.error.as_deref().unwrap_or("").contains("read-only"),
        "error should not mention read-only policy: {:?}",
        result.error
    );
}

#[test]
fn shape_projects_extracts_expected_fields() {
    let projects = json!([
        { "key": "AT", "name": "ALL TASKS", "projectTypeKey": "business", "style": "next-gen" },
        { "key": "GP", "name": "G-PROJECT", "projectTypeKey": "software", "style": "next-gen" }
    ]);
    let statuses: Vec<Value> = vec![
        json!([
            { "name": "Task", "statuses": [
                { "name": "To Do" }, { "name": "In Progress" }, { "name": "Collecting Intel" }, { "name": "Done" }
            ]},
            { "name": "Sub-task", "statuses": [
                { "name": "To Do" }, { "name": "Verification" }
            ]}
        ]),
        json!([
            { "name": "Task", "statuses": [
                { "name": "To Do" }, { "name": "Design" }, { "name": "Done" }
            ]},
            { "name": "Epic", "statuses": [
                { "name": "To Do" }, { "name": "Done" }
            ]}
        ]),
    ];
    let shaped = shape_projects(projects.as_array().unwrap(), &statuses);
    let arr = &shaped;

    assert_eq!(arr.len(), 2);

    assert_eq!(arr[0]["key"], "AT");
    assert_eq!(arr[0]["name"], "ALL TASKS");
    assert_eq!(arr[0]["projectType"], "business");
    let at_statuses: Vec<&str> = arr[0]["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        at_statuses,
        vec![
            "Collecting Intel",
            "Done",
            "In Progress",
            "To Do",
            "Verification",
        ]
    );
    let at_types: Vec<&str> = arr[0]["issueTypes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(at_types.contains(&"Task"));
    assert!(at_types.contains(&"Sub-task"));

    assert_eq!(arr[1]["key"], "GP");
    assert_eq!(arr[1]["projectType"], "software");
    let gp_statuses: Vec<&str> = arr[1]["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(gp_statuses, vec!["Design", "Done", "To Do"]);

    assert!(
        arr[0].get("users").is_none(),
        "users should not be in per-project data"
    );
}

#[test]
fn shape_projects_sorts_statuses_alphabetically() {
    let projects = json!([
        { "key": "P", "name": "P", "projectTypeKey": "software", "style": "next-gen" }
    ]);
    let statuses: Vec<Value> = vec![json!([
        { "name": "Task", "statuses": [
            { "name": "Done" }, { "name": "Custom" }, { "name": "To Do" }, { "name": "Alpha" }
        ]}
    ])];
    let shaped = shape_projects(projects.as_array().unwrap(), &statuses);
    let ordered: Vec<&str> = shaped[0]["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(ordered, vec!["Alpha", "Custom", "Done", "To Do"]);
}

#[test]
fn shape_projects_empty_inputs() {
    let shaped = shape_projects(&[], &[]);
    assert_eq!(shaped.len(), 0);
}

// ── list_transitions / transition_ticket / create_ticket ─────────────────

#[test]
fn parameters_schema_includes_new_actions() {
    let schema = test_tool(vec!["get_ticket"]).parameters_schema();
    let actions = schema["properties"]["action"]["enum"].as_array().unwrap();
    let action_strs: Vec<&str> = actions.iter().filter_map(|v| v.as_str()).collect();
    assert!(action_strs.contains(&"list_transitions"));
    assert!(action_strs.contains(&"transition_ticket"));
    assert!(action_strs.contains(&"create_ticket"));
}

#[test]
fn parameters_schema_describes_transition_params() {
    let schema = test_tool(vec!["transition_ticket"]).parameters_schema();
    let props = &schema["properties"];
    assert!(props["transition_id"].is_object());
    assert!(props["transition_name"].is_object());
}

#[test]
fn parameters_schema_describes_create_params() {
    let schema = test_tool(vec!["create_ticket"]).parameters_schema();
    let props = &schema["properties"];
    for key in [
        "project_key",
        "issue_type",
        "summary",
        "description",
        "assignee",
        "labels",
        "parent_key",
    ] {
        assert!(props[key].is_object(), "missing schema property: {key}");
    }
}

#[tokio::test]
async fn execute_list_transitions_disallowed_returns_error() {
    let result = test_tool(vec!["get_ticket"])
        .execute(json!({"action": "list_transitions", "issue_key": "PROJ-1"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("not enabled"));
}

#[tokio::test]
async fn execute_transition_ticket_blocked_in_readonly_mode() {
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    });
    let tool = JiraTool::new(
        "https://test.atlassian.net".into(),
        Some("test@example.com".into()),
        "token".into(),
        vec!["transition_ticket".into()],
        security,
        30,
    );
    let result = tool
        .execute(json!({
            "action": "transition_ticket",
            "issue_key": "PROJ-1",
            "transition_id": "31"
        }))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("read-only"));
}

#[tokio::test]
async fn execute_create_ticket_blocked_in_readonly_mode() {
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    });
    let tool = JiraTool::new(
        "https://test.atlassian.net".into(),
        Some("test@example.com".into()),
        "token".into(),
        vec!["create_ticket".into()],
        security,
        30,
    );
    let result = tool
        .execute(json!({
            "action": "create_ticket",
            "project_key": "PROJ",
            "issue_type": "Task",
            "summary": "test"
        }))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("read-only"));
}

#[tokio::test]
async fn execute_list_transitions_not_blocked_in_readonly_mode() {
    let security = Arc::new(SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    });
    let tool = JiraTool::new(
        "https://127.0.0.1:1".into(),
        Some("test@example.com".into()),
        "token".into(),
        vec!["list_transitions".into()],
        security,
        30,
    );
    let result = tool
        .execute(json!({"action": "list_transitions", "issue_key": "PROJ-1"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(
        !result.error.as_deref().unwrap_or("").contains("read-only"),
        "list_transitions should be a Read op, but error mentioned read-only: {:?}",
        result.error
    );
}

#[tokio::test]
async fn execute_list_transitions_missing_key_returns_error() {
    let result = test_tool(vec!["list_transitions"])
        .execute(json!({"action": "list_transitions"}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("issue_key"));
}

#[tokio::test]
async fn execute_transition_ticket_missing_id_and_name_returns_error() {
    let result = test_tool(vec!["transition_ticket"])
        .execute(json!({"action": "transition_ticket", "issue_key": "PROJ-1"}))
        .await
        .unwrap();
    assert!(!result.success);
    let err = result.error.unwrap();
    assert!(err.contains("transition_id") && err.contains("transition_name"));
}

#[tokio::test]
async fn execute_transition_ticket_both_id_and_name_returns_error() {
    let result = test_tool(vec!["transition_ticket"])
        .execute(json!({
            "action": "transition_ticket",
            "issue_key": "PROJ-1",
            "transition_id": "31",
            "transition_name": "In Progress"
        }))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("only one"));
}

#[tokio::test]
async fn execute_create_ticket_missing_required_fields_returns_error() {
    let tool = test_tool(vec!["create_ticket"]);
    // Missing project_key
    let r1 = tool
        .execute(json!({
            "action": "create_ticket",
            "issue_type": "Task",
            "summary": "x"
        }))
        .await
        .unwrap();
    assert!(!r1.success);
    assert!(r1.error.as_deref().unwrap().contains("project_key"));
    // Missing issue_type
    let r2 = tool
        .execute(json!({
            "action": "create_ticket",
            "project_key": "PROJ",
            "summary": "x"
        }))
        .await
        .unwrap();
    assert!(!r2.success);
    assert!(r2.error.as_deref().unwrap().contains("issue_type"));
    // Missing summary
    let r3 = tool
        .execute(json!({
            "action": "create_ticket",
            "project_key": "PROJ",
            "issue_type": "Task"
        }))
        .await
        .unwrap();
    assert!(!r3.success);
    assert!(r3.error.as_deref().unwrap().contains("summary"));
}

#[tokio::test]
async fn cloud_list_transitions_returns_shaped_response() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let auth = basic_auth_header("test@example.com", "test-token");

    Mock::given(method("GET"))
        .and(path("/rest/api/3/issue/PROJ-1/transitions"))
        .and(header("authorization", auth.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "transitions": [
                { "id": "11", "name": "To Do",       "to": { "name": "To Do" }, "isAvailable": true },
                { "id": "21", "name": "In Progress", "to": { "name": "In Progress" } },
                { "id": "31", "name": "Done",        "to": { "name": "Done" } }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(
        server.uri(),
        Some("test@example.com".into()),
        "test-token",
        vec!["list_transitions"],
    );
    let result = tool
        .execute(json!({"action": "list_transitions", "issue_key": "PROJ-1"}))
        .await
        .unwrap();
    assert!(result.success, "unexpected error: {:?}", result.error);
    let output: Value = serde_json::from_str(&result.output).unwrap();
    let arr = output["transitions"].as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[1]["id"], "21");
    assert_eq!(arr[1]["name"], "In Progress");
    assert_eq!(arr[1]["to_status"], "In Progress");
    // Verbose Jira fields are dropped.
    assert!(arr[0].get("isAvailable").is_none());
    server.verify().await;
}

#[tokio::test]
async fn cloud_transition_ticket_by_id_posts_expected_body() {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let auth = basic_auth_header("test@example.com", "test-token");
    let body = json!({ "transition": { "id": "31" } });

    Mock::given(method("POST"))
        .and(path("/rest/api/3/issue/PROJ-1/transitions"))
        .and(header("authorization", auth.as_str()))
        .and(body_json(&body))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(
        server.uri(),
        Some("test@example.com".into()),
        "test-token",
        vec!["transition_ticket"],
    );
    let result = tool
        .execute(json!({
            "action": "transition_ticket",
            "issue_key": "PROJ-1",
            "transition_id": "31"
        }))
        .await
        .unwrap();
    assert!(result.success, "unexpected error: {:?}", result.error);
    let output: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["ok"], true);
    assert_eq!(output["transition_id"], "31");
    assert_eq!(output["issue_key"], "PROJ-1");
    server.verify().await;
}

#[tokio::test]
async fn server_transition_ticket_by_name_resolves_then_posts_to_v2() {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/rest/api/2/issue/PROJ-7/transitions"))
        .and(header("authorization", "Bearer pat-token-abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "transitions": [
                { "id": "21", "name": "In Progress", "to": { "name": "In Progress" } },
                { "id": "31", "name": "Done", "to": { "name": "Done" } }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let post_body = json!({ "transition": { "id": "21" } });
    Mock::given(method("POST"))
        .and(path("/rest/api/2/issue/PROJ-7/transitions"))
        .and(header("authorization", "Bearer pat-token-abc"))
        .and(body_json(&post_body))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(
        server.uri(),
        None,
        "pat-token-abc",
        vec!["transition_ticket"],
    );
    let result = tool
        .execute(json!({
            "action": "transition_ticket",
            "issue_key": "PROJ-7",
            "transition_name": "in progress"
        }))
        .await
        .unwrap();
    assert!(result.success, "unexpected error: {:?}", result.error);
    let output: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["transition_id"], "21");
    server.verify().await;
}

#[tokio::test]
async fn transition_ticket_unknown_name_returns_error_with_available() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/rest/api/3/issue/PROJ-1/transitions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "transitions": [
                { "id": "21", "name": "In Progress", "to": { "name": "In Progress" } },
                { "id": "31", "name": "Done", "to": { "name": "Done" } }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    // No POST mock — if the tool tried to POST, the test would fail with
    // an unmocked request error from wiremock's verify().
    let tool = test_tool_with_base_url(
        server.uri(),
        Some("test@example.com".into()),
        "test-token",
        vec!["transition_ticket"],
    );
    let result = tool
        .execute(json!({
            "action": "transition_ticket",
            "issue_key": "PROJ-1",
            "transition_name": "Reticulate Splines"
        }))
        .await
        .unwrap();
    assert!(!result.success);
    let err = result.error.unwrap();
    assert!(err.contains("Reticulate Splines"));
    assert!(err.contains("In Progress"));
    assert!(err.contains("Done"));
    server.verify().await;
}

#[tokio::test]
async fn cloud_create_ticket_minimal_posts_expected_body() {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let auth = basic_auth_header("test@example.com", "test-token");
    let expected = json!({
        "fields": {
            "project":   { "key": "PROJ" },
            "issuetype": { "name": "Task" },
            "summary":   "My new task"
        }
    });

    Mock::given(method("POST"))
        .and(path("/rest/api/3/issue"))
        .and(header("authorization", auth.as_str()))
        .and(body_json(&expected))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id":   "10042",
            "key":  "PROJ-99",
            "self": "https://test.atlassian.net/rest/api/3/issue/10042"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(
        server.uri(),
        Some("test@example.com".into()),
        "test-token",
        vec!["create_ticket"],
    );
    let result = tool
        .execute(json!({
            "action": "create_ticket",
            "project_key": "PROJ",
            "issue_type": "Task",
            "summary": "My new task"
        }))
        .await
        .unwrap();
    assert!(result.success, "unexpected error: {:?}", result.error);
    let output: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["key"], "PROJ-99");
    assert_eq!(output["id"], "10042");
    assert_eq!(
        output["browse_url"].as_str().unwrap(),
        format!("{}/browse/PROJ-99", server.uri())
    );
    server.verify().await;
}

#[tokio::test]
async fn cloud_create_ticket_with_description_uses_adf() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/rest/api/3/issue"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "1", "key": "PROJ-1", "self": "x"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(
        server.uri(),
        Some("test@example.com".into()),
        "test-token",
        vec!["create_ticket"],
    );
    tool.execute(json!({
        "action": "create_ticket",
        "project_key": "PROJ",
        "issue_type": "Task",
        "summary": "s",
        "description": "**bold** body"
    }))
    .await
    .unwrap();

    let received = &server.received_requests().await.unwrap();
    let req: &Request = received.last().unwrap();
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    let desc = &body["fields"]["description"];
    assert_eq!(desc["type"], "doc", "description must be ADF in Cloud mode");
    assert_eq!(desc["version"], 1);
}

#[tokio::test]
async fn server_create_ticket_with_description_uses_plain_string() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/rest/api/2/issue"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "1", "key": "PROJ-1", "self": "x"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(server.uri(), None, "pat-token-abc", vec!["create_ticket"]);
    tool.execute(json!({
        "action": "create_ticket",
        "project_key": "PROJ",
        "issue_type": "Task",
        "summary": "s",
        "description": "plain text"
    }))
    .await
    .unwrap();

    let received = &server.received_requests().await.unwrap();
    let req: &Request = received.last().unwrap();
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(
        body["fields"]["description"], "plain text",
        "description must be a plain string in Server mode"
    );
}

#[tokio::test]
async fn cloud_create_ticket_with_assignee_uses_account_id() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rest/api/3/issue"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "1", "key": "PROJ-1", "self": "x"
        })))
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(
        server.uri(),
        Some("test@example.com".into()),
        "test-token",
        vec!["create_ticket"],
    );
    tool.execute(json!({
        "action": "create_ticket",
        "project_key": "PROJ",
        "issue_type": "Task",
        "summary": "s",
        "assignee": "acc-123"
    }))
    .await
    .unwrap();

    let req: Request = server
        .received_requests()
        .await
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["fields"]["assignee"]["accountId"], "acc-123");
    assert!(body["fields"]["assignee"].get("name").is_none());
}

#[tokio::test]
async fn server_create_ticket_with_assignee_uses_username() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rest/api/2/issue"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "1", "key": "PROJ-1", "self": "x"
        })))
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(server.uri(), None, "pat-token-abc", vec!["create_ticket"]);
    tool.execute(json!({
        "action": "create_ticket",
        "project_key": "PROJ",
        "issue_type": "Task",
        "summary": "s",
        "assignee": "jdoe"
    }))
    .await
    .unwrap();

    let req: Request = server
        .received_requests()
        .await
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["fields"]["assignee"]["name"], "jdoe");
    assert!(body["fields"]["assignee"].get("accountId").is_none());
}

#[tokio::test]
async fn cloud_create_ticket_jira_error_surfaces_body() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rest/api/3/issue"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string(r#"{"errors":{"customfield_12345":"Field is required"}}"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let tool = test_tool_with_base_url(
        server.uri(),
        Some("test@example.com".into()),
        "test-token",
        vec!["create_ticket"],
    );
    let result = tool
        .execute(json!({
            "action": "create_ticket",
            "project_key": "PROJ",
            "issue_type": "Task",
            "summary": "s"
        }))
        .await
        .unwrap();
    assert!(!result.success);
    let err = result.error.unwrap();
    assert!(err.contains("400"));
    assert!(err.contains("customfield_12345"));
    server.verify().await;
}

#[test]
fn validate_project_key_accepts_valid_keys() {
    assert!(validate_project_key("PROJ").is_ok());
    assert!(validate_project_key("ABC123").is_ok());
    assert!(validate_project_key("p1").is_ok());
}

#[test]
fn validate_project_key_rejects_invalid_keys() {
    assert!(validate_project_key("").is_err());
    assert!(validate_project_key("PROJ-1").is_err());
    assert!(validate_project_key("../etc").is_err());
    assert!(validate_project_key("PROJ ABC").is_err());
}

#[test]
fn shape_transitions_extracts_minimal_fields() {
    let raw = json!({
        "transitions": [
            {
                "id": "11", "name": "To Do",
                "to": { "name": "To Do", "id": "10000", "self": "https://x" },
                "isAvailable": true
            },
            {
                "id": "21", "name": "In Progress",
                "to": { "name": "In Progress" }
            }
        ]
    });
    let shaped = shape_transitions(&raw);
    assert_eq!(shaped.len(), 2);
    assert_eq!(shaped[0]["id"], "11");
    assert_eq!(shaped[0]["name"], "To Do");
    assert_eq!(shaped[0]["to_status"], "To Do");
    assert!(shaped[0].get("isAvailable").is_none());
}

#[test]
fn shape_transitions_handles_missing_array() {
    assert!(shape_transitions(&json!({})).is_empty());
}
