//! Cost ledger records and reporting types shared across runtime consumers.
//! Pricing, budget policy and persistence belong to `zeroclaw-cost`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Model identifier (e.g., "anthropic/claude-sonnet-4-20250514")
    pub model: String,
    /// Input/prompt tokens
    pub input_tokens: u64,
    /// Output/completion tokens
    pub output_tokens: u64,
    /// Cached input tokens (Anthropic `cache_read_input_tokens`, OpenAI
    /// `prompt_tokens_details.cached_tokens`). Subset of `input_tokens`
    /// when reported by the provider; the rate sheet's
    /// `cached_input_per_mtok` applies to these.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cached_input_tokens: u64,
    /// Total tokens (input + output, ignoring the cached subset).
    pub total_tokens: u64,
    /// Calculated cost in USD
    pub cost_usd: f64,
    #[serde(default = "default_true", skip_serializing_if = "is_true_bool")]
    pub pricing_available: bool,
    /// Timestamp of the request
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn default_true() -> bool {
    true
}

fn is_true_bool(v: &bool) -> bool {
    *v
}

impl TokenUsage {
    pub fn billable_input_tokens(&self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_input_tokens)
    }

    /// Get the recorded total cost without repricing historical usage.
    pub fn cost(&self) -> f64 {
        self.cost_usd
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsagePeriod {
    /// Current tracker session for this daemon lifetime.
    Session,
    /// Current UTC/local-day aggregate used by daily budget checks.
    Day,
    /// Current calendar-month aggregate used by monthly budget checks.
    Month,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRecord {
    /// Unique identifier
    pub id: String,
    /// Token usage details
    pub usage: TokenUsage,
    /// Session identifier (for grouping)
    pub session_id: String,
    /// Alias of the agent that incurred this cost (HashMap key in
    /// `config.agents`). `None` for records persisted before per-agent
    /// attribution, or when `[cost].track_per_agent = false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_alias: Option<String>,
    #[serde(
        default,
        rename = "goal_task_id",
        alias = "task_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub task_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum BudgetCheck {
    /// Within budget; request can proceed.
    Allowed,
    /// Warning threshold exceeded, but request can proceed.
    Warning {
        /// Current spend in the affected aggregation period before the
        /// estimated request cost is committed.
        current_usd: f64,
        /// Configured limit for the affected aggregation period.
        limit_usd: f64,
        /// Aggregation period that crossed the warning threshold.
        period: UsagePeriod,
    },
    /// Budget exceeded; request should be blocked before provider dispatch.
    Exceeded {
        /// Current spend in the affected aggregation period before the
        /// estimated request cost is committed.
        current_usd: f64,
        /// Configured limit for the affected aggregation period.
        limit_usd: f64,
        /// Aggregation period whose limit would be exceeded.
        period: UsagePeriod,
    },
}

/// Cost summary for reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostSummary {
    /// Total cost for the session
    pub session_cost_usd: f64,
    /// Total cost for the day
    pub daily_cost_usd: f64,
    /// Total cost for the month
    pub monthly_cost_usd: f64,
    /// Total tokens used
    pub total_tokens: u64,
    /// Number of requests
    pub request_count: usize,
    /// Breakdown by model
    pub by_model: std::collections::HashMap<String, ModelStats>,
    /// Breakdown by agent alias. Empty when `[cost].track_per_agent =
    /// false` or when no records carry an agent_alias.
    #[serde(default)]
    pub by_agent: std::collections::HashMap<String, AgentCostStats>,
}

/// Statistics for a specific agent alias.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCostStats {
    /// Agent alias (HashMap key in `config.agents`).
    pub agent_alias: String,
    /// Total cost attributed to this agent for the period.
    pub cost_usd: f64,
    /// Total tokens attributed to this agent for the period (input + output).
    pub total_tokens: u64,
    /// Input tokens (uncached + cached). Matches each record's
    /// `input_tokens` field.
    #[serde(default)]
    pub input_tokens: u64,
    /// Output tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// Cached input tokens (subset of `input_tokens` served from the
    /// provider's prompt cache).
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Number of LLM responses attributed to this agent for the period.
    pub request_count: usize,
}

/// Statistics for a specific model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStats {
    /// Model name (upstream resource id from usage telemetry).
    pub model: String,
    /// Total cost for this model.
    pub cost_usd: f64,
    /// Total input tokens for this model
    #[serde(default)]
    pub input_tokens: u64,
    /// Total cached input tokens for this model
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Total output tokens for this model
    #[serde(default)]
    pub output_tokens: u64,
    /// Total tokens for this model
    pub total_tokens: u64,
    /// Number of LLM responses for this model.
    pub request_count: usize,
}

impl Default for CostSummary {
    fn default() -> Self {
        Self {
            session_cost_usd: 0.0,
            daily_cost_usd: 0.0,
            monthly_cost_usd: 0.0,
            total_tokens: 0,
            request_count: 0,
            by_model: std::collections::HashMap::new(),
            by_agent: std::collections::HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_record_task_attribution_roundtrips_existing_field() {
        let usage = TokenUsage {
            model: "test/model".into(),
            input_tokens: 100,
            output_tokens: 50,
            cached_input_tokens: 0,
            total_tokens: 150,
            cost_usd: 0.0002,
            pricing_available: true,
            timestamp: "2026-06-30T00:00:00Z".parse().unwrap(),
        };
        let record = CostRecord {
            id: "rec-1".into(),
            session_id: "session-123".into(),
            agent_alias: Some("agent-a".into()),
            task_id: Some("goal-123".into()),
            usage,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            json.contains("goal_task_id"),
            "serialized key stays compatible with existing JSONL"
        );
        let parsed: CostRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.agent_alias.as_deref(), Some("agent-a"));
        assert_eq!(parsed.task_id.as_deref(), Some("goal-123"));
    }

    #[test]
    fn cost_record_task_attribution_accepts_generic_alias() {
        let json = r#"{
            "id": "rec-1",
            "usage": {
                "model": "test/model",
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15,
                "cost_usd": 0.001,
                "timestamp": "2026-06-30T00:00:00Z"
            },
            "session_id": "session-123",
            "agent_alias": "agent-a",
            "task_id": "task-123"
        }"#;
        let parsed: CostRecord = serde_json::from_str(json).unwrap();

        assert_eq!(parsed.task_id.as_deref(), Some("task-123"));
        assert!(parsed.usage.pricing_available);
    }
}
