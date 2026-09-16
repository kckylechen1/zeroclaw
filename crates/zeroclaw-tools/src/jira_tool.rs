use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::policy::{SecurityPolicy, ToolOperation};

const JIRA_SEARCH_PAGE_SIZE: u32 = 100;
const MAX_ERROR_BODY_CHARS: usize = 500;

/// Controls how much data is returned by `get_ticket`.
#[derive(Default)]
enum LevelOfDetails {
    Basic,
    #[default]
    BasicSearch,
    Full,
    Changelog,
}

pub struct JiraTool {
    base_url: String,
    email: Option<String>,
    api_token: String,
    allowed_actions: Vec<String>,
    http: Client,
    security: Arc<SecurityPolicy>,
    timeout_secs: u64,
}

impl JiraTool {
    pub fn new(
        base_url: String,
        email: Option<String>,
        api_token: String,
        allowed_actions: Vec<String>,
        security: Arc<SecurityPolicy>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            email,
            api_token,
            allowed_actions,
            http: Client::new(),
            security,
            timeout_secs,
        }
    }

    /// `"3"` for Jira Cloud (email present), `"2"` for Server/DC (no email).
    fn api_version(&self) -> &str {
        if self.email.is_some() { "3" } else { "2" }
    }

    /// Returns an authenticated request builder.
    /// Cloud: HTTP Basic (`email:token`). Server/DC: Bearer token.
    fn authenticated(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.email {
            Some(email) => req.basic_auth(email, Some(&self.api_token)),
            None => req.bearer_auth(&self.api_token),
        }
    }

    /// `true` when connected to Jira Cloud (API v3, email present).
    fn is_cloud(&self) -> bool {
        self.email.is_some()
    }

    fn is_action_allowed(&self, action: &str) -> bool {
        self.allowed_actions.iter().any(|a| a == action)
    }

    async fn get_ticket(
        &self,
        issue_key: &str,
        level: LevelOfDetails,
    ) -> anyhow::Result<ToolResult> {
        validate_issue_key(issue_key)?;
        let ver = self.api_version();
        let url = format!("{}/rest/api/{}/issue/{}", self.base_url, ver, issue_key);

        let query: Vec<(&str, &str)> = match &level {
            LevelOfDetails::Basic => vec![
                ("fields", "summary"),
                ("fields", "priority"),
                ("fields", "status"),
                ("fields", "assignee"),
                ("fields", "description"),
                ("fields", "created"),
                ("fields", "updated"),
                ("fields", "comment"),
                ("expand", "renderedFields"),
            ],
            LevelOfDetails::BasicSearch => vec![
                ("fields", "summary"),
                ("fields", "priority"),
                ("fields", "status"),
                ("fields", "assignee"),
                ("fields", "created"),
                ("fields", "updated"),
            ],
            LevelOfDetails::Full => vec![("expand", "renderedFields"), ("expand", "names")],
            LevelOfDetails::Changelog => vec![("expand", "changelog")],
        };

        let req = self
            .http
            .get(&url)
            .query(&query)
            .timeout(std::time::Duration::from_secs(self.timeout_secs));
        let resp = self.authenticated(req).send().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Jira get_ticket request failed"
            );
            anyhow::Error::msg(format!("Jira get_ticket request failed: {e}"))
        })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Jira get_ticket failed ({status}): {}",
                crate::util_helpers::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS)
            );
        }

        let raw: Value = resp.json().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Failed to parse Jira get_ticket response"
            );
            anyhow::Error::msg(format!("Failed to parse Jira get_ticket response: {e}"))
        })?;

        let shaped = match level {
            LevelOfDetails::Basic => shape_basic(&raw),
            LevelOfDetails::BasicSearch => shape_basic_search(&raw),
            LevelOfDetails::Full => shape_full(&raw),
            LevelOfDetails::Changelog => shape_changelog(&raw),
        };

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&shaped)
                .unwrap_or_else(|_| shaped.to_string())
                .into(),
            error: None,
        })
    }

    #[allow(clippy::cast_possible_truncation)]
    async fn search_tickets(
        &self,
        jql: &str,
        max_results: Option<u32>,
    ) -> anyhow::Result<ToolResult> {
        let max_results = max_results.unwrap_or(25).clamp(1, 999);

        let issues = if self.is_cloud() {
            self.search_tickets_v3(jql, max_results).await?
        } else {
            self.search_tickets_v2(jql, max_results).await?
        };

        let output = json!(issues);
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&output)
                .unwrap_or_else(|_| output.to_string())
                .into(),
            error: None,
        })
    }

    /// Cloud (v3): `POST /rest/api/3/search/jql` with `nextPageToken` pagination.
    #[allow(clippy::cast_possible_truncation)]
    async fn search_tickets_v3(&self, jql: &str, max_results: u32) -> anyhow::Result<Vec<Value>> {
        let url = format!("{}/rest/api/3/search/jql", self.base_url);
        let mut issues: Vec<Value> = Vec::new();
        let mut next_page_token: Option<String> = None;

        loop {
            let remaining = max_results.saturating_sub(issues.len() as u32);
            let page_size = remaining.min(JIRA_SEARCH_PAGE_SIZE);

            let mut body = json!({
                "jql": jql,
                "maxResults": page_size,
                "fields": ["summary", "priority", "status", "assignee", "created", "updated"]
            });

            if let Some(token) = &next_page_token {
                body["nextPageToken"] = json!(token);
            }

            let req = self
                .http
                .post(&url)
                .json(&body)
                .timeout(std::time::Duration::from_secs(self.timeout_secs));
            let resp = self.authenticated(req).send().await.map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "jira: Jira search_tickets request failed"
                );
                anyhow::Error::msg(format!("Jira search_tickets request failed: {e}"))
            })?;

            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "Jira search_tickets failed ({status}): {}",
                    crate::util_helpers::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS)
                );
            }

            let raw: Value = resp.json().await.map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "jira: Failed to parse Jira search response"
                );
                anyhow::Error::msg(format!("Failed to parse Jira search response: {e}"))
            })?;

            if let Some(page) = raw["issues"].as_array() {
                issues.extend(page.iter().map(shape_basic_search));
            }

            let is_last = raw["isLast"].as_bool().unwrap_or(true);
            if is_last || issues.len() as u32 >= max_results {
                break;
            }

            next_page_token = raw["nextPageToken"].as_str().map(String::from);
            if next_page_token.is_none() {
                break;
            }
        }

        Ok(issues)
    }

    /// Server/DC (v2): `POST /rest/api/2/search` with `startAt` offset pagination.
    #[allow(clippy::cast_possible_truncation)]
    async fn search_tickets_v2(&self, jql: &str, max_results: u32) -> anyhow::Result<Vec<Value>> {
        let url = format!("{}/rest/api/2/search", self.base_url);
        let mut issues: Vec<Value> = Vec::new();
        let mut start_at: u32 = 0;

        loop {
            let remaining = max_results.saturating_sub(issues.len() as u32);
            let page_size = remaining.min(JIRA_SEARCH_PAGE_SIZE);

            let body = json!({
                "jql": jql,
                "startAt": start_at,
                "maxResults": page_size,
                "fields": ["summary", "priority", "status", "assignee", "created", "updated"]
            });

            let req = self
                .http
                .post(&url)
                .json(&body)
                .timeout(std::time::Duration::from_secs(self.timeout_secs));
            let resp = self.authenticated(req).send().await.map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "jira: Jira search_tickets request failed"
                );
                anyhow::Error::msg(format!("Jira search_tickets request failed: {e}"))
            })?;

            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "Jira search_tickets failed ({status}): {}",
                    crate::util_helpers::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS)
                );
            }

            let raw: Value = resp.json().await.map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "jira: Failed to parse Jira search response"
                );
                anyhow::Error::msg(format!("Failed to parse Jira search response: {e}"))
            })?;

            let page = raw["issues"].as_array();
            let page_len = page.map_or(0, |p| p.len());
            if let Some(page) = page {
                issues.extend(page.iter().map(shape_basic_search));
            }

            let total = raw["total"].as_u64().unwrap_or(0) as u32;
            start_at += page_len as u32;
            if page_len == 0 || start_at >= total || issues.len() as u32 >= max_results {
                break;
            }
        }

        Ok(issues)
    }

    async fn comment_ticket(
        &self,
        issue_key: &str,
        comment_text: &str,
    ) -> anyhow::Result<ToolResult> {
        validate_issue_key(issue_key)?;

        let ver = self.api_version();
        let url = format!(
            "{}/rest/api/{}/issue/{}/comment",
            self.base_url, ver, issue_key
        );

        let body = if self.is_cloud() {
            let emails = extract_emails(comment_text);
            let mut mentions: HashMap<String, (String, String)> = HashMap::new();
            for email in emails {
                if let Some(info) = self.resolve_email(&email).await {
                    mentions.insert(email, info);
                }
            }
            let adf = build_adf(comment_text, &mentions);
            json!({ "body": adf })
        } else {
            json!({ "body": comment_text })
        };

        let req = self
            .http
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(self.timeout_secs));
        let resp = self.authenticated(req).send().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Jira comment_ticket request failed"
            );
            anyhow::Error::msg(format!("Jira comment_ticket request failed: {e}"))
        })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Jira comment_ticket failed ({status}): {}",
                crate::util_helpers::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS)
            );
        }

        let response: Value = resp.json().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Failed to parse Jira comment response"
            );
            anyhow::Error::msg(format!("Failed to parse Jira comment response: {e}"))
        })?;

        let shaped = shape_comment_response(&response);
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&shaped)
                .unwrap_or_else(|_| shaped.to_string())
                .into(),
            error: None,
        })
    }

    async fn list_projects(&self) -> anyhow::Result<ToolResult> {
        let ver = self.api_version();
        let url = format!("{}/rest/api/{}/project", self.base_url, ver);

        let req = self
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(self.timeout_secs));
        let resp = self.authenticated(req).send().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Jira list_projects request failed"
            );
            anyhow::Error::msg(format!("Jira list_projects request failed: {e}"))
        })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Jira list_projects failed ({status}): {}",
                crate::util_helpers::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS)
            );
        }

        let projects: Vec<Value> = resp.json().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Failed to parse Jira list_projects response"
            );
            anyhow::Error::msg(format!("Failed to parse Jira list_projects response: {e}"))
        })?;

        let keys: Vec<String> = projects
            .iter()
            .filter_map(|p| p["key"].as_str().map(String::from))
            .collect();

        const STATUS_CONCURRENCY: usize = 5;

        let users_url = format!(
            "{}/rest/api/{}/user/assignable/multiProjectSearch",
            self.base_url, ver
        );

        let users_req = self
            .http
            .get(&users_url)
            .query(&[
                ("projectKeys", keys.join(",").as_str()),
                ("maxResults", "50"),
            ])
            .timeout(std::time::Duration::from_secs(self.timeout_secs));
        let users_resp = self.authenticated(users_req).send().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Jira list_projects users request failed"
            );
            anyhow::Error::msg(format!("Jira list_projects users request failed: {e}"))
        })?;

        let users: Vec<Value> = if users_resp.status().is_success() {
            users_resp.json().await.map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "jira: Failed to parse Jira list_projects users response"
                );
                anyhow::Error::msg(format!(
                    "Failed to parse Jira list_projects users response: {e}"
                ))
            })?
        } else {
            let status = users_resp.status();
            let text = users_resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Jira list_projects users failed ({status}): {}",
                crate::util_helpers::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS)
            );
        };

        let mut set: tokio::task::JoinSet<(usize, anyhow::Result<Value>)> =
            tokio::task::JoinSet::new();
        let mut statuses_results = vec![json!([]); keys.len()];

        for (i, key) in keys.iter().enumerate() {
            if set.len() >= STATUS_CONCURRENCY {
                let Some(Ok((idx, result))) = set.join_next().await else {
                    continue;
                };
                statuses_results[idx] = result.map_err(|e| {
                    ::zeroclaw_log::record!(
                        ERROR,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "jira: Jira statuses failed"
                    );
                    anyhow::Error::msg(format!("Jira statuses failed: {e}"))
                })?;
            }

            let client = self.http.clone();
            let request_url = format!("{url}/{key}/statuses");
            let email = self.email.clone();
            let token = self.api_token.clone();
            let timeout = self.timeout_secs;

            set.spawn(async move {
                let result = async {
                    let req = client
                        .get(&request_url)
                        .timeout(std::time::Duration::from_secs(timeout));
                    let req = match &email {
                        Some(e) => req.basic_auth(e, Some(&token)),
                        None => req.bearer_auth(&token),
                    };
                    let resp = req.send().await.map_err(|e| {
                        ::zeroclaw_log::record!(
                            ERROR,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "jira: statuses request failed"
                        );
                        anyhow::Error::msg(format!("statuses request failed: {e}"))
                    })?;

                    if !resp.status().is_success() {
                        anyhow::bail!("statuses request returned {}", resp.status());
                    }

                    resp.json::<Value>().await.map_err(|e| {
                        ::zeroclaw_log::record!(
                            ERROR,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "jira: failed to parse statuses response"
                        );
                        anyhow::Error::msg(format!("failed to parse statuses response: {e}"))
                    })
                }
                .await;
                (i, result)
            });
        }

        while let Some(Ok((idx, result))) = set.join_next().await {
            statuses_results[idx] = result.map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "jira: Jira statuses failed"
                );
                anyhow::Error::msg(format!("Jira statuses failed: {e}"))
            })?;
        }

        let shaped_projects = shape_projects(&projects, &statuses_results);
        let shaped_users: Vec<Value> = users
            .iter()
            .filter_map(|u| {
                let display = u["displayName"].as_str()?;
                let email = u["emailAddress"].as_str()?;
                Some(json!({ "displayName": display, "emailAddress": email }))
            })
            .collect();

        let output = json!({ "projects": shaped_projects, "users": shaped_users });
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&output)
                .unwrap_or_else(|_| output.to_string())
                .into(),
            error: None,
        })
    }

    async fn get_myself(&self) -> anyhow::Result<ToolResult> {
        let ver = self.api_version();
        let url = format!("{}/rest/api/{}/myself", self.base_url, ver);

        let req = self
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(self.timeout_secs));
        let resp = self.authenticated(req).send().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Jira myself request failed"
            );
            anyhow::Error::msg(format!("Jira myself request failed: {e}"))
        })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Jira myself failed ({status}): {}",
                crate::util_helpers::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS)
            );
        }

        let raw: Value = resp.json().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Failed to parse Jira myself response"
            );
            anyhow::Error::msg(format!("Failed to parse Jira myself response: {e}"))
        })?;

        let shaped = json!({
            "accountId":    raw["accountId"],
            "displayName":  raw["displayName"],
            "emailAddress": raw["emailAddress"],
            "active":       raw["active"],
        });

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&shaped)
                .unwrap_or_else(|_| shaped.to_string())
                .into(),
            error: None,
        })
    }

    async fn resolve_email(&self, email: &str) -> Option<(String, String)> {
        let ver = self.api_version();
        let url = format!("{}/rest/api/{}/user/search", self.base_url, ver);
        let req = self
            .http
            .get(&url)
            .query(&[("query", email)])
            .timeout(std::time::Duration::from_secs(self.timeout_secs));
        let result = self
            .authenticated(req)
            .send()
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()?;

        result.as_array()?.iter().find_map(|u| {
            let account_email = u["emailAddress"].as_str()?;
            if account_email.eq_ignore_ascii_case(email) {
                Some((
                    u["accountId"].as_str()?.to_string(),
                    u["displayName"].as_str()?.to_string(),
                ))
            } else {
                None
            }
        })
    }

    /// Fetches the available transitions for an issue and returns a minimal
    /// shape `{ transitions: [{ id, name, to_status }] }`.
    async fn fetch_transitions(&self, issue_key: &str) -> anyhow::Result<Vec<Value>> {
        validate_issue_key(issue_key)?;
        let ver = self.api_version();
        let url = format!(
            "{}/rest/api/{}/issue/{}/transitions",
            self.base_url, ver, issue_key
        );

        let req = self
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(self.timeout_secs));
        let resp = self.authenticated(req).send().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Jira list_transitions request failed"
            );
            anyhow::Error::msg(format!("Jira list_transitions request failed: {e}"))
        })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Jira list_transitions failed ({status}): {}",
                crate::util_helpers::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS)
            );
        }

        let raw: Value = resp.json().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Failed to parse Jira transitions response"
            );
            anyhow::Error::msg(format!("Failed to parse Jira transitions response: {e}"))
        })?;

        Ok(shape_transitions(&raw))
    }

    async fn list_transitions(&self, issue_key: &str) -> anyhow::Result<ToolResult> {
        let transitions = self.fetch_transitions(issue_key).await?;
        let output = json!({ "transitions": transitions });
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&output)
                .unwrap_or_else(|_| output.to_string())
                .into(),
            error: None,
        })
    }

    async fn transition_ticket(
        &self,
        issue_key: &str,
        transition_id: Option<&str>,
        transition_name: Option<&str>,
    ) -> anyhow::Result<ToolResult> {
        validate_issue_key(issue_key)?;

        // Resolve transition_name → id if needed.
        let resolved_id: String = match (transition_id, transition_name) {
            (Some(id), _) if !id.trim().is_empty() => id.to_string(),
            (_, Some(name)) if !name.trim().is_empty() => {
                let transitions = self.fetch_transitions(issue_key).await?;
                let needle = name.trim().to_ascii_lowercase();
                let found = transitions.iter().find_map(|t| {
                    let n = t["name"].as_str()?;
                    if n.eq_ignore_ascii_case(&needle) || n.to_ascii_lowercase() == needle {
                        t["id"].as_str().map(String::from)
                    } else {
                        None
                    }
                });
                match found {
                    Some(id) => id,
                    None => {
                        let available: Vec<&str> = transitions
                            .iter()
                            .filter_map(|t| t["name"].as_str())
                            .collect();
                        anyhow::bail!(
                            "Transition '{name}' not found for {issue_key}. Available: {}",
                            available.join(", ")
                        );
                    }
                }
            }
            _ => {
                anyhow::bail!(
                    "transition_ticket requires exactly one of transition_id or transition_name"
                );
            }
        };

        let ver = self.api_version();
        let url = format!(
            "{}/rest/api/{}/issue/{}/transitions",
            self.base_url, ver, issue_key
        );
        let body = json!({ "transition": { "id": resolved_id } });

        let req = self
            .http
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(self.timeout_secs));
        let resp = self.authenticated(req).send().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Jira transition_ticket request failed"
            );
            anyhow::Error::msg(format!("Jira transition_ticket request failed: {e}"))
        })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Jira transition_ticket failed ({status}): {}",
                crate::util_helpers::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS)
            );
        }

        // Jira returns 204 No Content on a successful transition.
        let output = json!({
            "ok": true,
            "issue_key": issue_key,
            "transition_id": resolved_id,
        });
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&output)
                .unwrap_or_else(|_| output.to_string())
                .into(),
            error: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_ticket(
        &self,
        project_key: &str,
        issue_type: &str,
        summary: &str,
        description: Option<&str>,
        assignee: Option<&str>,
        labels: Option<&[String]>,
        parent_key: Option<&str>,
    ) -> anyhow::Result<ToolResult> {
        validate_project_key(project_key)?;
        if summary.trim().is_empty() {
            anyhow::bail!("create_ticket requires a non-empty summary");
        }
        if issue_type.trim().is_empty() {
            anyhow::bail!("create_ticket requires a non-empty issue_type");
        }
        if let Some(parent) = parent_key {
            validate_issue_key(parent)?;
        }

        let mut fields = serde_json::Map::new();
        fields.insert("project".into(), json!({ "key": project_key }));
        fields.insert("issuetype".into(), json!({ "name": issue_type }));
        fields.insert("summary".into(), json!(summary));

        if let Some(desc) = description {
            let value = if self.is_cloud() {
                build_adf(desc, &HashMap::new())
            } else {
                json!(desc)
            };
            fields.insert("description".into(), value);
        }

        if let Some(a) = assignee {
            let value = if self.is_cloud() {
                json!({ "accountId": a })
            } else {
                json!({ "name": a })
            };
            fields.insert("assignee".into(), value);
        }

        if let Some(ls) = labels {
            fields.insert("labels".into(), json!(ls));
        }

        if let Some(parent) = parent_key {
            fields.insert("parent".into(), json!({ "key": parent }));
        }

        let body = json!({ "fields": Value::Object(fields) });

        let ver = self.api_version();
        let url = format!("{}/rest/api/{}/issue", self.base_url, ver);

        let req = self
            .http
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(self.timeout_secs));
        let resp = self.authenticated(req).send().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Jira create_ticket request failed"
            );
            anyhow::Error::msg(format!("Jira create_ticket request failed: {e}"))
        })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Jira create_ticket failed ({status}): {}",
                crate::util_helpers::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS)
            );
        }

        let raw: Value = resp.json().await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "jira: Failed to parse Jira create_ticket response"
            );
            anyhow::Error::msg(format!("Failed to parse Jira create_ticket response: {e}"))
        })?;

        let key = raw["key"].as_str().unwrap_or("");
        let output = json!({
            "id":         raw["id"],
            "key":        key,
            "self_url":   raw["self"],
            "browse_url": format!("{}/browse/{}", self.base_url, key),
        });
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&output)
                .unwrap_or_else(|_| output.to_string())
                .into(),
            error: None,
        })
    }
}

#[async_trait]
impl Tool for JiraTool {
    fn name(&self) -> &str {
        "jira"
    }

    fn description(&self) -> &str {
        "Interact with Jira: read tickets, search with JQL, add comments, list projects and per-issue transitions, transition an issue through its workflow, and create new issues."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "get_ticket",
                        "search_tickets",
                        "comment_ticket",
                        "list_projects",
                        "myself",
                        "list_transitions",
                        "transition_ticket",
                        "create_ticket"
                    ],
                    "description": "The Jira action to perform. Enabled actions are configured in [jira].allowed_actions. Use 'myself' to verify that credentials are valid and the Jira connection is working."
                },
                "issue_key": {
                    "type": "string",
                    "description": "Jira issue key, e.g. 'PROJ-123'. Required for get_ticket, comment_ticket, list_transitions, and transition_ticket."
                },
                "level_of_details": {
                    "type": "string",
                    "enum": ["basic", "basic_search", "full", "changelog"],
                    "description": "How much data to return for get_ticket. Omit to use the default ('basic'). Options: 'basic' — summary, status, priority, assignee, rendered description, and rendered comments (best for reading a ticket in full); 'basic_search' — lightweight fields only, no description or comments (best when you only need to identify the ticket); 'full' — all Jira fields plus rendered HTML (verbose, use sparingly); 'changelog' — issue key and full change history only."
                },
                "jql": {
                    "type": "string",
                    "description": "JQL query string for search_tickets. Example: 'project = PROJ AND status = \"In Progress\" ORDER BY updated DESC'."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of issues to return for search_tickets. Defaults to 25, capped at 999.",
                    "default": 25
                },
                "comment": {
                    "type": "string",
                    "description": "Comment body for comment_ticket. In Jira Cloud mode, supports a limited markdown-like syntax converted to Atlassian Document Format (ADF): mention a user with @user@domain.com (the leading @ is required; a bare email without @ prefix is treated as plain text), bold with **text**, bullet list items with a leading '- ', and newlines as line breaks. In Jira Server/Data Center mode, comments are posted as plain text with no ADF conversion or mention resolution. Example: 'Hi @john@company.com, this is **important**.\n- Check the logs\n- Rerun the pipeline'"
                },
                "transition_id": {
                    "type": "string",
                    "description": "Transition ID to apply for transition_ticket. Provide either transition_id or transition_name (not both). Use list_transitions to discover the IDs valid for an issue's current state."
                },
                "transition_name": {
                    "type": "string",
                    "description": "Transition name (case-insensitive) to apply for transition_ticket, e.g. 'In Progress' or 'Done'. Provide either transition_id or transition_name (not both). The tool resolves the name against the issue's available transitions and returns an error listing valid names if not found."
                },
                "project_key": {
                    "type": "string",
                    "description": "Jira project key, e.g. 'PROJ'. Required for create_ticket. Use list_projects to discover keys."
                },
                "issue_type": {
                    "type": "string",
                    "description": "Issue type name, e.g. 'Task', 'Bug', 'Story'. Required for create_ticket. Valid values per project are returned by list_projects."
                },
                "summary": {
                    "type": "string",
                    "description": "Ticket title. Required for create_ticket. Must be non-empty."
                },
                "description": {
                    "type": "string",
                    "description": "Ticket description for create_ticket. Optional. In Jira Cloud mode, the same limited markdown-like syntax as 'comment' is supported and rendered to ADF (no mention resolution). In Server/Data Center mode, sent as plain text."
                },
                "assignee": {
                    "type": "string",
                    "description": "Assignee for create_ticket. Optional. In Jira Cloud, pass an accountId; in Server/Data Center, pass a username."
                },
                "labels": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Labels to attach to the new issue for create_ticket. Optional."
                },
                "parent_key": {
                    "type": "string",
                    "description": "Parent issue key for create_ticket. Optional. Used for sub-tasks or to set the parent epic (e.g. 'PROJ-100')."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some("Missing required parameter: action".into()),
                });
            }
        };

        // Reject unknown actions before the allowlist check so typos produce a
        // clear "unknown action" error rather than a misleading "not enabled" one.
        if !matches!(
            action,
            "get_ticket"
                | "search_tickets"
                | "comment_ticket"
                | "list_projects"
                | "myself"
                | "list_transitions"
                | "transition_ticket"
                | "create_ticket"
        ) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Unknown action: '{action}'. Valid actions: get_ticket, search_tickets, comment_ticket, list_projects, myself, list_transitions, transition_ticket, create_ticket"
                )),
            });
        }

        if !self.is_action_allowed(action) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Action '{action}' is not enabled. Add it to jira.allowed_actions in config.toml. \
                     Currently allowed: {}",
                    self.allowed_actions.join(", ")
                )),
            });
        }

        let operation = match action {
            "get_ticket" | "search_tickets" | "list_projects" | "myself" | "list_transitions" => {
                ToolOperation::Read
            }
            "comment_ticket" | "transition_ticket" | "create_ticket" => ToolOperation::Act,
            _ => unreachable!(),
        };

        if let Err(error) = self.security.enforce_tool_operation(operation, "jira") {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(error),
            });
        }

        let result = match action {
            "get_ticket" => {
                let issue_key = match args.get("issue_key").and_then(|v| v.as_str()) {
                    Some(k) => k,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: ToolOutput::default(),
                            error: Some("get_ticket requires issue_key parameter".into()),
                        });
                    }
                };
                let level = match args.get("level_of_details").and_then(|v| v.as_str()) {
                    Some("basic_search") => LevelOfDetails::BasicSearch,
                    Some("full") => LevelOfDetails::Full,
                    Some("changelog") => LevelOfDetails::Changelog,
                    _ => LevelOfDetails::Basic,
                };
                self.get_ticket(issue_key, level).await
            }
            "search_tickets" => {
                let jql = match args.get("jql").and_then(|v| v.as_str()) {
                    Some(j) => j,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: ToolOutput::default(),
                            error: Some("search_tickets requires jql parameter".into()),
                        });
                    }
                };
                let max_results = args
                    .get("max_results")
                    .and_then(|v| v.as_u64())
                    .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
                self.search_tickets(jql, max_results).await
            }
            "myself" => self.get_myself().await,
            "list_projects" => self.list_projects().await,
            "comment_ticket" => {
                let issue_key = match args.get("issue_key").and_then(|v| v.as_str()) {
                    Some(k) => k,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: ToolOutput::default(),
                            error: Some("comment_ticket requires issue_key parameter".into()),
                        });
                    }
                };
                let comment = match args.get("comment").and_then(|v| v.as_str()) {
                    Some(c) if !c.trim().is_empty() => c,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: ToolOutput::default(),
                            error: Some(
                                "comment_ticket requires a non-empty comment parameter".into(),
                            ),
                        });
                    }
                };
                self.comment_ticket(issue_key, comment).await
            }
            "list_transitions" => {
                let issue_key = match args.get("issue_key").and_then(|v| v.as_str()) {
                    Some(k) => k,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: ToolOutput::default(),
                            error: Some("list_transitions requires issue_key parameter".into()),
                        });
                    }
                };
                self.list_transitions(issue_key).await
            }
            "transition_ticket" => {
                let issue_key = match args.get("issue_key").and_then(|v| v.as_str()) {
                    Some(k) => k,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: ToolOutput::default(),
                            error: Some("transition_ticket requires issue_key parameter".into()),
                        });
                    }
                };
                let transition_id = args
                    .get("transition_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty());
                let transition_name = args
                    .get("transition_name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty());
                if transition_id.is_none() && transition_name.is_none() {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(
                            "transition_ticket requires either transition_id or transition_name"
                                .into(),
                        ),
                    });
                }
                if transition_id.is_some() && transition_name.is_some() {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(
                            "transition_ticket accepts only one of transition_id or transition_name, not both".into(),
                        ),
                    });
                }
                self.transition_ticket(issue_key, transition_id, transition_name)
                    .await
            }
            "create_ticket" => {
                let project_key = match args.get("project_key").and_then(|v| v.as_str()) {
                    Some(k) if !k.trim().is_empty() => k,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: ToolOutput::default(),
                            error: Some(
                                "create_ticket requires a non-empty project_key parameter".into(),
                            ),
                        });
                    }
                };
                let issue_type = match args.get("issue_type").and_then(|v| v.as_str()) {
                    Some(t) if !t.trim().is_empty() => t,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: ToolOutput::default(),
                            error: Some(
                                "create_ticket requires a non-empty issue_type parameter".into(),
                            ),
                        });
                    }
                };
                let summary = match args.get("summary").and_then(|v| v.as_str()) {
                    Some(s) if !s.trim().is_empty() => s,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: ToolOutput::default(),
                            error: Some(
                                "create_ticket requires a non-empty summary parameter".into(),
                            ),
                        });
                    }
                };
                let description = args
                    .get("description")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                let assignee = args
                    .get("assignee")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty());
                let labels: Option<Vec<String>> = args.get("labels").and_then(|v| {
                    v.as_array().map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                });
                let parent_key = args
                    .get("parent_key")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty());
                self.create_ticket(
                    project_key,
                    issue_type,
                    summary,
                    description,
                    assignee,
                    labels.as_deref(),
                    parent_key,
                )
                .await
            }
            _ => unreachable!(),
        };

        match result {
            Ok(tool_result) => Ok(tool_result),
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(e.to_string()),
            }),
        }
    }
}

// ── Input validation ──────────────────────────────────────────────────────────

/// Validates that `issue_key` matches the Jira key format `PROJ-123` or `proj-123`.
/// Prevents path traversal if a crafted key like `../../other` were interpolated
/// directly into the URL.
fn validate_issue_key(key: &str) -> anyhow::Result<()> {
    let valid = key.split_once('-').is_some_and(|(project, number)| {
        !project.is_empty()
            && project.chars().all(|c| c.is_ascii_alphanumeric())
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit())
    });
    if valid {
        Ok(())
    } else {
        anyhow::bail!(
            "Invalid issue key '{key}'. Expected format: PROJECT-123 (e.g. PROJ-42, proj-42)"
        )
    }
}

/// Validates that `key` matches the Jira project key format. Same character
/// class as the project portion of `validate_issue_key` so the two stay in
/// step.
fn validate_project_key(key: &str) -> anyhow::Result<()> {
    let valid = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        anyhow::bail!("Invalid project key '{key}'. Expected ASCII alphanumeric, e.g. PROJ")
    }
}

// ── Response shaping ──────────────────────────────────────────────────────────

/// Safely extracts the first 10 characters (date prefix) from a string.
/// Returns the full string if it is shorter than 10 characters instead of
/// panicking on out-of-bounds slice indexing.
fn date_prefix(s: &str) -> &str {
    s.get(..10).unwrap_or(s)
}

fn shape_basic(raw: &Value) -> Value {
    let f = &raw["fields"];
    let rf = &raw["renderedFields"];

    // Build a lookup map from comment ID → rendered body for O(1) access
    // instead of scanning the rendered array for each comment (O(n²)).
    let rendered_by_id: HashMap<&str, &str> = rf["comment"]["comments"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|rc| Some((rc["id"].as_str()?, rc["body"].as_str()?)))
                .collect()
        })
        .unwrap_or_default();

    let comments: Vec<Value> = f["comment"]["comments"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| {
                    let id = c["id"].as_str().unwrap_or("");
                    json!({
                        "author": c["author"]["displayName"],
                        "created": date_prefix(c["created"].as_str().unwrap_or("")),
                        "body": rendered_by_id.get(id).copied().unwrap_or("")
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "key":         raw["key"],
        "summary":     f["summary"],
        "status":      f["status"]["name"],
        "priority":    f["priority"]["name"],
        "assignee":    f["assignee"]["displayName"],
        "created":     date_prefix(f["created"].as_str().unwrap_or("")),
        "updated":     date_prefix(f["updated"].as_str().unwrap_or("")),
        "description": rf["description"].as_str().unwrap_or(""),
        "comments":    comments,
    })
}

fn shape_basic_search(raw: &Value) -> Value {
    let f = &raw["fields"];
    json!({
        "key":      raw["key"],
        "summary":  f["summary"],
        "status":   f["status"]["name"],
        "priority": f["priority"]["name"],
        "assignee": f["assignee"]["displayName"],
        "created":  date_prefix(f["created"].as_str().unwrap_or("")),
        "updated":  date_prefix(f["updated"].as_str().unwrap_or("")),
    })
}

fn shape_full(raw: &Value) -> Value {
    let mut result = raw.clone();
    let rf = &raw["renderedFields"];

    if let Some(desc) = rf["description"].as_str() {
        result["fields"]["description"] = json!(desc);
    }

    if let (Some(comments), Some(rendered_comments)) = (
        result["fields"]["comment"]["comments"].as_array_mut(),
        rf["comment"]["comments"].as_array(),
    ) {
        for (c, rc) in comments.iter_mut().zip(rendered_comments.iter()) {
            if let Some(body) = rc["body"].as_str() {
                c["body"] = json!(body);
            }
        }
    }

    result.as_object_mut().unwrap().remove("renderedFields");
    result
}

fn shape_changelog(raw: &Value) -> Value {
    json!({
        "key":       raw["key"],
        "changelog": raw["changelog"],
    })
}

/// Returns only the comment ID, author, and creation date — avoids
/// exposing internal Jira metadata back to the AI.
fn shape_comment_response(raw: &Value) -> Value {
    json!({
        "id":      raw["id"],
        "author":  raw["author"]["displayName"],
        "created": date_prefix(raw["created"].as_str().unwrap_or("")),
    })
}

/// Trims Jira's transitions response to `[{ id, name, to_status }]`, dropping
/// icons, conditions, and other workflow-engine internals.
fn shape_transitions(raw: &Value) -> Vec<Value> {
    raw["transitions"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|t| {
                    json!({
                        "id":        t["id"],
                        "name":      t["name"],
                        "to_status": t["to"]["name"],
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn shape_projects(projects: &[Value], statuses_per_project: &[Value]) -> Vec<Value> {
    projects
        .iter()
        .zip(statuses_per_project.iter())
        .map(|(p, statuses)| {
            let mut issue_types: Vec<String> = Vec::new();
            let mut all_statuses: HashSet<String> = HashSet::new();

            if let Some(arr) = statuses.as_array() {
                for it in arr {
                    if let Some(name) = it["name"].as_str() {
                        issue_types.push(name.to_string());
                    }
                    if let Some(ss) = it["statuses"].as_array() {
                        for s in ss {
                            if let Some(sn) = s["name"].as_str() {
                                all_statuses.insert(sn.to_string());
                            }
                        }
                    }
                }
            }

            let mut ordered: Vec<String> = all_statuses.into_iter().collect();
            ordered.sort();

            json!({
                "key":         p["key"],
                "name":        p["name"],
                "projectType": p["projectTypeKey"],
                "style":       p["style"],
                "issueTypes":  issue_types,
                "statuses":    ordered,
            })
        })
        .collect()
}

// ── Comment / ADF builder ─────────────────────────────────────────────────────

/// Strips trailing punctuation that commonly appears after an email address
/// (e.g. `@john@co.com,` or `@john@co.com)`). Also strips leading bracket-like
/// punctuation so `@(john@co.com)` resolves correctly.
fn clean_email(s: &str) -> &str {
    s.trim_start_matches(['(', '['])
        .trim_end_matches([',', '!', '?', ':', ';', ')', ']'])
}

fn extract_emails(text: &str) -> Vec<String> {
    let mut emails = Vec::new();
    for word in text.split_whitespace() {
        if let Some(rest) = word.strip_prefix('@') {
            let email = clean_email(rest);
            if email.contains('@') {
                emails.push(email.to_string());
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    emails.retain(|e| seen.insert(e.clone()));
    emails
}

fn parse_inline(text: &str, mentions: &HashMap<String, (String, String)>) -> Vec<Value> {
    let mut nodes: Vec<Value> = Vec::new();
    let mut chars = text.chars().peekable();
    let mut current = String::new();

    while let Some(ch) = chars.next() {
        if ch == '*' && chars.peek() == Some(&'*') {
            chars.next(); // consume second *
            if !current.is_empty() {
                nodes.push(json!({ "type": "text", "text": current.clone() }));
                current.clear();
            }
            let mut bold = String::new();
            let mut closed = false;
            loop {
                match chars.next() {
                    Some('*') if chars.peek() == Some(&'*') => {
                        chars.next(); // consume second *
                        closed = true;
                        break;
                    }
                    Some(c) => bold.push(c),
                    None => break,
                }
            }
            if closed && !bold.is_empty() {
                nodes.push(json!({
                    "type": "text",
                    "text": bold,
                    "marks": [{ "type": "strong" }]
                }));
            } else if !bold.is_empty() {
                // Unmatched ** — emit as literal text
                current.push_str("**");
                current.push_str(&bold);
            }
        } else if ch == '@' {
            let mut raw = String::new();
            while let Some(&next) = chars.peek() {
                if next.is_whitespace() {
                    break;
                }
                raw.push(chars.next().unwrap());
            }
            let email = clean_email(&raw);
            // Compute the end position of `email` within `raw` via pointer
            // arithmetic so the suffix is correct even when leading chars were
            // stripped by clean_email.
            let email_end = (email.as_ptr() as usize - raw.as_ptr() as usize) + email.len();
            let suffix = &raw[email_end..];
            if email.contains('@') {
                if let Some((account_id, display_name)) = mentions.get(email) {
                    if !current.is_empty() {
                        nodes.push(json!({ "type": "text", "text": current.clone() }));
                        current.clear();
                    }
                    nodes.push(json!({
                        "type": "mention",
                        "attrs": {
                            "id": account_id,
                            "text": format!("@{}", display_name)
                        }
                    }));
                    if !suffix.is_empty() {
                        current.push_str(suffix);
                    }
                } else {
                    current.push('@');
                    current.push_str(&raw);
                }
            } else {
                current.push('@');
                current.push_str(email);
            }
        } else {
            current.push(ch);
        }
    }

    if !current.is_empty() {
        nodes.push(json!({ "type": "text", "text": current }));
    }

    nodes
}

fn build_adf(text: &str, mentions: &HashMap<String, (String, String)>) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut paragraph: Vec<Value> = Vec::new();
    let mut list_items: Vec<Value> = Vec::new();

    let flush_paragraph = |paragraph: &mut Vec<Value>, content: &mut Vec<Value>| {
        if !paragraph.is_empty() {
            content.push(json!({ "type": "paragraph", "content": paragraph.clone() }));
            paragraph.clear();
        }
    };

    let flush_list = |list_items: &mut Vec<Value>, content: &mut Vec<Value>| {
        if !list_items.is_empty() {
            content.push(json!({ "type": "bulletList", "content": list_items.clone() }));
            list_items.clear();
        }
    };

    for line in text.lines() {
        if line.trim().is_empty() {
            flush_paragraph(&mut paragraph, &mut content);
            flush_list(&mut list_items, &mut content);
        } else if let Some(item) = line.strip_prefix("- ") {
            flush_paragraph(&mut paragraph, &mut content);
            let inline = parse_inline(item, mentions);
            list_items.push(json!({
                "type": "listItem",
                "content": [{ "type": "paragraph", "content": inline }]
            }));
        } else {
            flush_list(&mut list_items, &mut content);
            if !paragraph.is_empty() {
                paragraph.push(json!({ "type": "hardBreak" }));
            }
            paragraph.extend(parse_inline(line, mentions));
        }
    }

    flush_paragraph(&mut paragraph, &mut content);
    flush_list(&mut list_items, &mut content);

    json!({ "type": "doc", "version": 1, "content": content })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
