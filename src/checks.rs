//! CI checks on a pull request's head commit, and the part of a failed
//! GitHub Actions job's log worth reading.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckState {
    // Declaration order is display order: what needs attention first.
    Failed,
    Running,
    Passed,
    Neutral,
    Skipped,
}

impl CheckState {
    /// From a CheckRun's `status` / `conclusion`, or a commit status' `state`.
    pub fn from_github(status: Option<&str>, conclusion: Option<&str>) -> Self {
        match (status, conclusion) {
            (_, Some("SUCCESS")) => CheckState::Passed,
            (
                _,
                Some("FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED" | "ERROR"),
            ) => CheckState::Failed,
            (_, Some("SKIPPED")) => CheckState::Skipped,
            (_, Some("NEUTRAL" | "CANCELLED" | "STALE")) => CheckState::Neutral,
            (Some("COMPLETED"), None) => CheckState::Neutral,
            _ => CheckState::Running,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Annotation {
    pub level: String,
    pub path: String,
    pub line: Option<u64>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    #[serde(skip)]
    pub pr: usize,
    pub name: String,
    /// The workflow (for Actions) or app that ran it.
    pub source: Option<String>,
    pub state: CheckState,
    pub url: Option<String>,
    /// The step that failed, for a failed Actions job.
    pub failed_step: Option<String>,
    /// GitHub Actions job id, when the log can be fetched.
    #[serde(skip)]
    pub job_id: Option<u64>,
    pub annotations: Vec<Annotation>,
}

/// Only what explains a failure: failure and warning annotations, minus the
/// "Process completed with exit code N" every failed job carries.
pub fn keep_annotation(level: &str, message: &str) -> bool {
    matches!(level, "FAILURE" | "WARNING" | "failure" | "warning")
        && !message.starts_with("Process completed with exit code")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLine {
    Command(String),
    Output(String),
    Error(String),
    Warning(String),
}

/// How many lines of a failed step's output to show, counting back from
/// the error.
const TAIL: usize = 60;

/// The failed step's output from a raw Actions job log: from the end of the
/// `##[group]Run …` block before the first `##[error]`, up to the error
/// lines themselves, timestamps stripped. Returns the step's command, how
/// many earlier lines were cut, and the lines.
pub fn failed_step_excerpt(raw: &str) -> (Option<String>, usize, Vec<LogLine>) {
    let lines: Vec<&str> = raw
        .trim_start_matches('\u{feff}')
        .lines()
        .map(strip_timestamp)
        .collect();
    let Some(error) = lines.iter().position(|l| l.starts_with("##[error]")) else {
        let skip = lines.len().saturating_sub(TAIL);
        return (
            None,
            skip,
            lines[skip..].iter().map(|l| classify(l)).collect(),
        );
    };
    let step = lines[..error]
        .iter()
        .rposition(|l| l.starts_with("##[group]Run "));
    let command = step.map(|i| lines[i]["##[group]Run ".len()..].trim().to_string());
    let output_start = step
        .and_then(|i| {
            lines[i..error]
                .iter()
                .position(|l| l.starts_with("##[endgroup]"))
                .map(|j| i + j + 1)
        })
        .unwrap_or(step.unwrap_or(0));
    let mut end = error;
    while end + 1 < lines.len() && lines[end + 1].starts_with("##[error]") {
        end += 1;
    }
    let section: Vec<&str> = lines[output_start..=end]
        .iter()
        .copied()
        .filter(|l| !l.starts_with("##[group]") && !l.starts_with("##[endgroup]"))
        .collect();
    let skip = section.len().saturating_sub(TAIL);
    (
        command,
        skip,
        section[skip..].iter().map(|l| classify(l)).collect(),
    )
}

fn strip_timestamp(line: &str) -> &str {
    // "2026-10-08T10:50:03.1879946Z rest"
    match line.split_once(' ') {
        Some((stamp, rest))
            if stamp.len() >= 20 && stamp.ends_with('Z') && stamp.as_bytes()[4] == b'-' =>
        {
            rest
        }
        _ => line,
    }
}

fn classify(line: &str) -> LogLine {
    if let Some(rest) = line.strip_prefix("##[error]") {
        LogLine::Error(rest.to_string())
    } else if let Some(rest) = line.strip_prefix("##[warning]") {
        LogLine::Warning(rest.to_string())
    } else if let Some(rest) = line.strip_prefix("[command]") {
        LogLine::Command(rest.to_string())
    } else {
        LogLine::Output(line.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\u{feff}2026-10-08T10:50:01.0000000Z ##[group]Run actions/checkout@v4\n\
2026-10-08T10:50:01.0000000Z with: x\n\
2026-10-08T10:50:01.0000000Z ##[endgroup]\n\
2026-10-08T10:50:02.0000000Z checked out\n\
2026-10-08T10:50:03.0000000Z ##[group]Run cargo test\n\
2026-10-08T10:50:03.0000000Z cargo test\n\
2026-10-08T10:50:03.0000000Z shell: bash\n\
2026-10-08T10:50:03.0000000Z ##[endgroup]\n\
2026-10-08T10:50:04.0000000Z running 3 tests\n\
2026-10-08T10:50:04.0000000Z test a ... FAILED\n\
2026-10-08T10:50:05.0000000Z ##[error]Process completed with exit code 101.\n\
2026-10-08T10:50:05.0000000Z Post job cleanup.\n";

    #[test]
    fn excerpt_is_the_failed_steps_output_up_to_the_error() {
        let (command, skipped, lines) = failed_step_excerpt(LOG);
        assert_eq!(command.as_deref(), Some("cargo test"));
        assert_eq!(skipped, 0);
        assert_eq!(
            lines,
            [
                LogLine::Output("running 3 tests".into()),
                LogLine::Output("test a ... FAILED".into()),
                LogLine::Error("Process completed with exit code 101.".into()),
            ]
        );
    }

    #[test]
    fn long_output_keeps_the_tail() {
        let mut log = String::from(
            "2026-10-08T10:50:03.0000000Z ##[group]Run make\n2026-10-08T10:50:03.0000000Z ##[endgroup]\n",
        );
        for i in 0..100 {
            log.push_str(&format!("2026-10-08T10:50:04.0000000Z line {i}\n"));
        }
        log.push_str("2026-10-08T10:50:05.0000000Z ##[error]boom\n");
        let (_, skipped, lines) = failed_step_excerpt(&log);
        assert_eq!(skipped, 101 - TAIL);
        assert_eq!(lines.last(), Some(&LogLine::Error("boom".into())));
        assert_eq!(lines.len(), TAIL);
    }

    #[test]
    fn states_map_from_check_runs_and_statuses() {
        assert_eq!(
            CheckState::from_github(Some("COMPLETED"), Some("FAILURE")),
            CheckState::Failed
        );
        assert_eq!(
            CheckState::from_github(Some("IN_PROGRESS"), None),
            CheckState::Running
        );
        assert_eq!(
            CheckState::from_github(None, Some("ERROR")),
            CheckState::Failed
        );
        assert_eq!(
            CheckState::from_github(None, Some("PENDING")),
            CheckState::Running
        );
        assert_eq!(
            CheckState::from_github(Some("COMPLETED"), Some("SKIPPED")),
            CheckState::Skipped
        );
    }

    #[test]
    fn exit_code_annotations_are_dropped_as_noise() {
        assert!(!keep_annotation(
            "FAILURE",
            "Process completed with exit code 2."
        ));
        assert!(keep_annotation("FAILURE", "expected `u32`, found `i64`"));
        assert!(!keep_annotation("NOTICE", "Registry set"));
    }
}
