//! `loadmaster tune`: what other settings of one workload's tasks (a
//! repository's, which all its worktrees share; see
//! [`crate::config::workload`]) would have done to this machine's checks,
//! replayed through the admission rule itself.
//!
//! A scenario, not a promise, and every assumption is said with it:
//! - Checks arrive when they did. An agent that waited less might have asked
//!   for its next check sooner, and a failure that was retried is demand
//!   that another setting might not have had.
//! - No CPU is busy outside the queue.
//! - A check runs as long as it did, except that a task with a `min_weight`
//!   (which declares that it sizes itself by LOADMASTER_WEIGHT) runs longer
//!   on fewer cores than it had: its CPU work, spread over the cores it had,
//!   is spread over fewer. A task without one is taken to run as long
//!   whatever cores it gets, as one that does not size itself would. More
//!   cores never shorten a run.
//! - What the replay calls over capacity is from each check's cores kept
//!   busy on average: an estimate, not a bound, since checks that average
//!   more than the capacity together may peak at different times, and ones
//!   that average less may peak together.
//! - The machine has the capacity it has now; a measure-only check is
//!   replayed as one admitted; a resource whose limits a record lacks (from
//!   0.15 and before) is taken as unlimited.
//! - Only the workload's own tasks change; every other check on the machine
//!   keeps what it recorded, and is replayed beside them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use crate::history::Limit;
use crate::queue::{self, Admission, Job, JobId, JobState, Use};
use crate::report::{self, Check, CheckResult};

/// What tune may change of a task: its weight, and the cores it may start
/// with (`min_weight`; its weight if none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskSetting {
    pub weight: u32,
    pub min_weight: Option<u32>,
}

impl TaskSetting {
    /// Capped at the capacity, as the machine counts them.
    fn capped(self, capacity: u32) -> TaskSetting {
        let weight = self.weight.min(capacity).max(1);
        TaskSetting {
            weight,
            min_weight: self.min_weight.map(|l| l.clamp(1, weight)),
        }
    }

    /// Whether it sizes itself by the cores it gets: what a `min_weight`
    /// declares, whatever its value. One without is taken to run as long
    /// whatever cores it gets.
    fn sizes_itself(self) -> bool {
        self.min_weight.is_some()
    }
}

/// The settings of a scenario: those of the workload's tasks, by name.
pub type Settings = BTreeMap<String, TaskSetting>;

/// A check as the replay sees it.
#[derive(Debug, Clone)]
pub struct Arrival {
    /// When it first got in line, in Unix milliseconds.
    pub at: u64,
    /// Its job ID: its place among checks that got in line at the same
    /// millisecond.
    pub job: i64,
    /// How long it ran, and the CPU time it used.
    pub run_ms: u64,
    pub cpu_ms: Option<u64>,
    /// How long it waited, as recorded.
    pub waited_ms: u64,
    /// The cores it ran with.
    pub had: u32,
    pub task: String,
    /// What it got in line with.
    pub setting: TaskSetting,
    pub uses: BTreeMap<String, u32>,
    pub limits: BTreeMap<String, Limit>,
    /// Its checkout, for the admission rule's per-checkout limits.
    pub checkout: String,
    /// Its workload: what settings it shares.
    pub workload: String,
    pub passed: bool,
}

impl Arrival {
    /// A check that ran, as recorded; `None` for one that never did.
    pub fn of(check: &Check, workloads: &mut crate::config::Workloads) -> Option<Arrival> {
        let run_ms = check.run_ms?;
        Some(Arrival {
            at: check.queued_at,
            job: check.job,
            run_ms,
            cpu_ms: check.cpu_ms,
            waited_ms: check.wait_ms,
            had: check.granted.unwrap_or(check.weight).max(1),
            task: check.task.clone(),
            setting: TaskSetting {
                weight: check.weight,
                // One as high as its weight still says it sizes itself.
                min_weight: check
                    .min_weight
                    .or(check.sizes_itself.then_some(check.weight)),
            },
            uses: check.uses.clone(),
            limits: check.limits.clone(),
            checkout: check.checkout.clone(),
            workload: workloads.of(check.workload.as_deref(), &check.checkout),
            passed: check.result == CheckResult::Passed,
        })
    }

    /// The cores it kept busy on average, as recorded, or the cores it had
    /// where its CPU time was not.
    fn busy(&self) -> f64 {
        match self.cpu_ms {
            Some(cpu) if self.run_ms > 0 => cpu as f64 / self.run_ms as f64,
            _ => f64::from(self.had),
        }
    }

    /// The cores it keeps busy on average when it runs `ran` ms with
    /// `cores`, set up as `setting`.
    fn busy_with(&self, cores: u32, ran: u64, setting: TaskSetting) -> f64 {
        if !setting.sizes_itself() || cores >= self.had {
            return self.busy();
        }
        match self.cpu_ms {
            Some(cpu) if ran > 0 => cpu as f64 / ran as f64,
            _ => f64::from(cores),
        }
    }

    /// How long it runs with `cores`, set up as `setting`: as it did, unless
    /// it sizes itself by its cores and gets fewer than it had, when its CPU
    /// work, spread over the cores it had, is spread over these.
    fn run_with(&self, cores: u32, setting: TaskSetting) -> u64 {
        if !setting.sizes_itself() || cores >= self.had {
            return self.run_ms;
        }
        let run = self.run_ms as f64;
        let ms = match self.cpu_ms {
            Some(cpu) => {
                let cpu = cpu as f64;
                let at_full = (cpu / f64::from(self.had)).min(run);
                run - at_full + cpu / f64::from(cores)
            }
            None => run * f64::from(self.had) / f64::from(cores),
        };
        ms.ceil() as u64
    }
}

/// What a replay found, of every check and of the workload's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// Every check's wait, and its time from getting in line to the end.
    pub waits: Vec<u64>,
    pub turnarounds: Vec<u64>,
    /// The workload's, by task: waits and times to the end.
    pub tasks: BTreeMap<String, (Vec<u64>, Vec<u64>)>,
    /// The share of the time some check ran when the cores the running
    /// checks kept busy, on average, were more than the capacity.
    pub over: f64,
}

/// A check while it runs in the replay.
#[derive(Debug)]
struct Running {
    rank: usize,
    end: u64,
    cores: u32,
    busy: f64,
}

/// Replays `arrivals` (in any order) on a machine of `capacity`, the checks
/// of `workload` set up as `settings` says (by task; one it does not name
/// keeps what it recorded).
pub fn replay(arrivals: &[Arrival], capacity: u32, workload: &str, settings: &Settings) -> Outcome {
    let mut order: Vec<usize> = (0..arrivals.len()).collect();
    order.sort_by_key(|&i| (arrivals[i].at, arrivals[i].job, i));
    // Each check's setting in this scenario, by its rank in line.
    let set: Vec<TaskSetting> = order
        .iter()
        .map(|&i| {
            let a = &arrivals[i];
            let ours = (a.workload == workload)
                .then(|| settings.get(&a.task).copied())
                .flatten();
            ours.unwrap_or(a.setting).capped(capacity)
        })
        .collect();
    let arrival = |rank: usize| &arrivals[order[rank]];
    let mut outcome = Outcome::default();
    let (mut next, mut waiting, mut running) = (0, Vec::<usize>::new(), Vec::<Running>::new());
    let (mut busy_time, mut over_time) = (0u64, 0u64);
    let Some(first) = order.first() else {
        return outcome;
    };
    let mut now = arrivals[*first].at;
    loop {
        while next < order.len() && arrival(next).at <= now {
            waiting.push(next);
            next += 1;
        }
        running.retain(|r| r.end > now);
        if !waiting.is_empty() {
            for (rank, cores) in admit(&waiting, &running, arrivals, &order, &set, capacity) {
                let a = arrival(rank);
                let ran = a.run_with(cores, set[rank]);
                let busy = a.busy_with(cores, ran, set[rank]);
                running.push(Running {
                    rank,
                    end: now + ran,
                    cores,
                    busy,
                });
                waiting.retain(|&w| w != rank);
                let (wait, turnaround) = (now - a.at, now + ran - a.at);
                outcome.waits.push(wait);
                outcome.turnarounds.push(turnaround);
                if a.workload == workload {
                    let entry = outcome.tasks.entry(a.task.clone()).or_default();
                    entry.0.push(wait);
                    entry.1.push(turnaround);
                }
            }
        }
        #[cfg(test)]
        {
            let held: u32 = running.iter().map(|r| r.cores).sum();
            assert!(
                held <= capacity,
                "{held} cores held of {capacity}: {running:?}"
            );
        }
        let next_arrival = (next < order.len()).then(|| arrival(next).at);
        let next_end = running.iter().map(|r| r.end).min();
        let Some(then) = [next_arrival, next_end].into_iter().flatten().min() else {
            break;
        };
        // Nothing can start before then: what runs, runs until then.
        let dt = then.saturating_sub(now);
        if !running.is_empty() {
            busy_time += dt;
            if running.iter().map(|r| r.busy).sum::<f64>() > f64::from(capacity) + 1e-9 {
                over_time += dt;
            }
        }
        // A run of no time ends at once: it is gone by the next turn.
        now = then;
    }
    outcome.over = if busy_time == 0 {
        0.0
    } else {
        over_time as f64 / busy_time as f64
    };
    outcome
}

/// Which of the waiting checks the admission rule starts now, and with how
/// many cores: the rule itself, on the line as it stands.
fn admit(
    waiting: &[usize],
    running: &[Running],
    arrivals: &[Arrival],
    order: &[usize],
    set: &[TaskSetting],
    capacity: u32,
) -> Vec<(usize, u32)> {
    // In line order, which is the order they got in line.
    let mut line: Vec<(usize, Option<u32>)> = running
        .iter()
        .map(|r| (r.rank, Some(r.cores)))
        .chain(waiting.iter().map(|&w| (w, None)))
        .collect();
    line.sort_by_key(|&(rank, _)| rank);
    let jobs: Vec<Job> = line
        .iter()
        .map(|&(rank, granted)| job(rank, &arrivals[order[rank]], set[rank], granted, capacity))
        .collect();
    queue::explain_with_cores(&jobs, None)
        .into_iter()
        .filter_map(|(id, admission, cores)| {
            let rank = usize::try_from(id - 1).ok()?;
            let waits = waiting.contains(&rank);
            (waits && admission == Admission::Admitted)
                .then(|| (rank, cores.unwrap_or(set[rank].weight)))
        })
        .collect()
}

/// The job a check is in the replay's line.
fn job(rank: usize, a: &Arrival, s: TaskSetting, granted: Option<u32>, capacity: u32) -> Job {
    let checkout = PathBuf::from(&a.checkout);
    Job {
        id: rank as JobId + 1,
        task: a.task.clone(),
        weight: s.weight,
        state: if granted.is_some() {
            JobState::Running
        } else {
            JobState::Waiting
        },
        dir: checkout.clone(),
        args: Vec::new(),
        command: String::new(),
        owner: None,
        child: None,
        enqueued_at: UNIX_EPOCH,
        parked_at: None,
        started_at: None,
        capacity,
        measure_only: false,
        uses: a
            .uses
            .iter()
            .map(|(name, &amount)| {
                let limit = a.limits.get(name).copied().unwrap_or_default();
                Use {
                    name: name.clone(),
                    amount,
                    capacity: limit.capacity,
                    per_checkout: limit.per_checkout,
                }
            })
            .collect(),
        checkout,
        adapt_cpu: false,
        stop_signal: libc::SIGTERM,
        granted,
        state_since: None,
        min_weight: Some(s.min_weight.unwrap_or(s.weight)),
    }
}

/// Fewer passed runs say too little of the cores a task keeps busy.
pub const LEAST_RUNS: usize = 5;

/// A weight from what a task kept busy: the 90th percentile of the cores
/// its passed runs kept busy, rounded up, within 1 to the capacity, with
/// the runs it rests on. `None` if too few runs.
pub fn measured_weight(
    arrivals: &[Arrival],
    workload: &str,
    task: &str,
    capacity: u32,
) -> Option<(u32, f64, usize)> {
    let busy: Vec<f64> = arrivals
        .iter()
        .filter(|a| {
            a.workload == workload
                && a.task == task
                && a.passed
                && a.cpu_ms.is_some()
                && a.run_ms > 0
        })
        .map(Arrival::busy)
        .collect();
    if busy.len() < LEAST_RUNS {
        return None;
    }
    let p90 = report::percentile(&busy, 0.9)?;
    let weight = (p90.ceil() as u32).clamp(1, capacity);
    Some((weight, p90, busy.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrival(at: u64, task: &str, weight: u32, run_ms: u64, cpu_ms: u64) -> Arrival {
        Arrival {
            at,
            job: 0,
            run_ms,
            cpu_ms: Some(cpu_ms),
            waited_ms: 0,
            had: weight,
            task: task.to_owned(),
            setting: TaskSetting {
                weight,
                min_weight: None,
            },
            uses: BTreeMap::new(),
            limits: BTreeMap::new(),
            checkout: "/repo".to_owned(),
            workload: "/repo".to_owned(),
            passed: true,
        }
    }

    #[test]
    fn checks_that_fit_run_at_once_and_others_wait_their_turn() {
        // Two checks of 4 on 4 cores, a second apart, each 10 s.
        let arrivals = [
            arrival(0, "test", 4, 10_000, 40_000),
            arrival(1_000, "test", 4, 10_000, 40_000),
        ];
        let out = replay(&arrivals, 4, "/repo", &Settings::new());
        assert_eq!(out.waits, [0, 9_000]);
        assert_eq!(out.turnarounds, [10_000, 19_000]);
        assert_eq!(out.over, 0.0);
        // At weight 2 they overlap: no wait, but 8 cores busy on 4.
        let half = Settings::from([(
            "test".to_owned(),
            TaskSetting {
                weight: 2,
                min_weight: None,
            },
        )]);
        let out = replay(&arrivals, 4, "/repo", &half);
        assert_eq!(out.waits, [0, 0]);
        assert!(out.over > 0.8, "{}", out.over);
    }

    #[test]
    fn a_task_that_may_start_with_fewer_runs_longer_on_fewer() {
        // 50 s at 8 cores, then 50 s of waiting for I/O: its CPU phase
        // takes twice as long on 4 cores.
        let a = arrival(0, "test", 8, 100_000, 400_000);
        let consents = TaskSetting {
            weight: 8,
            min_weight: Some(1),
        };
        assert_eq!(a.run_with(4, consents), 150_000);
        assert_eq!(a.run_with(8, consents), 100_000);
        // More cores never shorten it; one that does not consent runs as
        // it did.
        assert_eq!(a.run_with(16, consents), 100_000);
        let fixed = TaskSetting {
            weight: 8,
            min_weight: None,
        };
        assert_eq!(a.run_with(4, fixed), 100_000);
        // A min_weight as high as the weight still says it sizes itself.
        let equal = TaskSetting {
            weight: 4,
            min_weight: Some(4),
        };
        assert_eq!(a.run_with(4, equal), 150_000);
        // Without its CPU time: the cores it gets, all the while.
        let unmeasured = Arrival {
            cpu_ms: None,
            ..a.clone()
        };
        assert_eq!(unmeasured.run_with(4, consents), 200_000);
        assert_eq!(unmeasured.busy_with(4, 200_000, consents), 4.0);
    }

    #[test]
    fn a_record_that_sizes_itself_runs_longer_on_fewer_cores() {
        // Recorded with min_weight = weight = 8: the record says it sizes
        // itself; replayed on 4 cores it takes longer.
        let check = Check {
            sizes_itself: true,
            ..crate::report::tests::check_for_tune(8, 100_000, 400_000)
        };
        let a = Arrival::of(&check, &mut crate::config::Workloads::default()).unwrap();
        let out = replay(&[a], 4, "/elsewhere", &Settings::new());
        assert_eq!(out.turnarounds, [150_000]);
        let fixed = Check {
            sizes_itself: false,
            ..check
        };
        let a = Arrival::of(&fixed, &mut crate::config::Workloads::default()).unwrap();
        assert_eq!(
            replay(&[a], 4, "/elsewhere", &Settings::new()).turnarounds,
            [100_000]
        );
    }

    #[test]
    fn other_workloads_keep_what_they_recorded() {
        let mut other = arrival(0, "test", 4, 10_000, 40_000);
        other.workload = "/other".to_owned();
        let arrivals = [other, arrival(1_000, "test", 4, 10_000, 40_000)];
        // Lowering this workload's `test` lets it start beside the other's.
        let lower = Settings::from([(
            "test".to_owned(),
            TaskSetting {
                weight: 1,
                min_weight: None,
            },
        )]);
        let out = replay(&arrivals, 4, "/repo", &lower);
        assert_eq!(out.tasks["test"].0, [9_000]);
        let out = replay(&arrivals, 5, "/repo", &lower);
        assert_eq!(out.tasks["test"].0, [0]);
    }

    #[test]
    fn every_check_starts_once_and_the_cores_held_never_pass_the_capacity() {
        // A line of random checks, some that may start with fewer cores.
        let mut seed: u64 = 7;
        let mut next = |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        for round in 0..30 {
            let mut arrivals = Vec::new();
            let mut at = 0;
            for i in 0..60 {
                at += next(3_000);
                let weight = 1 + next(6) as u32;
                let mut a = arrival(
                    at,
                    ["lint", "unit", "e2e"][i % 3],
                    weight,
                    1 + next(20_000),
                    0,
                );
                a.cpu_ms = Some(a.run_ms * u64::from(weight) / 2);
                if next(2) == 0 {
                    a.setting.min_weight = Some(1);
                }
                if next(4) == 0 {
                    a.workload = "/other".to_owned();
                }
                arrivals.push(a);
            }
            let halve = Settings::from([(
                "unit".to_owned(),
                TaskSetting {
                    weight: 2,
                    min_weight: Some(1),
                },
            )]);
            for settings in [Settings::new(), halve] {
                let out = replay(&arrivals, 4 + round % 5, "/repo", &settings);
                assert_eq!(out.waits.len(), arrivals.len(), "round {round}");
                assert!(out.waits.iter().zip(&out.turnarounds).all(|(w, t)| w <= t));
            }
        }
    }

    #[test]
    fn a_measured_weight_is_the_p90_of_the_cores_kept_busy() {
        let arrivals: Vec<Arrival> = [1_000, 1_200, 1_500, 2_500, 1_100]
            .iter()
            .map(|&cpu| arrival(0, "unit", 4, 1_000, cpu))
            .collect();
        assert_eq!(
            measured_weight(&arrivals, "/repo", "unit", 8),
            Some((3, 2.5, 5))
        );
        assert_eq!(
            measured_weight(&arrivals, "/repo", "unit", 2).map(|w| w.0),
            Some(2)
        );
        assert_eq!(measured_weight(&arrivals[..4], "/repo", "unit", 8), None);
    }
}
