//! How long checks usually run, from the history log: what an agent is told
//! to expect of the jobs it waits behind, and of its own.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use crate::config::Task;
use crate::history::{self, Entry, HISTORY_FILE};
use crate::queue::Job;
use crate::report::{self, CheckResult};

/// The latest part of the log that is read: enough for usual times, at a
/// cost that does not grow with the log.
const RECENT_BYTES: u64 = 4 << 20;

/// Fewer passed runs than this say too little to be called usual.
const LEAST_RUNS: usize = 3;

/// A task as it was run: its workload (its repository, whose worktrees run
/// the same tasks; see [`crate::config::workload`]), name, weight (as it
/// counts on the machine), `min_weight` (if fewer than its weight),
/// resources and extra arguments. A task of another repository is another
/// workload, though it may share the name, and a run of one file of a
/// suite says little about the whole suite.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Setting {
    pub workload: String,
    pub task: String,
    pub weight: u32,
    pub min_weight: Option<u32>,
    pub uses: BTreeMap<String, u32>,
    pub args: Vec<String>,
}

impl Setting {
    pub fn of_job(job: &Job) -> Setting {
        let least = job.least();
        Setting {
            workload: crate::config::workload(&job.checkout),
            task: job.task.clone(),
            weight: job.weight,
            min_weight: (least < job.weight).then_some(least),
            uses: job
                .uses
                .iter()
                .map(|u| (u.name.clone(), u.amount))
                .collect(),
            args: job.args.clone(),
        }
    }

    /// `task` of the checkout `checkout` (the resolved directory of its
    /// `.loadmaster.toml`), run without extra arguments on a machine of
    /// `capacity`.
    pub fn of_task(checkout: &Path, task: &Task, capacity: u32) -> Setting {
        let weight = task.weight.min(capacity);
        let least = task.min_weight.unwrap_or(weight).min(weight);
        Setting {
            workload: crate::config::workload(checkout),
            task: task.name.clone(),
            weight,
            min_weight: (least < weight).then_some(least),
            uses: task.uses.clone(),
            args: Vec::new(),
        }
    }
}

/// How long a setting's passed checks ran, and what that says it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunTime {
    /// The median: what is usual, not what is left of a running one.
    pub median: Duration,
    /// Of how many passed runs.
    pub runs: usize,
    /// When the latest of them ended, in Unix milliseconds.
    pub latest_at: u64,
}

/// Run times of passed checks, by setting.
#[derive(Debug, Default)]
pub struct Usual {
    /// Each setting's run times, sorted, and when its latest run ended.
    runs: HashMap<Setting, (Vec<u64>, u64)>,
}

impl Usual {
    /// From the end of the history log in `state_dir`; none if there is
    /// none, which nothing that uses it depends on.
    pub fn load(state_dir: &Path) -> Usual {
        let entries =
            history::read_recent(&state_dir.join(HISTORY_FILE), RECENT_BYTES).unwrap_or_default();
        Usual::of(&entries)
    }

    pub fn of(entries: &[Entry]) -> Usual {
        let mut runs: HashMap<Setting, (Vec<u64>, u64)> = HashMap::new();
        let mut workloads = crate::config::Workloads::default();
        for check in report::checks(entries) {
            let (CheckResult::Passed, Some(run_ms)) = (check.result, check.run_ms) else {
                continue;
            };
            let ended = check.queued_at + check.wait_ms + run_ms;
            let setting = Setting {
                workload: workloads.of(check.workload.as_deref(), &check.checkout),
                task: check.task,
                weight: check.weight,
                min_weight: check.min_weight,
                uses: check.uses,
                args: check.args,
            };
            let (times, latest) = runs.entry(setting).or_default();
            times.push(run_ms);
            *latest = (*latest).max(ended);
        }
        for (times, _) in runs.values_mut() {
            times.sort_unstable();
        }
        Usual { runs }
    }

    /// The usual run time of `setting`, if it passed often enough.
    pub fn run_time(&self, setting: &Setting) -> Option<RunTime> {
        let (times, latest_at) = self
            .runs
            .get(setting)
            .filter(|(t, _)| t.len() >= LEAST_RUNS)?;
        Some(RunTime {
            median: Duration::from_millis(times[times.len() / 2]),
            runs: times.len(),
            latest_at: *latest_at,
        })
    }

    /// [`Usual::run_time`] of a job in the queue.
    pub fn of_job(&self, job: &Job) -> Option<RunTime> {
        self.run_time(&Setting::of_job(job))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Outcome;

    fn passed(job: i64, task: &str, weight: u32, run_ms: u64) -> Entry {
        Entry {
            job,
            task: task.to_owned(),
            weight,
            dir: "/repo".to_owned(),
            checkout: None,
            workload: None,
            args: Vec::new(),
            capacity: 8,
            measure_only: false,
            outcome: Outcome::Exited,
            queued_at: 1_000 * job as u64,
            wait_ms: 0,
            run_ms: Some(run_ms),
            cpu_ms: None,
            load: None,
            machine_busy: None,
            signal: None,
            exit_code: Some(0),
            started: None,
            ok: Some(true),
            stopped: false,
            granted: None,
            min_weight: None,
            uses: BTreeMap::new(),
            limits: BTreeMap::new(),
        }
    }

    #[test]
    fn the_usual_run_time_is_the_median_of_passed_runs_of_one_setting() {
        let setting = |weight, args: &[&str]| Setting {
            workload: "/repo".to_owned(),
            task: "test".to_owned(),
            weight,
            min_weight: None,
            uses: BTreeMap::new(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        };
        let mut entries: Vec<Entry> = [9_000, 1_000, 3_000, 5_000]
            .iter()
            .enumerate()
            .map(|(i, &ms)| passed(i as i64, "test", 4, ms))
            .collect();
        // A failed run, runs of another weight, of one file and of another
        // checkout, do not count.
        entries.push(Entry {
            ok: Some(false),
            exit_code: Some(1),
            ..passed(10, "test", 4, 60_000)
        });
        entries.extend((20..25).map(|job| passed(job, "test", 2, 100)));
        entries.extend((30..35).map(|job| Entry {
            args: vec!["a.test.ts".to_owned()],
            ..passed(job, "test", 4, 10)
        }));
        entries.extend((40..45).map(|job| Entry {
            workload: Some("/other".to_owned()),
            ..passed(job, "test", 4, 77)
        }));
        let usual = Usual::of(&entries);
        let full = usual.run_time(&setting(4, &[])).unwrap();
        assert_eq!((full.median, full.runs), (Duration::from_millis(5_000), 4));
        // The first got in line at 0 and ran 9 s: it ended last.
        assert_eq!(full.latest_at, 9_000);
        assert_eq!(
            usual.run_time(&setting(2, &[])).map(|r| r.median),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            usual
                .run_time(&setting(4, &["a.test.ts"]))
                .map(|r| r.median),
            Some(Duration::from_millis(10))
        );
        // Too few runs, or none.
        assert_eq!(Usual::of(&entries[..2]).run_time(&setting(4, &[])), None);
        assert_eq!(
            usual.run_time(&Setting {
                task: "lint".to_owned(),
                ..setting(1, &[])
            }),
            None
        );
    }
}
