use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::{Result, bail};
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState};

use regex::{Regex, RegexBuilder};

use crate::{generated, github};

use crate::model::{PrFile, ViewedState};
use crate::reading_order::{self, Step};
use crate::viewed_sync::{Ledger, Outcome, Worker};
use crate::{theme, ui};

const ROOT: usize = 0;

/// A directory in the changed-file tree. Chains of directories with nothing
/// but a single subdirectory are folded into one node (`a/b/c/`), as GitHub's
/// own tree does — such a chain has exactly one set of files underneath, so
/// splitting it into rows would only add rows that toggle the same files.
#[derive(Debug)]
struct DirNode {
    label: String,
    parent: Option<usize>,
    subdirs: Vec<usize>,
    /// Files directly in this directory.
    files: Vec<usize>,
    /// Every file anywhere underneath — what a directory toggle acts on.
    all_files: Vec<usize>,
    /// The top of one pull request's files, when several PRs are open.
    pr_root: bool,
}

#[derive(Default)]
struct RawDir {
    subdirs: BTreeMap<String, RawDir>,
    files: Vec<usize>,
}

fn raw_tree<'a>(files: impl Iterator<Item = (usize, &'a PrFile)>) -> RawDir {
    let mut root = RawDir::default();
    for (i, f) in files {
        let mut node = &mut root;
        let mut parts: Vec<&str> = f.path.split('/').collect();
        parts.pop();
        for part in parts {
            node = node.subdirs.entry(part.to_string()).or_default();
        }
        node.files.push(i);
    }
    root
}

fn dir_node(label: String, parent: Option<usize>, pr_root: bool) -> DirNode {
    DirNode {
        label,
        parent,
        subdirs: Vec::new(),
        files: Vec::new(),
        all_files: Vec::new(),
        pr_root,
    }
}

/// Builds the tree and returns it with the node each pull request starts at.
/// With one PR that is the root itself; with several, each PR gets its own
/// top-level node so they sit side by side in one tree.
fn build_tree(files: &[PrFile], pr_labels: &[String]) -> (Vec<DirNode>, Vec<usize>) {
    let mut dirs = vec![dir_node(String::new(), None, false)];
    if pr_labels.len() <= 1 {
        fill(&mut dirs, ROOT, raw_tree(files.iter().enumerate()));
        return (dirs, vec![ROOT]);
    }
    let mut roots = Vec::new();
    let mut all = Vec::new();
    for (pr, label) in pr_labels.iter().enumerate() {
        let id = dirs.len();
        dirs.push(dir_node(label.clone(), Some(ROOT), true));
        dirs[ROOT].subdirs.push(id);
        let raw = raw_tree(files.iter().enumerate().filter(|(_, f)| f.pr == pr));
        all.extend(fill(&mut dirs, id, raw));
        roots.push(id);
    }
    dirs[ROOT].all_files = all;
    (dirs, roots)
}

/// Moves `raw`'s contents under `dirs[id]`, creating child nodes, and
/// returns every file now under `id`.
fn fill(dirs: &mut Vec<DirNode>, id: usize, raw: RawDir) -> Vec<usize> {
    let mut all = Vec::new();
    for (name, mut child) in raw.subdirs {
        let mut label = format!("{name}/");
        while child.files.is_empty() && child.subdirs.len() == 1 {
            let (next, grandchild) = child.subdirs.into_iter().next().expect("len is 1");
            label.push_str(&next);
            label.push('/');
            child = grandchild;
        }
        let child_id = dirs.len();
        dirs.push(dir_node(label, Some(id), false));
        dirs[id].subdirs.push(child_id);
        all.extend(fill(dirs, child_id, child));
    }
    all.extend(&raw.files);
    dirs[id].files = raw.files;
    dirs[id].all_files = all.clone();
    all
}

/// What a toggle on `indices` should do: if every one is already viewed,
/// unmark them all; otherwise mark the ones that aren't (DISMISSED included —
/// re-ticking a file that changed since you viewed it is the point).
/// The same rule covers a single file and a directory at any depth.
fn toggle_plan(files: &[PrFile], indices: &[usize]) -> (Vec<usize>, bool) {
    let all_viewed = indices
        .iter()
        .all(|&i| files[i].viewed == ViewedState::Viewed);
    if all_viewed {
        (indices.to_vec(), false)
    } else {
        let pending = indices
            .iter()
            .copied()
            .filter(|&i| files[i].viewed != ViewedState::Viewed)
            .collect();
        (pending, true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowRef {
    Dir(usize),
    File { file: usize, dir: usize },
}

/// Where one opened PR lives and its base and head commits — what prognost
/// needs to work out a reading order.
pub struct PrSource {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub base: String,
    pub head: String,
}

/// A file to hand to `LAUGH_OPEN_CMD`, picked up by the app loop, which
/// gives the command the terminal while it runs.
pub struct OpenRequest {
    pub path: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub base: String,
    pub head: String,
}

/// A bulk Viewed change waiting on a yes / no.
#[derive(Debug)]
struct Confirm {
    targets: Vec<usize>,
    viewed: bool,
    /// What the files have in common, for the dialog's title and question.
    what: Bulk,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Bulk {
    Generated,
    Matching(String),
}

/// A word reads as a glob when it has a `*` that can't be a regex
/// repetition: at the start, or after anything but `.`, `)`, `]` or `\`.
/// So `*.spec.ts` and `src/*/index.ts` are globs, `.*\.ts` is a regex.
fn is_glob(word: &str) -> bool {
    let chars: Vec<char> = word.chars().collect();
    chars.iter().enumerate().any(|(i, &c)| {
        c == '*' && (i == 0 || !matches!(chars[i - 1], '.' | ')' | ']' | '\\' | '*'))
    })
}

fn word_regex(word: &str) -> Option<Regex> {
    let source = if is_glob(word) {
        generated::glob_pattern(word)?
    } else {
        word.to_string()
    };
    RegexBuilder::new(&source)
        .case_insensitive(true)
        .build()
        .ok()
}
/// The `/` filter. Each whitespace-separated word is a case-insensitive
/// glob or regex, and a path has to match all of them; a word that is
/// neither (yet — say, halfway through typing a group) is matched as text.
#[derive(Debug, Default)]
struct Filter {
    text: String,
    words: Vec<Regex>,
    /// Some word didn't parse as a regex and is being matched as text.
    literal: bool,
}

impl Filter {
    #[cfg(test)]
    fn new(text: &str) -> Self {
        let mut filter = Filter::default();
        filter.set(text.to_string());
        filter
    }

    fn set(&mut self, text: String) {
        self.literal = false;
        self.words = text
            .split_whitespace()
            .filter_map(|word| {
                word_regex(word).or_else(|| {
                    self.literal = true;
                    RegexBuilder::new(&regex::escape(word))
                        .case_insensitive(true)
                        .build()
                        .ok()
                })
            })
            .collect();
        self.text = text;
    }

    fn push(&mut self, c: char) {
        let mut text = std::mem::take(&mut self.text);
        text.push(c);
        self.set(text);
    }

    fn pop(&mut self) {
        let mut text = std::mem::take(&mut self.text);
        text.pop();
        self.set(text);
    }

    fn clear(&mut self) {
        self.set(String::new());
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Whether the filter narrows anything down at all.
    fn is_active(&self) -> bool {
        !self.words.is_empty()
    }

    fn matches(&self, path: &str) -> bool {
        self.words.iter().all(|re| re.is_match(path))
    }
}

/// One PR's diffs, by path, as GitHub serves them.
enum Patches {
    Loading,
    Loaded(HashMap<String, Option<String>>),
    Failed(String),
}

type PatchResult = (usize, Result<HashMap<String, Option<String>>, String>);

/// The diff shown beside the tree: the selected file's patch, fetched per
/// PR the first time it's needed.
struct DiffPane {
    open: bool,
    scroll: u16,
    /// The file the scroll position belongs to.
    file: Option<usize>,
    /// Where the pane was last drawn, for the mouse wheel.
    area: Option<Rect>,
    patches: HashMap<usize, Patches>,
    tx: Sender<PatchResult>,
    rx: Receiver<PatchResult>,
}

impl Default for DiffPane {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        DiffPane {
            open: false,
            scroll: 0,
            file: None,
            area: None,
            patches: HashMap::new(),
            tx,
            rx,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderBy {
    Kind,
    Asking,
    Prognost,
}

type PrognostResult = (usize, Result<Option<Vec<(usize, usize)>>, String>);

/// Which directory node each file sits directly in.
fn file_dirs(dirs: &[DirNode], count: usize) -> Vec<usize> {
    let mut out = vec![ROOT; count];
    for (d, node) in dirs.iter().enumerate() {
        for &f in &node.files {
            out[f] = d;
        }
    }
    out
}

/// The files of each PR, in the order the tree's roots give them.
fn files_per_pr(dirs: &[DirNode], pr_roots: &[usize]) -> Vec<Vec<usize>> {
    pr_roots
        .iter()
        .map(|&r| dirs[r].all_files.clone())
        .collect()
}

/// The changed-files screen: a directory tree with each file's Viewed state.
pub struct FilesView {
    files: Vec<PrFile>,
    dirs: Vec<DirNode>,
    pr_roots: Vec<usize>,
    /// `None` shows every open PR together; `Some(i)` just the i-th.
    scope: Option<usize>,
    collapsed: HashSet<usize>,
    hide_viewed: bool,
    table: TableState,
    status: Option<String>,
    /// Generated files waiting on a yes / no before they're marked viewed.
    confirm: Option<Confirm>,
    /// `/`: only files whose path contains every word of this.
    filter: Filter,
    /// Typing the filter: keys go into it.
    filtering: bool,
    ledger: Ledger,
    /// `None` only in tests, where nothing should reach GitHub.
    worker: Option<Worker>,
    sources: Vec<PrSource>,
    file_dir: Vec<usize>,
    /// Showing the reading order instead of the tree.
    order_mode: bool,
    /// Reading order per opened PR, and how each was worked out.
    order: Vec<Vec<Step>>,
    order_by: Vec<OrderBy>,
    prognost: Option<Receiver<PrognostResult>>,
    diff: DiffPane,
    /// `LAUGH_OPEN_CMD`: when set, ⏎ on a file runs it instead of showing
    /// the diff here.
    open_cmd: Option<String>,
    open_request: Option<OpenRequest>,
}

impl FilesView {
    /// The node the tree is drawn from for the current scope.
    fn view_root(&self) -> usize {
        self.scope.map_or(ROOT, |pr| self.pr_roots[pr])
    }

    pub fn set_scope(&mut self, scope: Option<usize>) {
        self.scope = scope;
        self.table.select(Some(0));
        self.clamp();
    }

    fn file_visible(&self, file: usize) -> bool {
        !(self.hide_viewed && self.files[file].viewed == ViewedState::Viewed)
            && self.filter.matches(&self.files[file].path)
    }

    /// Files in scope that match the filter, viewed or not.
    fn matching(&self) -> Vec<usize> {
        self.dirs[self.view_root()]
            .all_files
            .iter()
            .copied()
            .filter(|&f| self.filter.matches(&self.files[f].path))
            .collect()
    }

    fn filter_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => self.filtering = false,
            KeyCode::Esc => {
                self.filtering = false;
                self.filter.clear();
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Down => return self.move_by(1),
            KeyCode::Up => return self.move_by(-1),
            KeyCode::Char(c) => self.filter.push(c),
            _ => return,
        }
        self.table.select(Some(0));
        self.clamp();
    }

    /// `V` while filtering: every match at once, after asking.
    fn ask_to_toggle_matching(&mut self) {
        let (targets, viewed) = toggle_plan(&self.files, &self.matching());
        match targets.len() {
            0 => self.status = Some("no files match".to_string()),
            1 => self.apply_toggle(&targets),
            _ => {
                self.confirm = Some(Confirm {
                    targets,
                    viewed,
                    what: Bulk::Matching(self.filter.text.trim().to_string()),
                })
            }
        }
    }

    fn dir_visible(&self, dir: usize) -> bool {
        self.dirs[dir]
            .all_files
            .iter()
            .any(|&f| self.file_visible(f))
    }

    fn rows(&self) -> Vec<RowRef> {
        if self.order_mode {
            return self.order_rows();
        }
        let mut rows = Vec::new();
        self.push_contents(self.view_root(), &mut rows);
        rows
    }

    /// PRs in the current scope, in order.
    fn scoped_prs(&self) -> Vec<usize> {
        match self.scope {
            Some(pr) => vec![pr],
            None => (0..self.order.len()).collect(),
        }
    }

    fn order_rows(&self) -> Vec<RowRef> {
        self.scoped_prs()
            .into_iter()
            .flat_map(|pr| self.order[pr].iter())
            .filter(|step| self.file_visible(step.file))
            .map(|step| RowRef::File {
                file: step.file,
                dir: self.file_dir[step.file],
            })
            .collect()
    }

    fn order_reason(&self, file: usize) -> Option<&str> {
        self.order
            .iter()
            .flatten()
            .find(|s| s.file == file)
            .map(|s| s.reason.as_str())
    }

    /// Switches between the tree and the reading order. The first time,
    /// asks prognost (in the background) to refine the order.
    fn toggle_order(&mut self) {
        self.order_mode = !self.order_mode;
        self.table.select(Some(0));
        self.clamp();
        if self.order_mode && self.prognost.is_none() && !self.sources.is_empty() {
            let (tx, rx) = mpsc::channel();
            let files = self.files.clone();
            let per_pr = files_per_pr(&self.dirs, &self.pr_roots);
            let sources: Vec<(String, String, String, String)> = self
                .sources
                .iter()
                .map(|s| {
                    (
                        s.owner.clone(),
                        s.repo.clone(),
                        s.base.clone(),
                        s.head.clone(),
                    )
                })
                .collect();
            self.order_by = vec![OrderBy::Asking; self.order.len()];
            thread::spawn(move || {
                for (pr, (owner, repo, base, head)) in sources.into_iter().enumerate() {
                    let result = reading_order::prognost_deps(
                        &files,
                        &per_pr[pr],
                        &owner,
                        &repo,
                        &base,
                        &head,
                    )
                    .map_err(|e| format!("{e:#}"));
                    if tx.send((pr, result)).is_err() {
                        break;
                    }
                }
            });
            self.prognost = Some(rx);
        }
    }

    fn drain_prognost(&mut self) {
        let Some(rx) = &self.prognost else { return };
        let results: Vec<PrognostResult> = rx.try_iter().collect();
        for (pr, result) in results {
            let indices = files_per_pr(&self.dirs, &self.pr_roots).swap_remove(pr);
            match result {
                // prognost ran but found no calls between the changed files
                // (or none it can read — it's TypeScript only): still by kind.
                Ok(Some(deps)) if !deps.is_empty() => {
                    self.order[pr] = reading_order::order(&self.files, &indices, &deps);
                    self.order_by[pr] = OrderBy::Prognost;
                }
                Ok(_) => self.order_by[pr] = OrderBy::Kind,
                Err(e) => {
                    self.order_by[pr] = OrderBy::Kind;
                    self.status = Some(format!("prognost: {e}"));
                }
            }
        }
    }

    fn order_label(&self) -> &'static str {
        let by: Vec<OrderBy> = self
            .scoped_prs()
            .iter()
            .map(|&pr| self.order_by[pr])
            .collect();
        if by.contains(&OrderBy::Asking) {
            "by kind · asking prognost…"
        } else if by.contains(&OrderBy::Prognost) {
            "by calls (prognost)"
        } else {
            "by kind"
        }
    }

    fn push_contents(&self, dir: usize, rows: &mut Vec<RowRef>) {
        for &sub in &self.dirs[dir].subdirs {
            if !self.dir_visible(sub) {
                continue;
            }
            rows.push(RowRef::Dir(sub));
            if !self.collapsed.contains(&sub) {
                self.push_contents(sub, rows);
            }
        }
        for &file in &self.dirs[dir].files {
            if self.file_visible(file) {
                rows.push(RowRef::File { file, dir });
            }
        }
    }

    /// The same rows as `rows`, each with the tree guide (`├─ `, `│  `…)
    /// that draws its place in the hierarchy.
    fn rows_with_guides(&self) -> Vec<(RowRef, String)> {
        if self.order_mode {
            return self
                .order_rows()
                .into_iter()
                .enumerate()
                .map(|(n, row)| (row, format!("{:>3}  ", n + 1)))
                .collect();
        }
        let mut rows = Vec::new();
        self.push_guided(self.view_root(), "", true, &mut rows);
        rows
    }

    fn push_guided(&self, dir: usize, prefix: &str, top: bool, rows: &mut Vec<(RowRef, String)>) {
        let node = &self.dirs[dir];
        let children: Vec<RowRef> = node
            .subdirs
            .iter()
            .filter(|&&d| self.dir_visible(d))
            .map(|&d| RowRef::Dir(d))
            .chain(
                node.files
                    .iter()
                    .filter(|&&f| self.file_visible(f))
                    .map(|&file| RowRef::File { file, dir }),
            )
            .collect();
        let count = children.len();
        for (i, child) in children.into_iter().enumerate() {
            let last = i + 1 == count;
            let (branch, carry) = match (top, last) {
                (true, _) => ("", ""),
                (false, true) => ("└─ ", "   "),
                (false, false) => ("├─ ", "│  "),
            };
            rows.push((child, format!("{prefix}{branch}")));
            if let RowRef::Dir(d) = child
                && !self.collapsed.contains(&d)
            {
                self.push_guided(d, &format!("{prefix}{carry}"), false, rows);
            }
        }
    }

    fn current(&self) -> Option<RowRef> {
        let rows = self.rows();
        self.table.selected().and_then(|i| rows.get(i).copied())
    }

    fn clamp(&mut self) {
        let len = self.rows().len();
        let sel = match self.table.selected() {
            _ if len == 0 => None,
            Some(i) => Some(i.min(len - 1)),
            None => Some(0),
        };
        self.table.select(sel);
    }

    fn move_by(&mut self, delta: i64) {
        let len = self.rows().len() as i64;
        if len == 0 {
            return;
        }
        let cur = self.table.selected().unwrap_or(0) as i64;
        self.table
            .select(Some((cur + delta).clamp(0, len - 1) as usize));
    }

    fn select_dir(&mut self, dir: usize) {
        if dir == self.view_root() {
            return;
        }
        if let Some(i) = self.rows().iter().position(|r| *r == RowRef::Dir(dir)) {
            self.table.select(Some(i));
        }
    }

    /// Left: fold an open directory; otherwise step out to the parent.
    fn go_left(&mut self, row: RowRef) {
        match row {
            RowRef::Dir(d) if !self.collapsed.contains(&d) => {
                self.collapsed.insert(d);
            }
            RowRef::Dir(d) => {
                if let Some(parent) = self.dirs[d].parent {
                    self.select_dir(parent);
                }
            }
            RowRef::File { dir, .. } => self.select_dir(dir),
        }
    }

    /// A directory's files that the filter lets through, viewed or not —
    /// what its counts show and what `V` on it covers.
    fn dir_files(&self, dir: usize) -> Vec<usize> {
        self.dirs[dir]
            .all_files
            .iter()
            .copied()
            .filter(|&f| self.filter.matches(&self.files[f].path))
            .collect()
    }

    fn viewed_count(&self, indices: &[usize]) -> usize {
        indices
            .iter()
            .filter(|&&i| self.files[i].viewed == ViewedState::Viewed)
            .count()
    }

    /// Changes the screen immediately and queues the write to GitHub.
    fn apply_toggle(&mut self, indices: &[usize]) {
        let (targets, viewed) = toggle_plan(&self.files, indices);
        if targets.is_empty() {
            return;
        }
        let jobs = self.ledger.apply(&mut self.files, &targets, viewed);
        if let Some(worker) = &self.worker {
            for job in jobs {
                worker.send(job);
            }
        }
        self.clamp();
    }

    fn settle(&mut self, outcome: Outcome) {
        let reverted = self
            .ledger
            .settle(&mut self.files, outcome.id, outcome.error.is_none());
        if let Some(error) = outcome.error {
            self.status = Some(format!(
                "couldn't save to GitHub, reverted {reverted} file(s): {error}"
            ));
            self.clamp();
        }
    }

    fn drain_outcomes(&mut self) {
        let outcomes: Vec<Outcome> = match &self.worker {
            Some(worker) => worker.outcomes.try_iter().collect(),
            None => return,
        };
        for outcome in outcomes {
            self.settle(outcome);
        }
    }

    /// Viewed and total files within the current scope.
    pub fn viewed_counts(&self) -> (usize, usize) {
        let in_scope = &self.dirs[self.view_root()].all_files;
        (self.viewed_count(in_scope), in_scope.len())
    }

    pub fn pending(&self) -> usize {
        self.ledger.pending()
    }

    pub fn show_saving(&mut self) {
        self.status = Some(format!(
            "saving {} pending change(s) to GitHub…",
            self.ledger.pending()
        ));
    }

    /// Lets every queued write reach GitHub before exiting, so quitting right
    /// after a toggle doesn't silently drop it.
    pub fn finish(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let failures: Vec<String> = worker
            .finish()
            .into_iter()
            .filter_map(|o| o.error)
            .collect();
        if !failures.is_empty() {
            bail!(
                "{} Viewed change(s) didn't reach GitHub: {}",
                failures.len(),
                failures.join("; ")
            );
        }
        Ok(())
    }

    /// `pr_ids` / `pr_labels` are per opened pull request; each file's `pr`
    /// indexes into them.
    pub fn new(
        pr_ids: Vec<String>,
        pr_labels: &[String],
        sources: Vec<PrSource>,
        files: Vec<PrFile>,
    ) -> Self {
        let (dirs, pr_roots) = build_tree(&files, pr_labels);
        let order: Vec<Vec<Step>> = files_per_pr(&dirs, &pr_roots)
            .iter()
            .map(|indices| reading_order::order(&files, indices, &[]))
            .collect();
        let mut view = FilesView {
            file_dir: file_dirs(&dirs, files.len()),
            order_by: vec![OrderBy::Kind; order.len()],
            order,
            order_mode: false,
            prognost: None,
            diff: DiffPane::default(),
            open_cmd: std::env::var("LAUGH_OPEN_CMD")
                .ok()
                .filter(|c| !c.trim().is_empty()),
            open_request: None,
            sources,
            dirs,
            pr_roots,
            scope: None,
            ledger: Ledger::new(&files),
            files,
            collapsed: HashSet::new(),
            hide_viewed: false,
            table: TableState::default(),
            status: None,
            confirm: None,
            filter: Filter::default(),
            filtering: false,
            worker: Some(Worker::spawn(pr_ids)),
        };
        view.clamp();
        view
    }

    /// Picks up finished background writes; call once per frame.
    pub fn tick(&mut self) {
        self.drain_outcomes();
        self.drain_prognost();
        self.drain_patches();
    }

    fn selected_file(&self) -> Option<usize> {
        match self.current()? {
            RowRef::File { file, .. } => Some(file),
            RowRef::Dir(_) => None,
        }
    }

    /// Collects fetched diffs, and starts fetching the selected file's PR's
    /// if the diff pane needs it.
    fn drain_patches(&mut self) {
        for (pr, result) in self.diff.rx.try_iter() {
            let state = match result {
                Ok(map) => Patches::Loaded(map),
                Err(e) => Patches::Failed(e),
            };
            self.diff.patches.insert(pr, state);
        }
        if !self.diff.open {
            return;
        }
        let Some(file) = self.selected_file() else {
            return;
        };
        let pr = self.files[file].pr;
        if self.diff.patches.contains_key(&pr) {
            return;
        }
        let Some(source) = self.sources.get(pr) else {
            return;
        };
        self.diff.patches.insert(pr, Patches::Loading);
        let (owner, repo, number) = (source.owner.clone(), source.repo.clone(), source.number);
        let tx = self.diff.tx.clone();
        thread::spawn(move || {
            let result = github::fetch_patches(&owner, &repo, number).map_err(|e| format!("{e:#}"));
            let _ = tx.send((pr, result));
        });
    }

    /// True while Esc should close the diff rather than quit.
    pub fn takes_esc(&self) -> bool {
        self.diff.open || !self.filter.is_empty()
    }

    /// A file waiting to be opened with `LAUGH_OPEN_CMD`.
    pub fn take_open_request(&mut self) -> Option<OpenRequest> {
        self.open_request.take()
    }

    pub fn open_cmd(&self) -> Option<&str> {
        self.open_cmd.as_deref()
    }

    /// The mouse wheel: over the diff it scrolls the diff, anywhere else
    /// it moves the cursor.
    pub fn wheel(&mut self, down: bool, at: Position) {
        if self.diff.open && self.diff.area.is_some_and(|a| a.contains(at)) {
            self.diff.scroll = if down {
                self.diff.scroll.saturating_add(3)
            } else {
                self.diff.scroll.saturating_sub(3)
            };
        } else {
            self.handle_key(if down { KeyCode::Down } else { KeyCode::Up });
        }
    }

    pub fn set_status(&mut self, status: String) {
        self.status = Some(status);
    }

    fn open_file(&mut self, file: usize) {
        if self.open_cmd.is_none() {
            self.diff.open = !self.diff.open;
            return;
        }
        let pf = &self.files[file];
        let Some(source) = self.sources.get(pf.pr) else {
            return;
        };
        self.open_request = Some(OpenRequest {
            path: pf.path.clone(),
            owner: source.owner.clone(),
            repo: source.repo.clone(),
            number: source.number,
            base: source.base.clone(),
            head: source.head.clone(),
        });
    }

    /// Generated files in scope that aren't viewed yet.
    fn unviewed_generated(&self) -> Vec<usize> {
        self.dirs[self.view_root()]
            .all_files
            .iter()
            .copied()
            .filter(|&f| {
                self.files[f].generated.is_some() && self.files[f].viewed != ViewedState::Viewed
            })
            .collect()
    }

    /// True while a dialog is open or the filter is being typed, and every
    /// key should come here.
    pub fn is_modal(&self) -> bool {
        self.confirm.is_some() || self.filtering
    }

    fn ask_to_mark_generated(&mut self) {
        let targets = self.unviewed_generated();
        if targets.is_empty() {
            self.status = Some("no unviewed generated files here".to_string());
        } else {
            self.confirm = Some(Confirm {
                targets,
                viewed: true,
                what: Bulk::Generated,
            });
        }
    }

    fn answer_confirm(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let Some(confirm) = self.confirm.take() else {
                    return;
                };
                let jobs = self
                    .ledger
                    .apply(&mut self.files, &confirm.targets, confirm.viewed);
                if let Some(worker) = &self.worker {
                    for job in jobs {
                        worker.send(job);
                    }
                }
                let count = confirm.targets.len();
                let how = if confirm.viewed {
                    "marked viewed"
                } else {
                    "unmarked"
                };
                self.status = Some(match confirm.what {
                    Bulk::Generated => format!("{count} generated file(s) {how}"),
                    Bulk::Matching(q) => format!("{count} file(s) matching “{q}” {how}"),
                });
                self.clamp();
            }
            KeyCode::Char('n') | KeyCode::Char('q') | KeyCode::Esc => self.confirm = None,
            _ => {}
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        self.status = None;
        if self.confirm.is_some() {
            self.answer_confirm(code);
            return;
        }
        if self.filtering {
            self.filter_key(code);
            return;
        }
        if code == KeyCode::Char('/') {
            self.filtering = true;
            self.diff.open = false;
            return;
        }
        if code == KeyCode::Esc && !self.diff.open && !self.filter.is_empty() {
            self.filter.clear();
            self.clamp();
            return;
        }
        if code == KeyCode::Char('m') {
            if self.filter.is_active() {
                self.ask_to_toggle_matching();
            } else {
                self.ask_to_mark_generated();
            }
            return;
        }
        if code == KeyCode::Char('o') {
            self.toggle_order();
            return;
        }
        if self.diff.open {
            match code {
                KeyCode::Esc => {
                    self.diff.open = false;
                    return;
                }
                KeyCode::Char('J') | KeyCode::PageDown => {
                    self.diff.scroll = self.diff.scroll.saturating_add(10);
                    return;
                }
                KeyCode::Char('K') | KeyCode::PageUp => {
                    self.diff.scroll = self.diff.scroll.saturating_sub(10);
                    return;
                }
                _ => {}
            }
        }
        let Some(current) = self.current() else {
            if code == KeyCode::Char('H') {
                self.hide_viewed = !self.hide_viewed;
                self.clamp();
            }
            return;
        };
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Char('g') | KeyCode::Home => self.table.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => {
                let len = self.rows().len();
                self.table.select(len.checked_sub(1));
            }
            // Viewed changes are writes to GitHub, so they sit on deliberate
            // letter keys only — never on space, which is easy to hit by accident.
            KeyCode::Char('v') => match current {
                RowRef::File { file, .. } => self.apply_toggle(&[file]),
                RowRef::Dir(_) => {
                    self.status =
                        Some("v toggles a single file — use V for a whole directory".to_string());
                }
            },
            KeyCode::Char('V') => {
                // On a file, V means "the directory this file sits in".
                // Files at the repository root have no directory row, so
                // they fall back to just themselves rather than the whole PR.
                // With a filter on, only what matches.
                let targets = match current {
                    RowRef::Dir(d) => self.dir_files(d),
                    RowRef::File { dir, .. } if dir != self.view_root() => self.dir_files(dir),
                    RowRef::File { file, .. } => vec![file],
                };
                self.apply_toggle(&targets);
            }
            KeyCode::Enter => match current {
                RowRef::Dir(d) => {
                    if !self.collapsed.remove(&d) {
                        self.collapsed.insert(d);
                    }
                }
                RowRef::File { file, .. } => self.open_file(file),
            },
            KeyCode::Char('l') | KeyCode::Right => {
                if let RowRef::Dir(d) = current {
                    self.collapsed.remove(&d);
                }
            }
            KeyCode::Char('h') | KeyCode::Left => self.go_left(current),
            KeyCode::Char('H') => {
                self.hide_viewed = !self.hide_viewed;
                self.clamp();
            }
            _ => {}
        }
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Key hints for the footer, for whatever the cursor is on.
    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let hide = if self.hide_viewed {
            ("H", "show viewed")
        } else {
            ("H", "hide viewed")
        };
        if self.confirm.is_some() {
            return vec![("y", "yes"), ("n", "cancel")];
        }
        if self.filtering {
            return vec![("type", "filter by path"), ("⏎", "done"), ("esc", "clear")];
        }
        if self.filter.is_active() && !self.diff.open {
            let v = match self.current() {
                Some(RowRef::Dir(_)) => ("V", "viewed: matches inside"),
                _ => ("V", "matches in folder"),
            };
            return vec![
                ("v", "this file"),
                v,
                ("m", "every match"),
                ("/", "edit"),
                ("esc", "clear filter"),
                hide,
            ];
        }
        let mut hints = match self.current() {
            Some(RowRef::Dir(_)) => vec![("V", "viewed: everything inside"), ("⏎", "fold"), hide],
            Some(RowRef::File { .. }) if self.diff.open => {
                vec![
                    ("v", "viewed"),
                    ("J K", "scroll the diff"),
                    ("esc", "close"),
                    hide,
                ]
            }
            Some(RowRef::File { .. }) => vec![
                ("v", "viewed"),
                ("V", "whole folder"),
                if self.open_cmd.is_some() {
                    ("⏎", "open")
                } else {
                    ("⏎", "diff")
                },
                hide,
            ],
            None => vec![hide],
        };
        if !self.unviewed_generated().is_empty() {
            hints.push(("m", "viewed: generated"));
        }
        hints.push(if self.order_mode {
            ("o", "tree")
        } else {
            ("o", "reading order")
        });
        hints
    }

    pub fn draw(&mut self, f: &mut ratatui::Frame<'_>, area: Rect) {
        draw(f, area, self);
    }
}

fn viewed_icon(state: ViewedState) -> Span<'static> {
    match state {
        ViewedState::Viewed => Span::styled("✓", Style::default().fg(theme::GREEN)),
        ViewedState::Dismissed => Span::styled("⟳", Style::default().fg(theme::YELLOW)),
        ViewedState::Unviewed => Span::styled("○", theme::faint()),
    }
}

/// `+N` or `-N` right-aligned in its own column, blank for zero, so the
/// added and removed counts each line up down the tree.
fn count_cell(n: u64, sign: char, color: ratatui::style::Color) -> Cell<'static> {
    if n == 0 {
        return Cell::from("");
    }
    Cell::from(
        Line::from(Span::styled(
            format!("{sign}{n}"),
            Style::default().fg(color),
        ))
        .right_aligned(),
    )
}

/// "Mark these N generated files viewed?" — listing what and why, since it
/// writes to GitHub for files the reviewer hasn't opened.
fn draw_confirm(f: &mut ratatui::Frame<'_>, area: Rect, app: &FilesView, confirm: &Confirm) {
    const SHOWN: usize = 12;
    let targets = &confirm.targets;
    let (verb, done) = if confirm.viewed {
        ("Mark ", " as viewed?")
    } else {
        ("Unmark ", " — back to not viewed?")
    };
    let (kind, title) = match &confirm.what {
        Bulk::Generated => (
            " generated file(s)".to_string(),
            "Generated files".to_string(),
        ),
        Bulk::Matching(q) => (
            format!(" file(s) matching “{q}”"),
            format!("Files matching “{q}”"),
        ),
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(verb, theme::text()),
            Span::styled(format!("{}", targets.len()), theme::bold(theme::accent())),
            Span::styled(format!("{kind}{done}"), theme::text()),
        ]),
        Line::raw(""),
    ];
    for &i in targets.iter().take(SHOWN) {
        let file = &app.files[i];
        lines.push(Line::from(vec![
            Span::raw("  "),
            viewed_icon(file.viewed),
            Span::raw(" "),
            Span::styled(file.path.clone(), theme::text()),
            Span::styled(
                format!("  {}", file.generated.unwrap_or("")),
                theme::faint(),
            ),
        ]));
    }
    if targets.len() > SHOWN {
        lines.push(Line::from(Span::styled(
            format!("  …and {} more", targets.len() - SHOWN),
            theme::muted(),
        )));
    }
    lines.push(Line::raw(""));
    let mut keys = vec![Span::raw("  ")];
    keys.extend(ui::key_hints(&[
        (
            "y",
            if confirm.viewed {
                "mark them viewed"
            } else {
                "unmark them"
            },
        ),
        ("n", "cancel"),
    ]));
    lines.push(Line::from(keys));

    let width = lines
        .iter()
        .map(|l| l.width() as u16)
        .max()
        .unwrap_or(40)
        .saturating_add(6)
        .clamp(44, area.width.saturating_sub(4));
    let popup = ui::centered(area, width, lines.len() as u16 + 2);
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(ui::panel(&title, true)), popup);
}

/// The selected file's diff, beside the tree.
fn draw_diff(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut FilesView) {
    let Some(file) = app.selected_file() else {
        f.render_widget(
            Paragraph::new(Span::styled(
                "  Pick a file to see its diff.",
                theme::muted(),
            ))
            .block(ui::panel("Diff", true)),
            area,
        );
        return;
    };
    if app.diff.file != Some(file) {
        app.diff.file = Some(file);
        app.diff.scroll = 0;
    }
    let pf = &app.files[file];
    // Long paths lose their start, not the file name.
    let room = (area.width as usize).saturating_sub(6);
    let title = if pf.path.chars().count() > room {
        let tail: String = pf.path.chars().rev().take(room.saturating_sub(1)).collect();
        format!("…{}", tail.chars().rev().collect::<String>())
    } else {
        pf.path.clone()
    };
    let block = ui::panel(&title, true);
    let inner = block.inner(area);
    let note =
        |text: String| Paragraph::new(Span::styled(text, theme::muted())).block(block.clone());
    match app.diff.patches.get(&pf.pr) {
        None | Some(Patches::Loading) => {
            f.render_widget(note("fetching the diff…".to_string()), area);
        }
        Some(Patches::Failed(e)) => f.render_widget(note(e.clone()), area),
        Some(Patches::Loaded(map)) => match map.get(&pf.path).and_then(|p| p.as_deref()) {
            None => f.render_widget(
                note(
                    "GitHub doesn't show a diff for this file — it's binary, too large, or only renamed."
                        .to_string(),
                ),
                area,
            ),
            Some(patch) => {
                let (lines, _) = ui::diff_lines(patch, inner.width as usize);
                let last = (lines.len() as u16).saturating_sub(inner.height);
                app.diff.scroll = app.diff.scroll.min(last);
                f.render_widget(
                    Paragraph::new(lines)
                        .block(block)
                        .scroll((app.diff.scroll, 0)),
                    area,
                );
            }
        },
    }
}

fn draw(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut FilesView) {
    let [summary_area, list_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);

    let (viewed, total) = app.viewed_counts();
    let dismissed = app.dirs[app.view_root()]
        .all_files
        .iter()
        .filter(|&&f| app.files[f].viewed == ViewedState::Dismissed)
        .count();

    let mut summary = vec![Span::styled(" Viewed  ", theme::muted())];
    summary.extend(ui::gauge(viewed, total, 28));
    summary.push(Span::styled(
        format!("  {viewed}/{total}"),
        theme::bold(theme::text()),
    ));
    if dismissed > 0 {
        summary.push(Span::styled("   ⟳ ", Style::default().fg(theme::YELLOW)));
        summary.push(Span::styled(
            format!("{dismissed} changed since viewed"),
            Style::default().fg(theme::YELLOW),
        ));
    }
    if app.hide_viewed {
        summary.push(Span::raw("   "));
        summary.push(Span::styled(" hiding viewed ", theme::pill_on()));
    }
    if app.filtering || !app.filter.is_empty() {
        summary.push(Span::raw("   "));
        summary.push(Span::styled(" / ", theme::pill_on()));
        summary.push(Span::styled(
            format!(" {}", app.filter.text),
            theme::bold(theme::text()),
        ));
        if app.filtering {
            summary.push(Span::styled("▏", theme::accent()));
        }
        summary.push(Span::styled(
            format!("  {} match", app.matching().len()),
            theme::muted(),
        ));
        if app.filter.literal {
            summary.push(Span::styled(
                "  · not a valid regex, matched as text",
                Style::default().fg(theme::YELLOW),
            ));
        }
    }
    if app.order_mode {
        summary.push(Span::raw("   "));
        summary.push(Span::styled(" reading order ", theme::pill_on()));
        summary.push(Span::styled(
            format!(" {}", app.order_label()),
            theme::muted(),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(summary)), summary_area);

    // Pad both sides of `done/total` to the widest total any directory row
    // shows (the root itself is never a row), so the slashes line up.
    let digits = app
        .dirs
        .iter()
        .skip(1)
        .map(|d| d.all_files.len().to_string().len())
        .max()
        .unwrap_or(1);
    // Beside a diff the tree is narrow: give the names the bars' room.
    let gauge_width = if app.diff.open { 0 } else { 8 };
    let rows: Vec<Row> = app
        .rows_with_guides()
        .into_iter()
        .map(|(r, guide)| {
            let guide = Span::styled(guide, theme::faint());
            match r {
                RowRef::Dir(d) => {
                    let node = &app.dirs[d];
                    let shown = app.dir_files(d);
                    let done = app.viewed_count(&shown);
                    let total = shown.len();
                    let fold = if app.collapsed.contains(&d) {
                        "▸ "
                    } else {
                        "▾ "
                    };
                    let (adds, dels) = shown.iter().fold((0, 0), |(a, d), &i| {
                        (a + app.files[i].additions, d + app.files[i].deletions)
                    });
                    let mut name = vec![guide, Span::styled(fold, theme::muted())];
                    if node.pr_root {
                        name.push(Span::styled("▣ ", Style::default().fg(theme::BRAND)));
                        name.push(Span::styled(node.label.clone(), theme::bold(theme::text())));
                    } else {
                        name.push(Span::styled(
                            node.label.clone(),
                            theme::bold(theme::accent()),
                        ));
                    }
                    if done == total {
                        name.push(Span::styled(" ✓", Style::default().fg(theme::GREEN)));
                    }
                    Row::new(vec![
                        Cell::from(Line::from(name)),
                        Cell::from(Line::from(ui::gauge(done, total, gauge_width))),
                        Cell::from(Span::styled(
                            format!("{done:>digits$}/{total:>digits$}"),
                            theme::muted(),
                        )),
                        count_cell(adds, '+', theme::GREEN),
                        count_cell(dels, '-', theme::RED),
                    ])
                }
                RowRef::File { file, .. } => {
                    let pf = &app.files[file];
                    let name_style = match pf.viewed {
                        ViewedState::Viewed => theme::muted(),
                        _ => theme::text(),
                    };
                    Row::new(vec![
                        Cell::from(Line::from(vec![
                            guide,
                            viewed_icon(pf.viewed),
                            Span::raw(" "),
                            Span::styled(
                                if app.order_mode {
                                    pf.path.clone()
                                } else {
                                    pf.file_name().to_string()
                                },
                                name_style,
                            ),
                            Span::styled(
                                if app.order_mode {
                                    app.order_reason(file)
                                        .map_or(String::new(), |r| format!("  {r}"))
                                } else {
                                    pf.generated.map_or(String::new(), |why| format!("  {why}"))
                                },
                                theme::faint(),
                            ),
                        ])),
                        Cell::from(""),
                        Cell::from(""),
                        count_cell(pf.additions, '+', theme::GREEN),
                        count_cell(pf.deletions, '-', theme::RED),
                    ])
                }
            }
        })
        .collect();

    // Wide enough for the biggest count on screen: every directory total
    // is at most the whole tree's, so size the columns from that.
    let (all_adds, all_dels) = app
        .files
        .iter()
        .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
    let width = |n: u64| format!("+{n}").len() as u16;
    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(gauge_width as u16),
            Constraint::Length(2 * digits as u16 + 1),
            Constraint::Length(width(all_adds)),
            Constraint::Length(width(all_dels)),
        ],
    )
    .column_spacing(2)
    .block(ui::panel(&format!("{total} files changed"), false))
    .row_highlight_style(theme::selected_row())
    .highlight_symbol(Line::from(Span::styled("▌", theme::accent())))
    .highlight_spacing(HighlightSpacing::Always);
    let (list_area, diff_area) = if app.diff.open {
        let [list, diff] =
            Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                .areas(list_area);
        (list, Some(diff))
    } else {
        (list_area, None)
    };
    f.render_stateful_widget(table, list_area, &mut app.table);
    app.diff.area = diff_area;
    if let Some(diff_area) = diff_area {
        draw_diff(f, diff_area, app);
    }

    if let Some(confirm) = &app.confirm {
        draw_confirm(f, area, app, confirm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, viewed: ViewedState) -> PrFile {
        PrFile {
            pr: 0,
            path: path.to_string(),
            additions: 1,
            deletions: 0,
            viewed,
            generated: None,
        }
    }

    fn files(paths: &[&str]) -> Vec<PrFile> {
        paths
            .iter()
            .map(|p| file(p, ViewedState::Unviewed))
            .collect()
    }

    fn label_of(dirs: &[DirNode], id: usize) -> &str {
        &dirs[id].label
    }

    #[test]
    fn single_child_chains_fold_into_one_node() {
        let (dirs, _) = build_tree(
            &files(&[
                "packages/core/src/index.ts",
                "packages/core/src/index.spec.ts",
                ".changeset/x.md",
            ]),
            &[],
        );
        let top: Vec<&str> = dirs[ROOT]
            .subdirs
            .iter()
            .map(|&d| label_of(&dirs, d))
            .collect();
        assert_eq!(top, [".changeset/", "packages/core/src/"]);
    }

    #[test]
    fn branching_directory_keeps_its_children_and_owns_every_file_below() {
        // a/ branches into b/ and c/ and has a file of its own.
        let (dirs, _) = build_tree(&files(&["a/b/x.ts", "a/c/d/y.ts", "a/z.ts"]), &[]);
        let a = dirs[ROOT].subdirs[0];
        assert_eq!(label_of(&dirs, a), "a/");
        let children: Vec<&str> = dirs[a]
            .subdirs
            .iter()
            .map(|&d| label_of(&dirs, d))
            .collect();
        assert_eq!(children, ["b/", "c/d/"]);
        assert_eq!(dirs[a].files, [2]);
        let mut under_a = dirs[a].all_files.clone();
        under_a.sort();
        assert_eq!(under_a, [0, 1, 2]);
    }

    #[test]
    fn partial_directory_marks_only_the_unviewed_and_dismissed() {
        let files = vec![
            file("d/a", ViewedState::Viewed),
            file("d/b", ViewedState::Unviewed),
            file("d/c", ViewedState::Dismissed),
        ];
        assert_eq!(toggle_plan(&files, &[0, 1, 2]), (vec![1, 2], true));
    }

    #[test]
    fn fully_viewed_directory_unmarks_everything() {
        let files = vec![
            file("d/a", ViewedState::Viewed),
            file("d/b", ViewedState::Viewed),
        ];
        assert_eq!(toggle_plan(&files, &[0, 1]), (vec![0, 1], false));
    }

    #[test]
    fn dismissed_file_gets_re_marked_not_unmarked() {
        let files = vec![file("d/a", ViewedState::Dismissed)];
        assert_eq!(toggle_plan(&files, &[0]), (vec![0], true));
    }

    fn app(files: Vec<PrFile>, hide_viewed: bool) -> FilesView {
        FilesView {
            dirs: build_tree(&files, &[]).0,
            pr_roots: vec![ROOT],
            scope: None,
            ledger: Ledger::new(&files),
            files,
            collapsed: HashSet::new(),
            hide_viewed,
            table: TableState::default(),
            status: None,
            confirm: None,
            filter: Filter::default(),
            filtering: false,
            worker: None,
            sources: Vec::new(),
            file_dir: Vec::new(),
            order_mode: false,
            order: Vec::new(),
            order_by: Vec::new(),
            prognost: None,
            diff: DiffPane::default(),
            open_cmd: None,
            open_request: None,
        }
    }

    fn row_labels(a: &FilesView) -> Vec<String> {
        a.rows()
            .into_iter()
            .map(|r| match r {
                RowRef::Dir(d) => a.dirs[d].label.clone(),
                RowRef::File { file, .. } => a.files[file].file_name().to_string(),
            })
            .collect()
    }

    #[test]
    fn rows_walk_the_tree_directories_before_files() {
        let a = app(files(&["a/b/x.ts", "a/z.ts", "root.md"]), false);
        assert_eq!(row_labels(&a), ["a/", "b/", "x.ts", "z.ts", "root.md"]);
    }

    #[test]
    fn hide_viewed_prunes_nested_directories_with_nothing_left() {
        let a = app(
            vec![
                file("a/done/x.ts", ViewedState::Viewed),
                file("a/todo/y.ts", ViewedState::Dismissed),
                file("a/z.ts", ViewedState::Viewed),
            ],
            true,
        );
        assert_eq!(row_labels(&a), ["a/", "todo/", "y.ts"]);
    }

    #[test]
    fn guides_follow_the_tree_and_match_row_order() {
        let a = app(files(&["a/b/x.ts", "a/b/y.ts", "a/z.ts", "root.md"]), false);
        let guided = a.rows_with_guides();
        let order: Vec<RowRef> = guided.iter().map(|(r, _)| *r).collect();
        assert_eq!(order, a.rows());
        let guides: Vec<&str> = guided.iter().map(|(_, g)| g.as_str()).collect();
        // a/ ─┬ b/ ─┬ x.ts
        //     │     └ y.ts
        //     └ z.ts
        // root.md
        assert_eq!(guides, ["", "├─ ", "│  ├─ ", "│  └─ ", "└─ ", ""]);
    }

    #[test]
    fn collapsing_hides_every_descendant_but_keeps_the_row() {
        let mut a = app(files(&["a/b/x.ts", "a/z.ts", "root.md"]), false);
        let a_dir = a.dirs[ROOT].subdirs[0];
        a.collapsed.insert(a_dir);
        assert_eq!(row_labels(&a), ["a/", "root.md"]);
    }

    #[test]
    fn left_folds_an_open_dir_then_steps_out_to_its_parent() {
        let mut a = app(files(&["a/b/x.ts", "a/z.ts"]), false);
        let a_dir = a.dirs[ROOT].subdirs[0];
        let b_dir = a.dirs[a_dir].subdirs[0];
        a.table.select(Some(1)); // b/
        a.go_left(RowRef::Dir(b_dir));
        assert!(a.collapsed.contains(&b_dir));
        a.go_left(RowRef::Dir(b_dir));
        assert_eq!(a.current(), Some(RowRef::Dir(a_dir)));
    }

    fn multi_pr_app() -> FilesView {
        let mut fs = files(&["src/app.ts", "terraform/main.tf", "README.md"]);
        fs[1].pr = 1;
        fs[2].pr = 1;
        let labels = ["app#1".to_string(), "infra#2".to_string()];
        let (dirs, pr_roots) = build_tree(&fs, &labels);
        FilesView {
            dirs,
            pr_roots,
            scope: None,
            ledger: Ledger::new(&fs),
            files: fs,
            collapsed: HashSet::new(),
            hide_viewed: false,
            table: TableState::default(),
            status: None,
            confirm: None,
            filter: Filter::default(),
            filtering: false,
            worker: None,
            sources: Vec::new(),
            file_dir: Vec::new(),
            order_mode: false,
            order: Vec::new(),
            order_by: Vec::new(),
            prognost: None,
            diff: DiffPane::default(),
            open_cmd: None,
            open_request: None,
        }
    }

    #[test]
    fn several_prs_sit_side_by_side_under_their_own_roots() {
        let a = multi_pr_app();
        assert_eq!(
            row_labels(&a),
            [
                "app#1",
                "src/",
                "app.ts",
                "infra#2",
                "terraform/",
                "main.tf",
                "README.md"
            ]
        );
        assert_eq!(a.viewed_counts(), (0, 3));
    }

    #[test]
    fn scoping_to_one_pr_shows_just_its_tree_and_counts() {
        let mut a = multi_pr_app();
        a.set_scope(Some(1));
        assert_eq!(row_labels(&a), ["terraform/", "main.tf", "README.md"]);
        assert_eq!(a.viewed_counts(), (0, 2));
    }

    #[test]
    fn v_on_a_pr_root_covers_that_pr_only() {
        let a = multi_pr_app();
        let infra = a.pr_roots[1];
        let mut covered = a.dirs[infra].all_files.clone();
        covered.sort();
        assert_eq!(covered, [1, 2]);
    }

    #[test]
    fn m_asks_first_and_only_y_marks_the_generated_files() {
        let mut fs = files(&["src/a.rs", "pnpm-lock.yaml", "ui/__snapshots__/b.snap"]);
        fs[1].generated = Some("lockfile");
        fs[2].generated = Some("snapshot");
        let mut a = app(fs, false);
        a.handle_key(KeyCode::Char('m'));
        // In tree order: the snapshot's directory comes before root files.
        assert_eq!(
            a.confirm.as_ref().map(|c| c.targets.as_slice()),
            Some(&[2, 1][..])
        );
        a.handle_key(KeyCode::Char('n'));
        assert!(a.confirm.is_none());
        assert_eq!(
            a.files[1].viewed,
            ViewedState::Unviewed,
            "cancel writes nothing"
        );
        a.handle_key(KeyCode::Char('m'));
        a.handle_key(KeyCode::Char('y'));
        assert_eq!(a.files[1].viewed, ViewedState::Viewed);
        assert_eq!(a.files[2].viewed, ViewedState::Viewed);
        assert_eq!(a.files[0].viewed, ViewedState::Unviewed);
    }

    #[test]
    fn while_the_dialog_is_open_other_keys_do_nothing() {
        let mut fs = files(&["src/a.rs", "yarn.lock"]);
        fs[1].generated = Some("lockfile");
        let mut a = app(fs, false);
        a.handle_key(KeyCode::Char('m'));
        a.handle_key(KeyCode::Char('v'));
        a.handle_key(KeyCode::Char('H'));
        assert!(a.is_modal() && !a.hide_viewed);
        assert_eq!(a.files[0].viewed, ViewedState::Unviewed);
    }

    #[test]
    fn o_lists_files_in_reading_order_and_keeps_v_working() {
        let mut a = app(
            files(&[
                "src/__tests__/retry.test.ts",
                "README.md",
                "src/retry.ts",
                "db/schema.sql",
            ]),
            false,
        );
        a.file_dir = file_dirs(&a.dirs, a.files.len());
        a.order = vec![reading_order::order(&a.files, &[0, 1, 2, 3], &[])];
        a.order_by = vec![OrderBy::Kind];
        a.handle_key(KeyCode::Char('o'));
        assert!(a.order_mode);
        assert_eq!(
            row_labels(&a),
            ["schema.sql", "retry.ts", "retry.test.ts", "README.md"]
        );
        a.handle_key(KeyCode::Char('v')); // first row: the schema
        assert_eq!(a.files[3].viewed, ViewedState::Viewed);
        a.handle_key(KeyCode::Char('o'));
        assert!(!a.order_mode);
    }

    #[test]
    fn enter_on_a_file_opens_the_diff_and_esc_closes_it() {
        let mut a = app(files(&["src/a.rs", "src/b.rs"]), false);
        a.table.select(Some(1));
        assert_eq!(a.selected_file(), Some(0));
        a.handle_key(KeyCode::Enter);
        assert!(a.takes_esc(), "the diff is open");
        a.handle_key(KeyCode::Char('J'));
        assert_eq!(a.diff.scroll, 10);
        a.handle_key(KeyCode::Char('j'));
        assert_eq!(a.selected_file(), Some(1), "j still moves the cursor");
        a.handle_key(KeyCode::Esc);
        assert!(!a.takes_esc());
        assert!(a.take_open_request().is_none());
    }

    #[test]
    fn the_wheel_scrolls_the_diff_under_the_pointer_and_moves_the_cursor_elsewhere() {
        let mut a = app(files(&["src/a.rs", "src/b.rs"]), false);
        a.table.select(Some(1));
        a.handle_key(KeyCode::Enter);
        a.diff.area = Some(Rect::new(40, 0, 60, 20));
        a.wheel(true, Position::new(50, 5));
        assert_eq!((a.diff.scroll, a.selected_file()), (3, Some(0)));
        a.wheel(true, Position::new(10, 5));
        assert_eq!((a.diff.scroll, a.selected_file()), (3, Some(1)));
    }

    #[test]
    fn slash_filters_by_path_and_m_marks_every_match_after_asking() {
        let mut a = app(
            files(&["src/a.spec.ts", "src/a.ts", "lib/b.spec.ts", "README.md"]),
            false,
        );
        a.handle_key(KeyCode::Char('/'));
        assert!(a.is_modal(), "typing goes to the filter");
        for c in "SPEC".chars() {
            a.handle_key(KeyCode::Char(c));
        }
        a.handle_key(KeyCode::Enter);
        assert!(!a.is_modal());
        let mut shown: Vec<usize> = a
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                RowRef::File { file, .. } => Some(file),
                RowRef::Dir(_) => None,
            })
            .collect();
        shown.sort();
        assert_eq!(shown, [0, 2], "case-insensitive, any depth");

        a.handle_key(KeyCode::Char('m'));
        let confirm = a.confirm.as_ref().expect("asks first");
        assert_eq!(confirm.what, Bulk::Matching("SPEC".into()));
        a.handle_key(KeyCode::Char('y'));
        let viewed: Vec<bool> = a
            .files
            .iter()
            .map(|f| f.viewed == ViewedState::Viewed)
            .collect();
        assert_eq!(viewed, [true, false, true, false]);

        assert!(a.takes_esc());
        a.handle_key(KeyCode::Esc);
        assert!(
            a.filter.is_empty() && !a.takes_esc(),
            "esc clears the filter"
        );
    }

    #[test]
    fn every_word_of_the_filter_must_appear() {
        assert!(Filter::new("web spec").matches("apps/web/src/Button.spec.ts"));
        assert!(!Filter::new("web spec").matches("apps/api/src/Button.spec.ts"));
        assert!(Filter::new("  ").matches("anything"));
        assert!(!Filter::new("  ").is_active());
    }

    #[test]
    fn words_are_regexes_and_broken_ones_match_as_text() {
        let f = Filter::new(r"^apps/(web|api)/.*\.SPEC\.ts$");
        assert!(f.matches("apps/api/src/Button.spec.ts"));
        assert!(!f.matches("apps/admin/src/Button.spec.ts"));
        assert!(!f.matches("apps/api/src/Button.spec.tsx"));
        assert!(!f.literal);

        let glob = Filter::new("*.spec.ts");
        assert!(!glob.literal);
        assert!(glob.matches("apps/web/src/Button.SPEC.ts"));
        assert!(!glob.matches("apps/web/src/Button.ts"));
        assert!(!glob.matches("apps/web/src/Button.spec.tsx"));
        let rooted = Filter::new("apps/**/*.ts");
        assert!(rooted.matches("apps/web/src/a.ts"));
        assert!(!rooted.matches("lib/apps/a.ts"));
        assert!(is_glob("src/*/index.ts"));
        assert!(!is_glob(r".*\.ts$"));
        assert!(!is_glob("(a|b)*"));

        let half = Filter::new("(web");
        assert!(half.literal);
        assert!(half.matches("x/(web)/y"));
        assert!(!half.matches("apps/web/a.ts"));
    }

    #[test]
    fn with_a_filter_v_on_a_folder_covers_only_its_matches() {
        let mut a = app(
            files(&["src/a.spec.ts", "src/a.ts", "lib/b.spec.ts"]),
            false,
        );
        a.filter.set("*.spec.ts".to_string());
        a.clamp();
        let src = a
            .rows()
            .iter()
            .position(|r| matches!(r, RowRef::Dir(d) if label_of(&a.dirs, *d) == "src/"))
            .expect("src/ is shown");
        a.table.select(Some(src));
        a.handle_key(KeyCode::Char('V'));
        assert!(
            a.confirm.is_none(),
            "V on a folder doesn't ask, as without a filter"
        );
        let viewed: Vec<bool> = a
            .files
            .iter()
            .map(|f| f.viewed == ViewedState::Viewed)
            .collect();
        assert_eq!(viewed, [true, false, false], "not src/a.ts, not lib/");
        let RowRef::Dir(d) = a.rows()[src] else {
            unreachable!()
        };
        assert_eq!(a.dir_files(d), [0], "its count is of matches only");
    }
}
