//! An order to read a PR's changed files in: what others depend on before
//! what uses it, each test right after the code it tests, config, docs and
//! generated files last.
//!
//! Without help it goes by what files are (a schema before the code, the
//! code before its test). With prognost's plan of the change it also goes by
//! who calls whom: a changed function's file before the files that call it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::model::PrFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Contract,
    Source,
    Test,
    Config,
    Docs,
    Generated,
}

impl Kind {
    fn reason(self) -> &'static str {
        match self {
            Kind::Contract => "schema / types",
            Kind::Source => "code",
            Kind::Test => "test",
            Kind::Config => "config",
            Kind::Docs => "docs",
            Kind::Generated => "generated",
        }
    }
}

fn kind(file: &PrFile) -> Kind {
    let path = file.path.to_lowercase();
    let name = path.rsplit('/').next().unwrap_or(&path);
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    if file.generated.is_some() {
        Kind::Generated
    } else if is_test(&path) {
        Kind::Test
    } else if matches!(ext, "sql" | "proto" | "graphql" | "gql" | "prisma")
        || name.ends_with(".d.ts")
        || path.contains("migration")
        || path.contains("/schema")
        || path.starts_with("schema")
        || matches!(name, "types.ts" | "types.rs" | "model.rs" | "models.py")
        || path.contains("/types/")
    {
        Kind::Contract
    } else if matches!(ext, "md" | "mdx" | "rst" | "txt" | "adoc")
        || path.starts_with("docs/")
        || path.starts_with(".changeset/")
    {
        Kind::Docs
    } else if path.starts_with(".github/")
        || matches!(
            ext,
            "json" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "lock" | "jsonnet"
        )
        || matches!(name, "dockerfile" | "makefile")
        || name.contains(".config.")
    {
        Kind::Config
    } else {
        Kind::Source
    }
}

fn is_test(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.contains(".test.")
        || name.contains(".spec.")
        || name.contains("_test.")
        || name.starts_with("test_")
        || path.contains("__tests__/")
        || path.starts_with("tests/")
        || path.contains("/tests/")
}

/// The source file a test is about: `src/__tests__/a.test.ts` → `src/a`.
fn stem(path: &str) -> String {
    let path = path.replace("__tests__/", "").replace("/tests/", "/");
    let path = path.strip_prefix("tests/").unwrap_or(&path).to_string();
    let (dir, name) = match path.rsplit_once('/') {
        Some((d, n)) => (format!("{d}/"), n.to_string()),
        None => (String::new(), path.clone()),
    };
    let mut base = name.split('.').next().unwrap_or(&name).to_string();
    for suffix in ["_test", "_spec"] {
        if let Some(b) = base.strip_suffix(suffix) {
            base = b.to_string();
        }
    }
    if let Some(b) = base.strip_prefix("test_") {
        base = b.to_string();
    }
    format!("{dir}{base}")
}

/// Ranking key: kind first, but a test whose code is in the PR sorts right
/// after that code.
fn heuristic_key(file: &PrFile, sources: &BTreeSet<String>) -> (Kind, String, u8) {
    let k = kind(file);
    let s = stem(&file.path);
    match k {
        Kind::Test if sources.contains(&s) => (Kind::Source, s, 1),
        Kind::Source => (Kind::Source, s, 0),
        _ => (k, file.path.clone(), 0),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub file: usize,
    pub reason: String,
}

/// Orders `indices` (files of one PR): heuristics only when `deps` is empty;
/// otherwise every `(callee_file, caller_file)` edge puts the callee first.
pub fn order(files: &[PrFile], indices: &[usize], deps: &[(usize, usize)]) -> Vec<Step> {
    let sources: BTreeSet<String> = indices
        .iter()
        .filter(|&&i| kind(&files[i]) == Kind::Source)
        .map(|&i| stem(&files[i].path))
        .collect();
    let key = |i: usize| heuristic_key(&files[i], &sources);

    let mut incoming: HashMap<usize, usize> = indices.iter().map(|&i| (i, 0)).collect();
    let mut outgoing: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut uses: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut used_by: HashMap<usize, Vec<usize>> = HashMap::new();
    for &(callee, caller) in deps.iter().collect::<BTreeSet<_>>() {
        if callee == caller || !incoming.contains_key(&callee) || !incoming.contains_key(&caller) {
            continue;
        }
        outgoing.entry(callee).or_default().push(caller);
        *incoming.get_mut(&caller).expect("present") += 1;
        uses.entry(caller).or_default().push(callee);
        used_by.entry(callee).or_default().push(caller);
    }

    // Kahn's algorithm, always taking the best-ranked ready file; a cycle
    // is broken by taking the best-ranked file still waiting.
    let mut ready: BTreeMap<(Kind, String, u8, usize), usize> = BTreeMap::new();
    let mut waiting: BTreeMap<(Kind, String, u8, usize), usize> = BTreeMap::new();
    for &i in indices {
        let (k, s, t) = key(i);
        let slot = if incoming[&i] == 0 {
            &mut ready
        } else {
            &mut waiting
        };
        slot.insert((k, s, t, i), i);
    }
    let mut out = Vec::new();
    while let Some((_, i)) = ready.pop_first().or_else(|| waiting.pop_first()) {
        let (k, s, t) = key(i);
        waiting.remove(&(k, s, t, i));
        let name = |j: usize| files[j].file_name().to_string();
        let reason = if let Some(callees) = uses.get(&i) {
            format!(
                "uses {}",
                callees
                    .iter()
                    .map(|&j| name(j))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else if let Some(callers) = used_by.get(&i) {
            format!(
                "used by {}",
                callers
                    .iter()
                    .map(|&j| name(j))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else if kind(&files[i]) == Kind::Test && sources.contains(&stem(&files[i].path)) {
            "its test".to_string()
        } else {
            kind(&files[i]).reason().to_string()
        };
        out.push(Step { file: i, reason });
        for &next in outgoing.get(&i).into_iter().flatten() {
            let n = incoming.get_mut(&next).expect("present");
            *n -= 1;
            if *n == 0 {
                let (k, s, t) = key(next);
                if waiting.remove(&(k, s.clone(), t, next)).is_some() {
                    ready.insert((k, s, t, next), next);
                }
            }
        }
    }
    out
}

// ---- prognost ----

#[derive(Debug, Deserialize)]
struct Plan {
    functions: Vec<PlanFunction>,
    calls: Vec<PlanCall>,
}

#[derive(Debug, Deserialize)]
struct PlanFunction {
    id: String,
    path: String,
}

#[derive(Debug, Deserialize)]
struct PlanCall {
    caller: String,
    callee: String,
}

/// `(callee_file, caller_file)` pairs, as indices into `files`, from a
/// prognost plan's calls between functions in different changed files.
fn deps_from_plan(plan: &Plan, files: &[PrFile], indices: &[usize]) -> Vec<(usize, usize)> {
    let by_path: HashMap<&str, usize> = indices
        .iter()
        .map(|&i| (files[i].path.as_str(), i))
        .collect();
    let file_of: HashMap<&str, usize> = plan
        .functions
        .iter()
        .filter_map(|f| Some((f.id.as_str(), *by_path.get(f.path.as_str())?)))
        .collect();
    plan.calls
        .iter()
        .filter_map(|c| {
            Some((
                *file_of.get(c.callee.as_str())?,
                *file_of.get(c.caller.as_str())?,
            ))
        })
        .filter(|(callee, caller)| callee != caller)
        .collect()
}

/// The prognost binary to use: `LAUGH_PROGNOST`, else `prognost` on PATH.
fn prognost_binary() -> Option<String> {
    if let Ok(path) = std::env::var("LAUGH_PROGNOST")
        && !path.is_empty()
    {
        return Some(path);
    }
    let found = Command::new("prognost").arg("--version").output().ok()?;
    found.status.success().then(|| "prognost".to_string())
}

/// Whether `dir` is a checkout of `owner/repo` with both commits present.
fn usable_checkout(dir: &Path, owner: &str, repo: &str, commits: &[&str]) -> bool {
    let Ok(out) = Command::new("git")
        .current_dir(dir)
        .args(["remote", "get-url", "origin"])
        .output()
    else {
        return false;
    };
    let url = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
    let want = format!("{owner}/{repo}").to_lowercase();
    let matches = url.trim_end_matches(".git").ends_with(&format!("/{want}"))
        || url.trim_end_matches(".git").ends_with(&format!(":{want}"));
    matches
        && commits.iter().all(|c| {
            Command::new("git")
                .current_dir(dir)
                .args(["cat-file", "-e", &format!("{c}^{{commit}}")])
                .status()
                .is_ok_and(|s| s.success())
        })
}

/// Runs `prognost plan` between the PR's base and head in the current
/// directory, if prognost is installed and the directory is a checkout of
/// the PR's repository with both commits — otherwise `Ok(None)`.
pub fn prognost_deps(
    files: &[PrFile],
    indices: &[usize],
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> Result<Option<Vec<(usize, usize)>>> {
    let Some(binary) = prognost_binary() else {
        return Ok(None);
    };
    let dir = std::env::current_dir()?;
    if !usable_checkout(&dir, owner, repo, &[base, head]) {
        return Ok(None);
    }
    let out = Command::new(&binary)
        .current_dir(&dir)
        .args(["plan", "--base", base, "--head", head, "--json"])
        .output()
        .context("failed to run prognost")?;
    if !out.status.success() {
        bail!(
            "prognost plan failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let plan: Plan =
        serde_json::from_slice(&out.stdout).context("could not read prognost's plan")?;
    Ok(Some(deps_from_plan(&plan, files, indices)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ViewedState;

    fn files(paths: &[&str]) -> Vec<PrFile> {
        paths
            .iter()
            .map(|p| PrFile {
                pr: 0,
                path: p.to_string(),
                additions: 1,
                deletions: 0,
                viewed: ViewedState::Unviewed,
                generated: None,
            })
            .collect()
    }

    fn paths(fs: &[PrFile], steps: &[Step]) -> Vec<String> {
        steps.iter().map(|s| fs[s.file].path.clone()).collect()
    }

    #[test]
    fn heuristics_put_contracts_first_tests_after_their_code_and_docs_last() {
        let mut fs = files(&[
            "README.md",
            "src/__tests__/retry.test.ts",
            "src/retry.ts",
            "db/migrations/0002_add_note.sql",
            "src/client.ts",
            ".github/workflows/ci.yml",
            "pnpm-lock.yaml",
        ]);
        fs[6].generated = Some("lockfile");
        let all: Vec<usize> = (0..fs.len()).collect();
        let steps = order(&fs, &all, &[]);
        assert_eq!(
            paths(&fs, &steps),
            [
                "db/migrations/0002_add_note.sql",
                "src/client.ts",
                "src/retry.ts",
                "src/__tests__/retry.test.ts",
                ".github/workflows/ci.yml",
                "README.md",
                "pnpm-lock.yaml",
            ]
        );
        assert_eq!(steps[3].reason, "its test");
    }

    #[test]
    fn calls_put_the_callee_before_its_callers() {
        // client.ts would come first alphabetically, but it calls pool.ts.
        let fs = files(&["src/client.ts", "src/pool.ts", "src/route.ts"]);
        let all = [0, 1, 2];
        let deps = [(1, 0), (0, 2)]; // pool ← client ← route
        let steps = order(&fs, &all, &deps);
        assert_eq!(
            paths(&fs, &steps),
            ["src/pool.ts", "src/client.ts", "src/route.ts"]
        );
        assert_eq!(steps[0].reason, "used by client.ts");
        assert_eq!(steps[1].reason, "uses pool.ts");
    }

    #[test]
    fn a_cycle_still_lists_every_file_once() {
        let fs = files(&["a.ts", "b.ts"]);
        let steps = order(&fs, &[0, 1], &[(0, 1), (1, 0)]);
        let mut seen: Vec<usize> = steps.iter().map(|s| s.file).collect();
        seen.sort();
        assert_eq!(seen, [0, 1]);
    }

    #[test]
    fn plan_calls_become_file_edges() {
        let fs = files(&["packages/db/index.ts", "apps/api/client.ts"]);
        let plan: Plan = serde_json::from_str(
            r#"{"functions":[{"id":"f1","path":"packages/db/index.ts"},{"id":"f2","path":"apps/api/client.ts"},{"id":"f3","path":"elsewhere.ts"}],
                "calls":[{"caller":"f2","callee":"f1"},{"caller":"f3","callee":"f1"},{"caller":"f1","callee":"f1"}]}"#,
        )
        .unwrap();
        assert_eq!(deps_from_plan(&plan, &fs, &[0, 1]), [(0, 1)]);
    }

    #[test]
    fn test_stems_find_their_source() {
        assert_eq!(stem("src/__tests__/retry.test.ts"), "src/retry");
        assert_eq!(stem("pkg/retry_test.go"), "pkg/retry");
        assert_eq!(stem("tests/test_retry.py"), "retry");
        assert_eq!(stem("src/retry.ts"), "src/retry");
    }
}
