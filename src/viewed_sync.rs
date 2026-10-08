//! Optimistic Viewed updates: the screen changes the moment a key is pressed,
//! and the write to GitHub happens on a background thread.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use crate::github;
use crate::model::{PrFile, ViewedState};

/// One write to one pull request. A toggle spanning several PRs becomes one
/// job per PR, so a failure on one rolls back only that PR's files.
pub struct Job {
    id: u64,
    pr: usize,
    paths: Vec<String>,
    viewed: bool,
}

pub struct Outcome {
    pub id: u64,
    pub error: Option<String>,
}

/// Bookkeeping that makes optimistic updates safe to roll back.
///
/// Jobs run one at a time in the order they were queued, so GitHub sees
/// toggles in the order they were pressed. When a job fails, each of its
/// files goes back to the last state GitHub actually confirmed — unless a
/// newer toggle has touched that file since, in which case the newer one
/// owns it and settles it when it finishes.
pub struct Ledger {
    confirmed: Vec<ViewedState>,
    latest: HashMap<usize, u64>,
    jobs: HashMap<u64, (Vec<usize>, ViewedState)>,
    next_id: u64,
}

impl Ledger {
    pub fn new(files: &[PrFile]) -> Self {
        Ledger {
            confirmed: files.iter().map(|f| f.viewed).collect(),
            latest: HashMap::new(),
            jobs: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn pending(&self) -> usize {
        self.jobs.len()
    }

    /// Sets every target to the new state right away and returns the jobs —
    /// one per pull request involved — that will make GitHub agree.
    pub fn apply(&mut self, files: &mut [PrFile], targets: &[usize], viewed: bool) -> Vec<Job> {
        let mut by_pr: Vec<(usize, Vec<usize>)> = Vec::new();
        for &f in targets {
            match by_pr.iter_mut().find(|(pr, _)| *pr == files[f].pr) {
                Some((_, group)) => group.push(f),
                None => by_pr.push((files[f].pr, vec![f])),
            }
        }
        by_pr
            .into_iter()
            .map(|(pr, group)| self.apply_one(files, pr, &group, viewed))
            .collect()
    }

    fn apply_one(
        &mut self,
        files: &mut [PrFile],
        pr: usize,
        targets: &[usize],
        viewed: bool,
    ) -> Job {
        let id = self.next_id;
        self.next_id += 1;
        let state = if viewed {
            ViewedState::Viewed
        } else {
            ViewedState::Unviewed
        };
        for &f in targets {
            files[f].viewed = state;
            self.latest.insert(f, id);
        }
        self.jobs.insert(id, (targets.to_vec(), state));
        Job {
            id,
            pr,
            paths: targets.iter().map(|&f| files[f].path.clone()).collect(),
            viewed,
        }
    }

    /// Records a job's outcome and returns how many files were rolled back.
    pub fn settle(&mut self, files: &mut [PrFile], id: u64, ok: bool) -> usize {
        let Some((targets, state)) = self.jobs.remove(&id) else {
            return 0;
        };
        let mut reverted = 0;
        for f in targets {
            if ok {
                self.confirmed[f] = state;
            }
            if self.latest.get(&f) == Some(&id) {
                self.latest.remove(&f);
                if !ok {
                    files[f].viewed = self.confirmed[f];
                    reverted += 1;
                }
            }
        }
        reverted
    }
}

/// A single background thread draining the job queue in order.
pub struct Worker {
    jobs: Sender<Job>,
    pub outcomes: Receiver<Outcome>,
    handle: JoinHandle<()>,
}

impl Worker {
    /// `pull_request_ids[i]` is the GraphQL node id of the i-th opened PR.
    pub fn spawn(pull_request_ids: Vec<String>) -> Self {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (outcome_tx, outcomes) = mpsc::channel();
        let handle = thread::spawn(move || {
            for job in job_rx {
                let paths: Vec<&str> = job.paths.iter().map(String::as_str).collect();
                let error = github::set_viewed(&pull_request_ids[job.pr], &paths, job.viewed)
                    .err()
                    .map(|e| format!("{e:#}"));
                if outcome_tx.send(Outcome { id: job.id, error }).is_err() {
                    break;
                }
            }
        });
        Worker {
            jobs,
            outcomes,
            handle,
        }
    }

    pub fn send(&self, job: Job) {
        // The worker only stops once this sender is dropped, so this can't fail.
        let _ = self.jobs.send(job);
    }

    /// Stops accepting jobs, waits for everything already queued to reach
    /// GitHub, and hands back the outcomes that hadn't been read yet.
    pub fn finish(self) -> Vec<Outcome> {
        drop(self.jobs);
        let _ = self.handle.join();
        self.outcomes.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(states: &[ViewedState]) -> Vec<PrFile> {
        states
            .iter()
            .enumerate()
            .map(|(i, &viewed)| PrFile {
                pr: 0,
                path: format!("f{i}"),
                additions: 0,
                deletions: 0,
                viewed,
                generated: None,
            })
            .collect()
    }

    #[test]
    fn apply_changes_the_screen_before_github_answers() {
        let mut fs = files(&[ViewedState::Unviewed, ViewedState::Dismissed]);
        let mut ledger = Ledger::new(&fs);
        let job = ledger.apply(&mut fs, &[0, 1], true).remove(0);
        assert_eq!(fs[0].viewed, ViewedState::Viewed);
        assert_eq!(fs[1].viewed, ViewedState::Viewed);
        assert_eq!(job.paths, ["f0", "f1"]);
        assert_eq!(ledger.pending(), 1);
    }

    #[test]
    fn failure_puts_back_what_github_last_confirmed() {
        // DISMISSED comes back as DISMISSED, not as "unviewed".
        let mut fs = files(&[ViewedState::Dismissed]);
        let mut ledger = Ledger::new(&fs);
        let job = ledger.apply(&mut fs, &[0], true).remove(0);
        assert_eq!(ledger.settle(&mut fs, job.id, false), 1);
        assert_eq!(fs[0].viewed, ViewedState::Dismissed);
        assert_eq!(ledger.pending(), 0);
    }

    #[test]
    fn failure_of_a_later_toggle_returns_to_the_earlier_confirmed_one() {
        let mut fs = files(&[ViewedState::Unviewed]);
        let mut ledger = Ledger::new(&fs);
        let first = ledger.apply(&mut fs, &[0], true).remove(0);
        let second = ledger.apply(&mut fs, &[0], false).remove(0);
        ledger.settle(&mut fs, first.id, true);
        ledger.settle(&mut fs, second.id, false);
        assert_eq!(fs[0].viewed, ViewedState::Viewed);
    }

    #[test]
    fn failure_does_not_undo_a_newer_toggle_still_in_flight() {
        let mut fs = files(&[ViewedState::Unviewed, ViewedState::Unviewed]);
        let mut ledger = Ledger::new(&fs);
        let dir = ledger.apply(&mut fs, &[0, 1], true).remove(0);
        let single = ledger.apply(&mut fs, &[1], false).remove(0);
        // The directory job fails: f0 is rolled back, f1 belongs to `single`.
        assert_eq!(ledger.settle(&mut fs, dir.id, false), 1);
        assert_eq!(fs[0].viewed, ViewedState::Unviewed);
        assert_eq!(fs[1].viewed, ViewedState::Unviewed);
        ledger.settle(&mut fs, single.id, true);
        assert_eq!(fs[1].viewed, ViewedState::Unviewed);
    }

    #[test]
    fn a_toggle_across_pull_requests_splits_into_one_job_each() {
        let mut fs = files(&[
            ViewedState::Unviewed,
            ViewedState::Unviewed,
            ViewedState::Unviewed,
        ]);
        fs[1].pr = 1;
        let mut ledger = Ledger::new(&fs);
        let jobs = ledger.apply(&mut fs, &[0, 1, 2], true);
        let split: Vec<(usize, Vec<String>)> =
            jobs.iter().map(|j| (j.pr, j.paths.clone())).collect();
        assert_eq!(
            split,
            [
                (0, vec!["f0".to_string(), "f2".to_string()]),
                (1, vec!["f1".to_string()])
            ]
        );
        // PR 1 failing leaves PR 0's files alone.
        ledger.settle(&mut fs, jobs[1].id, false);
        assert_eq!(fs[1].viewed, ViewedState::Unviewed);
        assert_eq!(fs[0].viewed, ViewedState::Viewed);
    }
}
