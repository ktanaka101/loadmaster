//! How long checks usually run, from the history log: what an agent is told
//! to expect of the jobs it waits behind, and of its own.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use crate::history::{self, Entry, HISTORY_FILE};
use crate::queue::Job;
use crate::report::{self, CheckResult};

/// The latest part of the log that is read: enough for usual times, at a
/// cost that does not grow with the log.
const RECENT_BYTES: u64 = 4 << 20;

/// Fewer passed runs than this say too little to be called usual.
const LEAST_RUNS: usize = 3;

/// A task as it was set up, as `report` groups checks: its name, weight,
/// `min_weight` (if fewer than its weight) and resources. Another setting's
/// runs say little about this one's.
type Setting = (String, u32, Option<u32>, BTreeMap<String, u32>);

/// Run times of passed checks, by setting.
#[derive(Debug, Default)]
pub struct Usual {
    runs: HashMap<Setting, Vec<u64>>,
}

impl Usual {
    /// From the history log in `state_dir`; none if there is none.
    pub fn load(state_dir: &Path) -> Usual {
        let entries =
            history::read_recent(&state_dir.join(HISTORY_FILE), RECENT_BYTES).unwrap_or_default();
        Usual::of(&entries)
    }

    pub fn of(entries: &[Entry]) -> Usual {
        let mut runs: HashMap<Setting, Vec<u64>> = HashMap::new();
        for check in report::checks(entries) {
            let (CheckResult::Passed, Some(run_ms)) = (check.result, check.run_ms) else {
                continue;
            };
            runs.entry((check.task, check.weight, check.min_weight, check.uses))
                .or_default()
                .push(run_ms);
        }
        for times in runs.values_mut() {
            times.sort_unstable();
        }
        Usual { runs }
    }

    /// The median run time of passed checks of `task` set up so, if it
    /// passed often enough. `weight` is as it counts on this machine
    /// (capped at the capacity); `min_weight` is given only if fewer.
    pub fn run_time(
        &self,
        task: &str,
        weight: u32,
        min_weight: Option<u32>,
        uses: &BTreeMap<String, u32>,
    ) -> Option<Duration> {
        let key = (task.to_owned(), weight, min_weight, uses.clone());
        let times = self.runs.get(&key).filter(|t| t.len() >= LEAST_RUNS)?;
        Some(Duration::from_millis(times[times.len() / 2]))
    }

    /// [`Usual::run_time`] of a job in the queue.
    pub fn of_job(&self, job: &Job) -> Option<Duration> {
        let least = job.least();
        let uses = job
            .uses
            .iter()
            .map(|u| (u.name.clone(), u.amount))
            .collect();
        self.run_time(
            &job.task,
            job.weight,
            (least < job.weight).then_some(least),
            &uses,
        )
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
        let none = BTreeMap::new();
        let mut entries: Vec<Entry> = [9_000, 1_000, 3_000, 5_000]
            .iter()
            .enumerate()
            .map(|(i, &ms)| passed(i as i64, "test", 4, ms))
            .collect();
        // A failed run, and runs of another weight, do not count.
        entries.push(Entry {
            ok: Some(false),
            exit_code: Some(1),
            ..passed(10, "test", 4, 60_000)
        });
        entries.extend((20..25).map(|job| passed(job, "test", 2, 100)));
        let usual = Usual::of(&entries);
        assert_eq!(
            usual.run_time("test", 4, None, &none),
            Some(Duration::from_millis(5_000))
        );
        assert_eq!(
            usual.run_time("test", 2, None, &none),
            Some(Duration::from_millis(100))
        );
        // Too few runs.
        let two = Usual::of(&entries[..2]);
        assert_eq!(two.run_time("test", 4, None, &none), None);
        assert_eq!(usual.run_time("lint", 1, None, &none), None);
    }
}
