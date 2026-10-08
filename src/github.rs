use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::checks::{Annotation, Check, CheckState, keep_annotation};
use crate::model::{Comment, PrFile, Thread, ViewedState};

/// A GraphQL variable: `String` goes through `gh api -f` (always a string),
/// `Raw` through `-F` (gh parses it, so numbers stay numbers).
enum Var<'a> {
    String(&'a str, &'a str),
    Raw(&'a str, String),
}

#[derive(Debug, Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
}

fn graphql<T: DeserializeOwned>(query: &str, vars: &[Var]) -> Result<T> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", "graphql", "-f", &format!("query={query}")]);
    for var in vars {
        match var {
            Var::String(name, value) => cmd.args(["-f", &format!("{name}={value}")]),
            Var::Raw(name, value) => cmd.args(["-F", &format!("{name}={value}")]),
        };
    }
    let output = cmd
        .output()
        .context("failed to run `gh`. Is the GitHub CLI installed?")?;
    if !output.status.success() {
        bail!(
            "gh api graphql failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let envelope: GraphQlEnvelope<T> = serde_json::from_slice(&output.stdout)
        .context("could not parse gh api graphql response")?;
    if let Some(errors) = envelope.errors {
        let messages: Vec<String> = errors.into_iter().map(|e| e.message).collect();
        bail!("GitHub API returned errors: {}", messages.join("; "));
    }
    envelope.data.context("GitHub API returned no data")
}

#[derive(Debug, Deserialize)]
struct RepositoryData<P> {
    repository: Option<RepositoryNode<P>>,
}

#[derive(Debug, Deserialize)]
struct RepositoryNode<P> {
    #[serde(rename = "pullRequest")]
    pull_request: Option<P>,
}

fn pull_request_of<P>(data: RepositoryData<P>, owner: &str, repo: &str, pr: u64) -> Result<P> {
    data.repository
        .and_then(|r| r.pull_request)
        .with_context(|| format!("PR #{pr} not found in {owner}/{repo}"))
}

#[derive(Debug, Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Connection<N> {
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
    nodes: Vec<N>,
}

fn paged_vars<'a>(owner: &'a str, repo: &'a str, pr: u64, after: Option<&'a str>) -> Vec<Var<'a>> {
    let mut vars = vec![
        Var::String("owner", owner),
        Var::String("repo", repo),
        Var::Raw("pr", pr.to_string()),
    ];
    if let Some(cursor) = after {
        vars.push(Var::String("after", cursor));
    }
    vars
}

// ---- review threads ----

const THREADS_QUERY: &str = r#"
query($owner: String!, $repo: String!, $pr: Int!, $after: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $pr) {
      reviewThreads(first: 50, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          isResolved
          isOutdated
          path
          line
          originalLine
          comments(first: 100) {
            nodes {
              author { login __typename }
              body
              diffHunk
              createdAt
              url
            }
          }
        }
      }
    }
  }
}
"#;

#[derive(Debug, Deserialize)]
struct ThreadsPullRequest {
    #[serde(rename = "reviewThreads")]
    review_threads: Connection<RawThread>,
}

#[derive(Debug, Deserialize)]
struct RawThread {
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    #[serde(rename = "isOutdated")]
    is_outdated: bool,
    path: Option<String>,
    line: Option<i64>,
    #[serde(rename = "originalLine")]
    original_line: Option<i64>,
    comments: RawComments,
}

#[derive(Debug, Deserialize)]
struct RawComments {
    nodes: Vec<RawComment>,
}

#[derive(Debug, Deserialize)]
struct RawComment {
    author: Option<RawAuthor>,
    body: String,
    #[serde(rename = "diffHunk")]
    diff_hunk: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawAuthor {
    login: String,
    #[serde(rename = "__typename")]
    typename: String,
}

impl From<RawThread> for Thread {
    fn from(raw: RawThread) -> Self {
        Thread {
            pr: 0,
            is_resolved: raw.is_resolved,
            is_outdated: raw.is_outdated,
            path: raw.path,
            line: raw.line,
            original_line: raw.original_line,
            comments: raw
                .comments
                .nodes
                .into_iter()
                .map(|c| {
                    let (author, author_is_bot) = match c.author {
                        Some(a) => (a.login, a.typename == "Bot"),
                        None => ("ghost".to_string(), false),
                    };
                    Comment {
                        author,
                        author_is_bot,
                        body: c.body,
                        diff_hunk: c.diff_hunk,
                        created_at: c.created_at,
                        url: c.url,
                    }
                })
                .collect(),
        }
    }
}

pub struct PullRequestThreads {
    pub threads: Vec<Thread>,
}

pub fn fetch_threads(owner: &str, repo: &str, pr: u64) -> Result<PullRequestThreads> {
    let mut threads = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let data: RepositoryData<ThreadsPullRequest> = graphql(
            THREADS_QUERY,
            &paged_vars(owner, repo, pr, after.as_deref()),
        )?;
        let page = pull_request_of(data, owner, repo, pr)?.review_threads;
        threads.extend(page.nodes.into_iter().map(Thread::from));
        if !page.page_info.has_next_page {
            break;
        }
        after = page.page_info.end_cursor;
    }
    Ok(PullRequestThreads { threads })
}

// ---- changed files and Viewed state ----

const FILES_QUERY: &str = r#"
query($owner: String!, $repo: String!, $pr: Int!, $after: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $pr) {
      id
      title
      url
      headRefOid
      headRepository { nameWithOwner }
      files(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { path additions deletions viewerViewedState }
      }
    }
  }
}
"#;

#[derive(Debug, Deserialize)]
struct FilesPullRequest {
    id: String,
    title: String,
    url: String,
    #[serde(rename = "headRefOid")]
    head_oid: String,
    #[serde(rename = "headRepository")]
    head_repository: Option<NameWithOwner>,
    files: Connection<RawFile>,
}

#[derive(Debug, Deserialize)]
struct NameWithOwner {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Debug, Deserialize)]
struct RawFile {
    path: String,
    additions: u64,
    deletions: u64,
    #[serde(rename = "viewerViewedState")]
    viewed: ViewedState,
}

pub struct PullRequestFiles {
    /// GraphQL node id — the mutations take this, not the PR number.
    pub id: String,
    pub title: String,
    pub url: String,
    /// Where the PR's head lives — a fork's, for PRs from forks — and its
    /// commit, for reading files as of the PR.
    pub head_repo: Option<String>,
    pub head_oid: String,
    pub files: Vec<PrFile>,
}

pub fn fetch_files(owner: &str, repo: &str, pr: u64) -> Result<PullRequestFiles> {
    let mut files = Vec::new();
    let mut head: Option<(String, String, String, Option<String>, String)> = None;
    let mut after: Option<String> = None;

    loop {
        let data: RepositoryData<FilesPullRequest> =
            graphql(FILES_QUERY, &paged_vars(owner, repo, pr, after.as_deref()))?;
        let pull_request = pull_request_of(data, owner, repo, pr)?;
        if head.is_none() {
            head = Some((
                pull_request.id,
                pull_request.title,
                pull_request.url,
                pull_request.head_repository.map(|r| r.name_with_owner),
                pull_request.head_oid,
            ));
        }
        let page = pull_request.files;
        files.extend(page.nodes.into_iter().map(|f| PrFile {
            pr: 0,
            path: f.path,
            additions: f.additions,
            deletions: f.deletions,
            viewed: f.viewed,
            generated: None,
        }));
        if !page.page_info.has_next_page {
            break;
        }
        after = page.page_info.end_cursor;
    }

    let (id, title, url, head_repo, head_oid) = head.context("PR response never returned an id")?;
    Ok(PullRequestFiles {
        id,
        title,
        url,
        head_repo,
        head_oid,
        files,
    })
}

#[derive(Debug, Deserialize)]
struct BlobRepository {
    repository: Option<BlobObject>,
}

#[derive(Debug, Deserialize)]
struct BlobObject {
    object: Option<Blob>,
}

#[derive(Debug, Deserialize)]
struct Blob {
    text: Option<String>,
}

/// The root `.gitattributes` of `repo` (`owner/name`) at `oid`, if it has one.
pub fn fetch_gitattributes(repo: &str, oid: &str) -> Result<Option<String>> {
    let (owner, name) = repo
        .split_once('/')
        .with_context(|| format!("unexpected repository name: {repo}"))?;
    let expression = format!("{oid}:.gitattributes");
    let data: BlobRepository = graphql(
        "query($owner: String!, $name: String!, $expression: String!) {
           repository(owner: $owner, name: $name) {
             object(expression: $expression) { ... on Blob { text } }
           }
         }",
        &[
            Var::String("owner", owner),
            Var::String("name", name),
            Var::String("expression", &expression),
        ],
    )?;
    Ok(data.repository.and_then(|r| r.object).and_then(|o| o.text))
}

/// Marks (or unmarks) every path as viewed in a single request: one aliased
/// mutation per path in one document, so a whole directory costs one round
/// trip instead of one `gh` process per file. Paths travel as variables,
/// never spliced into the query text.
pub fn set_viewed(pull_request_id: &str, paths: &[&str], viewed: bool) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mutation = if viewed {
        "markFileAsViewed"
    } else {
        "unmarkFileAsViewed"
    };
    let params: Vec<String> = (0..paths.len())
        .map(|i| format!("$p{i}: String!"))
        .collect();
    let fields: Vec<String> = (0..paths.len())
        .map(|i| {
            format!("f{i}: {mutation}(input: {{pullRequestId: $pr, path: $p{i}}}) {{ clientMutationId }}")
        })
        .collect();
    let query = format!(
        "mutation($pr: ID!, {}) {{ {} }}",
        params.join(", "),
        fields.join(" ")
    );

    let names: Vec<String> = (0..paths.len()).map(|i| format!("p{i}")).collect();
    let mut vars = vec![Var::String("pr", pull_request_id)];
    vars.extend(
        names
            .iter()
            .zip(paths)
            .map(|(name, path)| Var::String(name, path)),
    );

    graphql::<serde_json::Value>(&query, &vars).map(|_| ())
}

// ---- CI checks ----

const CHECKS_QUERY: &str = r#"
query($owner: String!, $repo: String!, $pr: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $pr) {
      commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 100) { nodes {
        __typename
        ... on CheckRun {
          databaseId name status conclusion detailsUrl
          checkSuite { app { name slug } workflowRun { workflow { name } } }
          steps(first: 100) { nodes { name conclusion } }
          annotations(first: 50) { nodes { path annotationLevel message location { start { line } } } }
        }
        ... on StatusContext { context state targetUrl }
      } } } } } }
    }
  }
}
"#;

#[derive(Debug, Deserialize)]
struct ChecksPullRequest {
    commits: Nodes<CommitNode>,
}

#[derive(Debug, Deserialize)]
struct Nodes<N> {
    nodes: Vec<N>,
}

#[derive(Debug, Deserialize)]
struct CommitNode {
    commit: CommitChecks,
}

#[derive(Debug, Deserialize)]
struct CommitChecks {
    #[serde(rename = "statusCheckRollup")]
    rollup: Option<Rollup>,
}

#[derive(Debug, Deserialize)]
struct Rollup {
    contexts: Nodes<RawContext>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum RawContext {
    CheckRun {
        #[serde(rename = "databaseId")]
        database_id: Option<u64>,
        name: String,
        status: Option<String>,
        conclusion: Option<String>,
        #[serde(rename = "detailsUrl")]
        details_url: Option<String>,
        #[serde(rename = "checkSuite")]
        check_suite: Option<CheckSuite>,
        steps: Option<Nodes<RawStep>>,
        annotations: Option<Nodes<RawAnnotation>>,
    },
    StatusContext {
        context: String,
        state: String,
        #[serde(rename = "targetUrl")]
        target_url: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct CheckSuite {
    app: Option<App>,
    #[serde(rename = "workflowRun")]
    workflow_run: Option<WorkflowRun>,
}

#[derive(Debug, Deserialize)]
struct App {
    name: String,
    slug: String,
}

#[derive(Debug, Deserialize)]
struct WorkflowRun {
    workflow: NamedNode,
}

#[derive(Debug, Deserialize)]
struct NamedNode {
    name: String,
}

#[derive(Debug, Deserialize)]
struct RawStep {
    name: String,
    conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawAnnotation {
    path: String,
    #[serde(rename = "annotationLevel")]
    level: Option<String>,
    message: String,
    location: Option<AnnotationLocation>,
}

#[derive(Debug, Deserialize)]
struct AnnotationLocation {
    start: AnnotationPosition,
}

#[derive(Debug, Deserialize)]
struct AnnotationPosition {
    line: Option<u64>,
}

impl From<RawContext> for Check {
    fn from(raw: RawContext) -> Self {
        match raw {
            RawContext::CheckRun {
                database_id,
                name,
                status,
                conclusion,
                details_url,
                check_suite,
                steps,
                annotations,
            } => {
                let state = CheckState::from_github(status.as_deref(), conclusion.as_deref());
                let (app, workflow) = match check_suite {
                    Some(suite) => (suite.app, suite.workflow_run.map(|w| w.workflow.name)),
                    None => (None, None),
                };
                let is_actions = app.as_ref().is_some_and(|a| a.slug == "github-actions");
                let failed_step = steps.and_then(|s| {
                    s.nodes
                        .into_iter()
                        .find(|step| step.conclusion.as_deref() == Some("FAILURE"))
                        .map(|step| step.name)
                });
                Check {
                    pr: 0,
                    name,
                    source: workflow.or(app.map(|a| a.name)),
                    state,
                    url: details_url,
                    failed_step,
                    job_id: database_id.filter(|_| is_actions),
                    annotations: annotations
                        .map(|a| a.nodes)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|a| keep_annotation(a.level.as_deref().unwrap_or(""), &a.message))
                        .map(|a| Annotation {
                            level: a.level.unwrap_or_default().to_lowercase(),
                            path: a.path,
                            line: a.location.and_then(|l| l.start.line),
                            message: a.message,
                        })
                        .collect(),
                }
            }
            RawContext::StatusContext {
                context,
                state,
                target_url,
            } => Check {
                pr: 0,
                name: context,
                source: None,
                state: CheckState::from_github(None, Some(&state)),
                url: target_url,
                failed_step: None,
                job_id: None,
                annotations: Vec::new(),
            },
        }
    }
}

/// Every check and commit status on the PR's latest commit, in the order
/// worth reading: failed, running, passed, neutral, skipped.
pub fn fetch_checks(owner: &str, repo: &str, pr: u64) -> Result<Vec<Check>> {
    let data: RepositoryData<ChecksPullRequest> = graphql(
        CHECKS_QUERY,
        &[
            Var::String("owner", owner),
            Var::String("repo", repo),
            Var::Raw("pr", pr.to_string()),
        ],
    )?;
    let pull_request = pull_request_of(data, owner, repo, pr)?;
    let mut checks: Vec<Check> = pull_request
        .commits
        .nodes
        .into_iter()
        .filter_map(|c| c.commit.rollup)
        .flat_map(|r| r.contexts.nodes)
        .map(Check::from)
        .collect();
    for check in &mut checks {
        // What failed before what merely warned.
        check.annotations.sort_by_key(|a| a.level != "failure");
    }
    checks.sort_by(|a, b| a.state.cmp(&b.state).then_with(|| a.source.cmp(&b.source)));
    Ok(checks)
}

/// The raw log of a GitHub Actions job.
pub fn fetch_job_log(owner: &str, repo: &str, job_id: u64) -> Result<String> {
    let output = Command::new("gh")
        .args([
            "api",
            &format!("repos/{owner}/{repo}/actions/jobs/{job_id}/logs"),
        ])
        .output()
        .context("failed to run `gh`")?;
    if !output.status.success() {
        bail!(
            "couldn't fetch the job log: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Infers `owner/repo` from the current directory's `origin` remote via `gh`.
pub fn infer_repo() -> Result<(String, String)> {
    let output = Command::new("gh")
        .args([
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "-q",
            ".nameWithOwner",
        ])
        .output()
        .context("failed to run `gh repo view`")?;
    if !output.status.success() {
        bail!(
            "could not infer the repository from the current directory: {}\n\
             pass --repo owner/name explicitly.",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let name_with_owner = String::from_utf8(output.stdout)?.trim().to_string();
    name_with_owner
        .split_once('/')
        .map(|(o, r)| (o.to_string(), r.to_string()))
        .with_context(|| format!("unexpected `gh repo view` output: {name_with_owner}"))
}
