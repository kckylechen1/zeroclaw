//! Development-only entry point for deterministic agent evaluation.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use zeroclaw_eval::Mode;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run all JSON trace fixtures in a suite.
    Run {
        #[arg(long, default_value = "evals")]
        suite: PathBuf,
        #[arg(long, default_value = "replay")]
        mode: Mode,
        #[arg(long, value_enum, default_value = "table")]
        format: OutputFormat,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Table,
    Json,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let Command::Run {
        suite,
        mode,
        format,
    } = Cli::parse().command;
    let report = zeroclaw_eval::run_suite(&suite, mode).await?;
    match format {
        OutputFormat::Table => print!("{}", report.render_table()),
        OutputFormat::Json => println!("{}", report.to_json()),
    }
    if !report.all_passed() {
        std::process::exit(1);
    }
    Ok(())
}
