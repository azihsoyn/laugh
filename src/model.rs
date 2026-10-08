use serde::{Deserialize, Serialize};

/// GitHub's per-viewer "Viewed" checkbox on a changed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ViewedState {
    Viewed,
    Unviewed,
    /// Was marked viewed, then the file changed again, so GitHub un-ticked it.
    Dismissed,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrFile {
    /// Which of the opened pull requests this belongs to (index into them).
    #[serde(skip)]
    pub pr: usize,
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    pub viewed: ViewedState,
    /// Why the file counts as generated (`lockfile`, `.gitattributes`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated: Option<&'static str>,
}

impl PrFile {
    /// Directory part of the path including the trailing slash; empty for
    /// files at the repository root.
    pub fn dir(&self) -> &str {
        match self.path.rfind('/') {
            Some(i) => &self.path[..=i],
            None => "",
        }
    }

    pub fn file_name(&self) -> &str {
        &self.path[self.dir().len()..]
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub author: String,
    /// True when GitHub reports this author's GraphQL type as `Bot` (e.g.
    /// CodeRabbit, the Codex connector) rather than `User`.
    pub author_is_bot: bool,
    pub body: String,
    pub diff_hunk: Option<String>,
    pub created_at: Option<String>,
    pub url: Option<String>,
    /// Written by whoever is running laugh (`gh`'s account).
    pub viewer_did_author: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Thread {
    /// Which of the opened pull requests this belongs to (index into them).
    #[serde(skip)]
    pub pr: usize,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub original_line: Option<i64>,
    pub comments: Vec<Comment>,
}

impl Thread {
    pub fn display_line(&self) -> Option<i64> {
        self.line.or(self.original_line)
    }

    /// The author of the first comment — the person (or bot) who opened
    /// this thread, as opposed to whoever replied to it.
    pub fn starter(&self) -> Option<&Comment> {
        self.comments.first()
    }
}
