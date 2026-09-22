use std::path::PathBuf;
use std::process::ExitCode;

use crate::interact::Cancelled;
use crate::tidy::{self, Options};
use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};

#[derive(clap::Parser)]
#[command(name = "karu", about = "Prune leftover git branches and worktrees")]
struct Cli {
    /// Repository path (default: current directory)
    path: Option<PathBuf>,

    /// Delete without the bulk confirmation (uncertain ones are still asked)
    #[arg(long)]
    yes: bool,

    /// Print judgments as JSON and do not delete
    #[arg(long, visible_alias = "dry-run", conflicts_with = "yes")]
    json: bool,

    /// Skip git fetch --prune
    #[arg(long)]
    no_fetch: bool,

    /// Delete dirty worktrees without asking about each one
    #[arg(long)]
    force: bool,

    /// Never send anything to Jev, whatever `karu.jev` says
    #[arg(long)]
    no_jev: bool,
}

pub fn run() -> Result<ExitCode> {
    let cli = Cli::from_arg_matches(&Cli::command().bin_name(invoked_as()).get_matches())
        .unwrap_or_else(|err| err.exit());
    exit_code(tidy::run(Options {
        path: cli.path.unwrap_or_else(|| PathBuf::from(".")),
        yes: cli.yes,
        json: cli.json,
        fetch: !cli.no_fetch,
        force: cli.force,
        no_jev: cli.no_jev,
        api_key: std::env::var("TYPESAFE_API_KEY")
            .ok()
            .filter(|s| !s.is_empty()),
    }))
}

fn exit_code(result: Result<()>) -> Result<ExitCode> {
    match result {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(error) if error.is::<Cancelled>() => Ok(ExitCode::from(130)),
        Err(error) => Err(error),
    }
}

/// `git karu` runs this program as `git-karu`; usage should show what was typed.
fn invoked_as() -> &'static str {
    let is_git_subcommand = std::env::args_os()
        .next()
        .map(PathBuf::from)
        .and_then(|path| path.file_stem().map(|stem| stem == "git-karu"))
        .unwrap_or(false);
    if is_git_subcommand {
        "git karu"
    } else {
        "karu"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_exits_successfully() {
        assert_eq!(exit_code(Ok(())).unwrap(), ExitCode::SUCCESS);
    }

    #[test]
    fn cancellation_exits_without_an_error_even_with_context() {
        let error = anyhow::Error::new(Cancelled).context("asking about a branch");
        assert_eq!(exit_code(Err(error)).unwrap(), ExitCode::from(130));
    }

    #[test]
    fn other_errors_keep_their_diagnostic_even_if_the_message_matches() {
        let error = anyhow::anyhow!(Cancelled.to_string()).context("failed to read the selection");
        let diagnostic = format!("{error:#}");
        assert_eq!(
            format!("{:#}", exit_code(Err(error)).unwrap_err()),
            diagnostic
        );
    }
}
