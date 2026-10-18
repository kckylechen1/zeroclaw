//! `zeroclaw soul` — Soul diagnostics.

use anyhow::Result;
use zeroclaw_runtime::agent::soul_check::{
    ModelTarget, SCENARIOS, SoulCheckReport, planned_model_calls, run_soul_check,
};

use crate::config::Config;
use crate::gateway_helpers::ta;

/// Output format for the Soul check report.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable table.
    Table,
    /// Machine-readable JSON.
    Json,
}

/// Run the Soul check for `agent`. Setup problems are errors; findings in the
/// report are not, so the exit status stays zero for them.
pub async fn check(
    config: &Config,
    agent: &str,
    targets: &[String],
    judge: Option<&str>,
) -> Result<SoulCheckReport> {
    let targets = targets
        .iter()
        .map(|spec| ModelTarget::parse(spec))
        .collect::<Result<Vec<_>>>()?;
    let judge = match judge {
        Some(spec) => ModelTarget::parse(spec)?,
        None => ModelTarget::agent_default(config, agent)?,
    };
    let target_count = targets.len().max(1);
    print_cost_notice(agent, target_count);
    run_soul_check(config, agent, &targets, judge).await
}

/// One line on stderr, so `--format json` keeps stdout machine-readable.
fn print_cost_notice(agent: &str, targets: usize) {
    let replies = targets * SCENARIOS.len();
    eprintln!(
        "{}",
        ta(
            "soul-check-notice",
            &[
                ("agent", agent),
                ("calls", &planned_model_calls(targets).to_string()),
                ("replies", &replies.to_string()),
                ("judgements", &replies.to_string()),
            ],
            "Soul check: this makes real model calls.",
        )
    );
}

/// Render a report in the requested format.
pub fn print_report(report: &SoulCheckReport, format: OutputFormat) {
    match format {
        OutputFormat::Json => println!("{}", report.to_json()),
        OutputFormat::Table => println!("{}", report.render_table()),
    }
}
