mod files_view;
mod format;
mod github;
mod model;
mod pr_app;
mod term;
mod theme;
mod threads_view;
mod ui;
mod viewed_sync;

use std::ffi::OsString;
use std::thread;

use anyhow::{Context, Result, anyhow};
use clap::{Args, CommandFactory, Parser, Subcommand};
use model::{PrFile, Thread};
use serde::Serialize;

/// The GitHub you'd want in a terminal: the things GitHub hides or makes
/// awkward, without opening a browser.
///
/// `laugh <pr>` with no subcommand is `laugh pr <pr>`.
#[derive(Parser, Debug)]
#[command(name = "laugh", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open a pull request: changed files with their Viewed state, and every
    /// review thread (resolved and outdated included). Switch with 1 / 2.
    Pr(PrArgs),
}

#[derive(Args, Debug)]
struct PrArgs {
    /// PR number (e.g. 123) or a full https://github.com/owner/repo/pull/123 URL
    pr: String,

    /// owner/repo to query; inferred from the current directory's git remote if omitted
    #[arg(long)]
    repo: Option<String>,

    /// Print the files and review threads as JSON and exit, instead of launching the TUI
    #[arg(long)]
    json: bool,
}

struct PrRef {
    owner: String,
    repo: String,
    number: u64,
}

impl PrRef {
    fn label(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }
}

fn parse_pr_ref(args: &PrArgs) -> Result<PrRef> {
    let pr_arg = args.pr.as_str();
    for prefix in ["https://github.com/", "http://github.com/"] {
        if let Some(rest) = pr_arg.strip_prefix(prefix) {
            let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
            let pull_idx = parts
                .iter()
                .position(|s| *s == "pull")
                .with_context(|| format!("not a PR URL: {pr_arg}"))?;
            anyhow::ensure!(pull_idx >= 2, "not a PR URL: {pr_arg}");
            let number: u64 = parts
                .get(pull_idx + 1)
                .with_context(|| format!("not a PR URL: {pr_arg}"))?
                .parse()
                .with_context(|| format!("not a PR URL: {pr_arg}"))?;
            return Ok(PrRef {
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
                number,
            });
        }
    }

    let number: u64 = pr_arg
        .trim_start_matches('#')
        .parse()
        .context("PR must be a number (123) or a https://github.com/owner/repo/pull/123 URL")?;

    let (owner, repo) = match args.repo.as_deref() {
        Some(r) => r
            .split_once('/')
            .map(|(o, n)| (o.to_string(), n.to_string()))
            .with_context(|| format!("--repo must look like owner/name, got: {r}"))?,
        None => github::infer_repo()?,
    };

    Ok(PrRef {
        owner,
        repo,
        number,
    })
}

#[derive(Serialize)]
struct PrJson<'a> {
    pr: PrMetaJson<'a>,
    files: &'a [PrFile],
    threads: &'a [Thread],
}

#[derive(Serialize)]
struct PrMetaJson<'a> {
    owner: &'a str,
    repo: &'a str,
    number: u64,
    title: &'a str,
    url: &'a str,
}

fn run_pr(args: PrArgs) -> Result<()> {
    let pr_ref = parse_pr_ref(&args)?;

    // Two independent GraphQL calls; fetch them side by side.
    let (files, threads) = thread::scope(|s| {
        let files = s.spawn(|| github::fetch_files(&pr_ref.owner, &pr_ref.repo, pr_ref.number));
        let threads = s.spawn(|| github::fetch_threads(&pr_ref.owner, &pr_ref.repo, pr_ref.number));
        (
            files.join().map_err(|_| anyhow!("file fetch panicked")),
            threads.join().map_err(|_| anyhow!("thread fetch panicked")),
        )
    });
    let files = files?.with_context(|| format!("fetching changed files for {}", pr_ref.label()))?;
    let threads =
        threads?.with_context(|| format!("fetching review threads for {}", pr_ref.label()))?;

    if args.json {
        let output = PrJson {
            pr: PrMetaJson {
                owner: &pr_ref.owner,
                repo: &pr_ref.repo,
                number: pr_ref.number,
                title: &files.title,
                url: &files.url,
            },
            files: &files.files,
            threads: &threads.threads,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    pr_app::run(
        pr_app::PrHeader {
            repo: format!("{}/{}", pr_ref.owner, pr_ref.repo),
            number: pr_ref.number,
            title: files.title,
        },
        files_view::FilesView::new(files.id, files.files),
        threads_view::ThreadsView::new(threads.threads),
    )
}

/// Inserts the default `pr` subcommand unless the first argument already
/// names a subcommand or asks for help/version, so `laugh 123` and
/// `laugh --repo o/r 123` keep working.
fn with_default_subcommand(mut args: Vec<OsString>) -> Vec<OsString> {
    let explicit = args.get(1).and_then(|a| a.to_str()).is_none_or(|first| {
        Cli::command()
            .get_subcommands()
            .any(|s| s.get_name() == first)
            || matches!(first, "help" | "-h" | "--help" | "-V" | "--version")
    });
    if !explicit {
        args.insert(1, "pr".into());
    }
    args
}

fn main() -> Result<()> {
    let cli = Cli::parse_from(with_default_subcommand(std::env::args_os().collect()));
    match cli.command {
        Command::Pr(args) => run_pr(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Vec<String> {
        let os: Vec<OsString> = args.iter().map(OsString::from).collect();
        with_default_subcommand(os)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn bare_pr_defaults_to_the_pr_subcommand() {
        assert_eq!(parsed(&["laugh", "123"]), ["laugh", "pr", "123"]);
        assert_eq!(
            parsed(&["laugh", "--repo", "o/r", "123"]),
            ["laugh", "pr", "--repo", "o/r", "123"]
        );
    }

    #[test]
    fn explicit_subcommands_and_help_are_left_alone() {
        assert_eq!(parsed(&["laugh", "pr", "1"]), ["laugh", "pr", "1"]);
        assert_eq!(parsed(&["laugh", "--help"]), ["laugh", "--help"]);
        assert_eq!(parsed(&["laugh"]), ["laugh"]);
    }
}
