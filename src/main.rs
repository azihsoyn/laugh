mod files_view;
mod format;
mod github;
mod logo;
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
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use model::{PrFile, Thread};
use serde::Serialize;

/// Your code, your agent and your review — in the same terminal.
///
/// Review pull requests without opening a browser: every review thread,
/// resolved ones included, and the changed files with GitHub's own Viewed
/// checkboxes — for one PR or a related set across repositories.
///
/// `laugh <pr>` with no subcommand is `laugh pr <pr>`.
#[derive(Parser, Debug)]
#[command(name = "laugh", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open one or more pull requests: changed files with their Viewed state,
    /// and every review thread (resolved and outdated included). Switch
    /// screens with 1 / 2; with several PRs, [ / ] steps between all of
    /// them together and one at a time.
    Pr(PrArgs),
}

#[derive(Args, Debug)]
struct PrArgs {
    /// One or more PRs: 123, owner/repo#123, or https://github.com/owner/repo/pull/123.
    /// Bare numbers use --repo, or the current directory's repository.
    #[arg(required = true, num_args = 1..)]
    prs: Vec<String>,

    /// owner/repo for bare PR numbers; inferred from the current directory's git remote if omitted
    #[arg(long)]
    repo: Option<String>,

    /// Print the files and review threads as JSON and exit, instead of launching the TUI
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrRef {
    owner: String,
    repo: String,
    number: u64,
}

impl PrRef {
    fn full(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }
}

fn split_repo(r: &str) -> Option<(String, String)> {
    let (owner, name) = r.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| (owner.to_string(), name.to_string()))
}

/// Parses one PR argument. `default_repo` is only consulted for bare numbers,
/// so the current directory needn't be a repository when every PR is spelled
/// out in full.
fn parse_pr_ref(
    arg: &str,
    default_repo: &mut dyn FnMut() -> Result<(String, String)>,
) -> Result<PrRef> {
    for prefix in ["https://github.com/", "http://github.com/"] {
        if let Some(rest) = arg.strip_prefix(prefix) {
            // Links copied from GitHub often carry `#discussion_r…` or `?w=1`.
            let rest = rest.split(['#', '?']).next().unwrap_or(rest);
            let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
            let pull_idx = parts
                .iter()
                .position(|s| *s == "pull")
                .with_context(|| format!("not a PR URL: {arg}"))?;
            anyhow::ensure!(pull_idx >= 2, "not a PR URL: {arg}");
            let number: u64 = parts
                .get(pull_idx + 1)
                .with_context(|| format!("not a PR URL: {arg}"))?
                .parse()
                .with_context(|| format!("not a PR URL: {arg}"))?;
            return Ok(PrRef {
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
                number,
            });
        }
    }

    if let Some((repo, number)) = arg.rsplit_once('#')
        && !repo.is_empty()
    {
        let (owner, repo) =
            split_repo(repo).with_context(|| format!("expected owner/repo#123, got: {arg}"))?;
        let number = number
            .parse()
            .with_context(|| format!("expected owner/repo#123, got: {arg}"))?;
        return Ok(PrRef {
            owner,
            repo,
            number,
        });
    }

    let number: u64 = arg
        .trim_start_matches('#')
        .parse()
        .with_context(|| format!("not a PR: {arg} (use 123, owner/repo#123, or a PR URL)"))?;
    let (owner, repo) = default_repo()?;
    Ok(PrRef {
        owner,
        repo,
        number,
    })
}

/// Short switcher labels: `repo#N`, widened to `owner/repo#N` only where two
/// PRs come from same-named repositories under different owners.
fn short_labels(refs: &[PrRef]) -> Vec<String> {
    refs.iter()
        .map(|r| {
            let ambiguous = refs.iter().any(|o| o.repo == r.repo && o.owner != r.owner);
            if ambiguous {
                r.full()
            } else {
                format!("{}#{}", r.repo, r.number)
            }
        })
        .collect()
}

#[derive(Serialize)]
struct Output<'a> {
    prs: Vec<PrJson<'a>>,
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

struct Fetched {
    files: github::PullRequestFiles,
    threads: github::PullRequestThreads,
}

fn fetch_all(refs: &[PrRef]) -> Result<Vec<Fetched>> {
    // Every PR's files and threads are independent calls; run them all at once.
    let results: Vec<_> = thread::scope(|s| {
        let handles: Vec<_> = refs
            .iter()
            .map(|r| {
                (
                    s.spawn(|| github::fetch_files(&r.owner, &r.repo, r.number)),
                    s.spawn(|| github::fetch_threads(&r.owner, &r.repo, r.number)),
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|(f, t)| {
                (
                    f.join().map_err(|_| anyhow!("file fetch panicked")),
                    t.join().map_err(|_| anyhow!("thread fetch panicked")),
                )
            })
            .collect()
    });
    results
        .into_iter()
        .zip(refs)
        .map(|((files, threads), r)| {
            Ok(Fetched {
                files: files?
                    .with_context(|| format!("fetching changed files for {}", r.full()))?,
                threads: threads?
                    .with_context(|| format!("fetching review threads for {}", r.full()))?,
            })
        })
        .collect()
}

fn run_pr(args: PrArgs) -> Result<()> {
    let mut inferred: Option<(String, String)> = None;
    let mut default_repo = || -> Result<(String, String)> {
        if let Some(r) = &inferred {
            return Ok(r.clone());
        }
        let r = match args.repo.as_deref() {
            Some(r) => split_repo(r)
                .with_context(|| format!("--repo must look like owner/name, got: {r}"))?,
            None => github::infer_repo()?,
        };
        inferred = Some(r.clone());
        Ok(r)
    };
    let mut refs: Vec<PrRef> = Vec::new();
    for arg in &args.prs {
        let r = parse_pr_ref(arg, &mut default_repo)?;
        if !refs.contains(&r) {
            refs.push(r);
        }
    }

    let fetched = fetch_all(&refs)?;

    if args.json {
        let output = Output {
            prs: fetched
                .iter()
                .zip(&refs)
                .map(|(f, r)| PrJson {
                    pr: PrMetaJson {
                        owner: &r.owner,
                        repo: &r.repo,
                        number: r.number,
                        title: &f.files.title,
                        url: &f.files.url,
                    },
                    files: &f.files.files,
                    threads: &f.threads.threads,
                })
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    let labels = short_labels(&refs);
    let mut headers = Vec::new();
    let mut pr_ids = Vec::new();
    let mut files = Vec::new();
    let mut threads = Vec::new();
    for (i, (f, r)) in fetched.into_iter().zip(&refs).enumerate() {
        pr_ids.push(f.files.id);
        files.extend(f.files.files.into_iter().map(|mut file| {
            file.pr = i;
            file
        }));
        threads.extend(f.threads.threads.into_iter().map(|mut t| {
            t.pr = i;
            t
        }));
        headers.push(pr_app::PrHeader {
            repo: format!("{}/{}", r.owner, r.repo),
            number: r.number,
            title: f.files.title,
            label: labels[i].clone(),
        });
    }

    pr_app::run(
        headers,
        files_view::FilesView::new(pr_ids, &labels, files),
        threads_view::ThreadsView::new(threads, labels),
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
    let matches = Cli::command()
        .before_help(logo::colored())
        .get_matches_from(with_default_subcommand(std::env::args_os().collect()));
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
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

    fn parse(arg: &str) -> Result<PrRef> {
        parse_pr_ref(arg, &mut || Ok(("me".to_string(), "here".to_string())))
    }

    fn pr(owner: &str, repo: &str, number: u64) -> PrRef {
        PrRef {
            owner: owner.into(),
            repo: repo.into(),
            number,
        }
    }

    #[test]
    fn pr_arguments_in_every_spelling() {
        assert_eq!(parse("12").unwrap(), pr("me", "here", 12));
        assert_eq!(parse("#12").unwrap(), pr("me", "here", 12));
        assert_eq!(parse("acme/infra#45").unwrap(), pr("acme", "infra", 45));
        assert_eq!(
            parse("https://github.com/acme/design/pull/67/files").unwrap(),
            pr("acme", "design", 67)
        );
        assert_eq!(
            parse("https://github.com/acme/app/pull/12#discussion_r123").unwrap(),
            pr("acme", "app", 12)
        );
        assert_eq!(
            parse("https://github.com/acme/app/pull/12/files?w=1").unwrap(),
            pr("acme", "app", 12)
        );
        assert!(parse("acme#4").is_err());
        assert!(parse("acme/infra#x").is_err());
    }

    #[test]
    fn full_spellings_never_ask_for_the_current_repo() {
        let mut asked = false;
        let r = parse_pr_ref("acme/infra#45", &mut || {
            asked = true;
            Ok(("me".into(), "here".into()))
        });
        assert!(r.is_ok() && !asked);
    }

    #[test]
    fn labels_stay_short_unless_repo_names_collide() {
        let refs = [
            pr("acme", "app", 1),
            pr("acme", "infra", 2),
            pr("other", "app", 3),
        ];
        assert_eq!(
            short_labels(&refs),
            ["acme/app#1", "infra#2", "other/app#3"]
        );
    }
}
