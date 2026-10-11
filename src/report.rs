//! `loadmaster report`: what the history log says about the checks that ran,
//! so a user can tell whether admission helps on their machine.
//!
//! Runs are grouped by machine setting (measure-only or admission, and the
//! capacity), so a measure-only baseline can be compared with admission.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use crate::history::{Entry, Outcome};
use crate::queue::PARK_TTL;

/// How long after giving up a re-run can still have resumed the place: the
/// parked job's lifetime, plus slack for a busy database (parking can wait
/// for it) and clocks.
const RESUME_WINDOW: Duration = PARK_TTL.saturating_add(Duration::from_secs(120));

/// One request for a check, from first getting in line to its end. Re-runs
/// after `--max-wait` resume the same job, so they are joined into one.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub task: String,
    /// The caller's directory (lossy).
    pub dir: String,
    pub weight: u32,
    pub capacity: Option<u32>,
    pub measure_only: bool,
    /// When it first got in line, in Unix milliseconds.
    pub queued_at: u64,
    /// From first getting in line until the task started (or, for a check
    /// that never ran, until it ended).
    pub wait_ms: u64,
    pub run_ms: Option<u64>,
    pub cpu_ms: Option<u64>,
    pub load: Option<f64>,
    pub result: CheckResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    Passed,
    /// The task exited with a non-zero code.
    Failed,
    /// Ended by a signal while running or waiting (usually a timeout), or
    /// its loadmaster process was killed before it could log the result.
    Killed,
    /// Gave up waiting and was not re-run, or could not be started.
    NotRun,
}

impl Check {
    fn end(&self) -> u64 {
        self.queued_at + self.wait_ms + self.run_ms.unwrap_or(0)
    }
}

/// Joins history entries into checks, roughly in the order they ended.
pub fn checks(entries: &[Entry]) -> Vec<Check> {
    // Checks that gave up, by job ID: their first and latest attempt.
    let mut parked: HashMap<i64, (&Entry, &Entry)> = HashMap::new();
    let mut checks = Vec::new();
    for entry in entries {
        let mut first = entry;
        if let Some((earlier, latest)) = parked.remove(&entry.job) {
            if resumes(latest, entry) {
                first = earlier;
            } else {
                // Not a re-run (the job ID was reused after the database was
                // recreated): the earlier place was abandoned.
                checks.push(join(earlier, latest));
            }
        }
        if entry.outcome == Outcome::GaveUp {
            parked.insert(entry.job, (first, entry));
        } else {
            checks.push(join(first, entry));
        }
    }
    // Places that were given up and never resumed.
    let mut abandoned: Vec<(&Entry, &Entry)> = parked.into_values().collect();
    abandoned.sort_by_key(|(first, _)| first.queued_at);
    checks.extend(abandoned.into_iter().map(|(first, last)| join(first, last)));
    checks
}

/// Whether `next` can be a re-run that resumed the place `gave_up` kept: the
/// same resume key as the queue's, within the place's lifetime.
fn resumes(gave_up: &Entry, next: &Entry) -> bool {
    let parked_at = gave_up.queued_at + gave_up.wait_ms;
    // A `lost` record carries the time the job first got in line.
    let after = next.outcome == Outcome::Lost || next.queued_at >= gave_up.queued_at;
    (&next.dir, &next.task, &next.args) == (&gave_up.dir, &gave_up.task, &gave_up.args)
        && after
        && next.queued_at <= parked_at.saturating_add(RESUME_WINDOW.as_millis() as u64)
}

fn join(first: &Entry, last: &Entry) -> Check {
    let killed = match last.signal {
        Some(signal) => signal.is_some(),
        // Records from before signals were logged: judge by the exit code.
        None => matches!(last.exit_code, Some(129..=192)),
    };
    let result = match last.outcome {
        Outcome::Exited if killed => CheckResult::Killed,
        Outcome::Exited if last.exit_code == Some(0) => CheckResult::Passed,
        Outcome::Exited => CheckResult::Failed,
        Outcome::Interrupted | Outcome::Lost => CheckResult::Killed,
        Outcome::GaveUp | Outcome::SpawnFailed | Outcome::Error => CheckResult::NotRun,
    };
    Check {
        task: last.task.clone(),
        dir: last.dir.clone(),
        weight: last.weight,
        capacity: last.capacity,
        measure_only: last.measure_only,
        queued_at: first.queued_at,
        wait_ms: (last.queued_at + last.wait_ms).saturating_sub(first.queued_at),
        run_ms: last.run_ms,
        cpu_ms: last.cpu_ms,
        load: last.load,
        result,
    }
}

/// How the weights of checks running at once stacked up over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stacking {
    /// Time during which at least one check was running.
    pub busy_ms: u64,
    /// The part of `busy_ms` in which their weights exceeded the capacity.
    pub over_ms: u64,
    /// The largest total weight at any moment.
    pub peak: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Overlap {
    /// As if each check had started the moment it got in line. Runs under
    /// contention would have taken longer, so this is an underestimate.
    pub demand: Stacking,
    /// As the checks really ran.
    pub in_use: Stacking,
}

/// Replays the checks whose run time is known, twice: as demanded and as run.
/// The time a task's leftover processes kept its slot is not known, so it is
/// not counted.
pub fn overlap(checks: &[Check], capacity: u32) -> Overlap {
    let ran: Vec<(&Check, u64)> = checks.iter().filter_map(|c| Some((c, c.run_ms?))).collect();
    let demand = ran
        .iter()
        .map(|&(c, run)| (c.queued_at, c.queued_at + run, c.weight));
    let in_use = ran.iter().map(|&(c, run)| {
        let start = c.queued_at + c.wait_ms;
        (start, start + run, c.weight)
    });
    Overlap {
        demand: stacking(demand, capacity),
        in_use: stacking(in_use, capacity),
    }
}

/// Sweeps half-open intervals `(start, end, weight)`.
fn stacking(intervals: impl Iterator<Item = (u64, u64, u32)>, capacity: u32) -> Stacking {
    let mut changes: BTreeMap<u64, i64> = BTreeMap::new();
    for (start, end, weight) in intervals.filter(|&(start, end, _)| end > start) {
        *changes.entry(start).or_default() += i64::from(weight);
        *changes.entry(end).or_default() -= i64::from(weight);
    }
    let mut result = Stacking::default();
    let mut level = 0u64;
    let mut since = 0u64;
    for (at, delta) in changes {
        if level > 0 {
            result.busy_ms += at - since;
        }
        if level > u64::from(capacity) {
            result.over_ms += at - since;
        }
        level = level.saturating_add_signed(delta);
        result.peak = result.peak.max(level);
        since = at;
    }
    result
}

/// Nearest-rank percentile of an unsorted sample.
fn percentile<T: Copy + PartialOrd>(values: &[T], p: f64) -> Option<T> {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.max(1) - 1).copied()
}

pub struct Options<'a> {
    pub source: &'a Path,
    /// Only checks that got in line at or after this time (Unix ms).
    pub since: Option<u64>,
    /// Only checks run from a directory whose path contains this.
    pub dir: Option<&'a str>,
    /// Lines of the log that could not be read.
    pub unreadable: usize,
    pub cpus: Option<usize>,
}

/// The whole report, as printed by `loadmaster report`.
pub fn render(entries: &[Entry], options: &Options) -> String {
    let all = checks(entries);
    let checks: Vec<Check> = all
        .into_iter()
        .filter(|c| options.since.is_none_or(|since| c.queued_at >= since))
        .filter(|c| options.dir.is_none_or(|dir| c.dir.contains(dir)))
        .collect();
    let mut out = format!("history: {}\n", options.source.display());
    if let Some(dir) = options.dir {
        out += &format!("only checks run from directories containing `{dir}`\n");
    }
    if options.unreadable > 0 {
        out += &format!("skipped {} unreadable line(s)\n", options.unreadable);
    }
    if checks.is_empty() {
        out += "no checks recorded yet\n";
        return out;
    }

    // Measure-only first: it is the baseline to compare against.
    let mut sections: BTreeMap<(bool, Option<u32>), Vec<Check>> = BTreeMap::new();
    for check in checks {
        sections
            .entry((!check.measure_only, check.capacity))
            .or_default()
            .push(check);
    }
    for ((admission, capacity), checks) in &sections {
        out += "\n";
        out += &section(!admission, *capacity, checks, options.cpus);
    }
    out += "\nA check is one request, re-runs after exit code 75 included. KILLED: ended by a \
            signal\n(usually a timeout) or loadmaster was killed. CORES: CPU time / run time. \
            RUN: passed checks.\nWAIT: from first getting in line to starting (or to the end, \
            if it never started).\n";
    out
}

fn section(
    measure_only: bool,
    capacity: Option<u32>,
    checks: &[Check],
    cpus: Option<usize>,
) -> String {
    let mode = if measure_only {
        "measure-only"
    } else {
        "admission"
    };
    let capacity_text = capacity.map_or("capacity not recorded".to_owned(), |c| {
        format!("capacity {c}")
    });
    let first = checks.iter().map(|c| c.queued_at).min().unwrap_or(0);
    let last = checks.iter().map(Check::end).max().unwrap_or(0);
    let mut out = format!(
        "== {mode} ({capacity_text}): {} check(s), {} to {}\n",
        checks.len(),
        format_time(first),
        format_time(last)
    );
    if let Some(capacity) = capacity {
        let o = overlap(checks, capacity);
        let share = |s: Stacking| 100.0 * s.over_ms as f64 / s.busy_ms.max(1) as f64;
        if measure_only && o.in_use.busy_ms > 0 {
            out += &format!(
                "weight in use: above capacity {:.0}% of the time checks ran, peak {}\n",
                share(o.in_use),
                o.in_use.peak
            );
        } else if o.demand.busy_ms > 0 {
            out += &format!(
                "demand: above capacity {:.0}% of the time checks ran, peak {} \
                 (held back by loadmaster; in use peaked at {})\n",
                share(o.demand),
                o.demand.peak,
                o.in_use.peak
            );
        }
    }
    let loads: Vec<f64> = checks.iter().filter_map(|c| c.load).collect();
    if let (Some(median), Some(p90)) = (percentile(&loads, 0.5), percentile(&loads, 0.9)) {
        let cpus = cpus.map_or(String::new(), |n| format!(" ({n} CPUs)"));
        out += &format!("load average when tasks ended: median {median:.1}, p90 {p90:.1}{cpus}\n");
    }
    out += "\n";
    out += &task_table(checks);
    out
}

fn task_table(checks: &[Check]) -> String {
    let mut by_task: BTreeMap<&str, Vec<&Check>> = BTreeMap::new();
    for check in checks {
        by_task.entry(&check.task).or_default().push(check);
    }
    let header = [
        "TASK", "WEIGHT", "CORES", "CHECKS", "PASSED", "FAILED", "KILLED", "NOT RUN", "RUN MED",
        "RUN P90", "WAIT MED", "WAIT P90", "WAIT MAX",
    ];
    let mut rows = vec![header.map(String::from).to_vec()];
    for (task, checks) in by_task {
        let count = |result| {
            checks
                .iter()
                .filter(|c| c.result == result)
                .count()
                .to_string()
        };
        let passed = || checks.iter().filter(|c| c.result == CheckResult::Passed);
        let runs: Vec<u64> = passed().filter_map(|c| c.run_ms).collect();
        let cores: Vec<f64> = passed()
            .filter_map(|c| Some((c.cpu_ms?, c.run_ms.filter(|&run| run > 0)?)))
            .map(|(cpu, run)| cpu as f64 / run as f64)
            .collect();
        let weights = checks.iter().map(|c| c.weight);
        let (low, high) = (weights.clone().min(), weights.max());
        let waits: Vec<u64> = checks.iter().map(|c| c.wait_ms).collect();
        let ms = |v: Option<u64>| {
            v.map_or("-".to_owned(), |ms| {
                format_duration(Duration::from_millis(ms))
            })
        };
        rows.push(vec![
            task.to_owned(),
            match (low, high) {
                (Some(low), Some(high)) if low < high => format!("{low}-{high}"),
                (_, high) => high.map_or("-".to_owned(), |w| w.to_string()),
            },
            percentile(&cores, 0.5).map_or("-".to_owned(), |c| format!("{c:.1}")),
            checks.len().to_string(),
            count(CheckResult::Passed),
            count(CheckResult::Failed),
            count(CheckResult::Killed),
            count(CheckResult::NotRun),
            ms(percentile(&runs, 0.5)),
            ms(percentile(&runs, 0.9)),
            ms(percentile(&waits, 0.5)),
            ms(percentile(&waits, 0.9)),
            ms(waits.iter().copied().max()),
        ]);
    }
    let widths: Vec<usize> = (0..header.len())
        .map(|col| {
            rows.iter()
                .map(|r| r[col].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in &rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(col, (cell, width))| {
                if col == 0 {
                    format!("{cell:<width$}")
                } else {
                    format!("{cell:>width$}")
                }
            })
            .collect();
        out += &cells.join("  ");
        out += "\n";
    }
    out
}

/// Parses `--since`: a number followed by `m`, `h`, `d` or `w`.
pub fn parse_since(text: &str) -> Result<Duration, String> {
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let unit_secs = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => {
            return Err(format!(
                "expected a number followed by m, h, d or w, like 7d; got `{text}`"
            ));
        }
    };
    let number: u64 = number.parse().map_err(|_| {
        format!("expected a number followed by m, h, d or w, like 7d; got `{text}`")
    })?;
    Ok(Duration::from_secs(number.saturating_mul(unit_secs)))
}

/// A short human-readable duration: `0.4s`, `12s`, `3m05s`, `2h10m`.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..10 => format!("{:.1}s", d.as_secs_f64()),
        10..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// Unix milliseconds as local time, `2026-10-11 09:12`.
fn format_time(unix_ms: u64) -> String {
    // libc deprecates `time_t` on musl ahead of a change in its size; this
    // works with either size.
    #[allow(deprecated)]
    let secs = libc::time_t::try_from(unix_ms / 1000).unwrap_or(libc::time_t::MAX);
    // SAFETY: an all-zero `tm` is a valid value to be overwritten.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the duration of the call.
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return format!("{unix_ms} ms");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(job: i64, task: &str, outcome: Outcome, queued_at: u64, wait_ms: u64) -> Entry {
        Entry {
            job,
            task: task.to_owned(),
            weight: 4,
            dir: "/src/app-wt1".to_owned(),
            args: Vec::new(),
            capacity: Some(8),
            measure_only: false,
            outcome,
            queued_at,
            wait_ms,
            run_ms: None,
            cpu_ms: None,
            load: None,
            signal: Some(None),
            exit_code: None,
        }
    }

    fn exited(job: i64, queued_at: u64, wait_ms: u64, run_ms: u64, code: i32) -> Entry {
        Entry {
            run_ms: Some(run_ms),
            exit_code: Some(code),
            ..entry(job, "test", Outcome::Exited, queued_at, wait_ms)
        }
    }

    fn options<'a>() -> Options<'a> {
        Options {
            source: Path::new("/state/history.jsonl"),
            since: None,
            dir: None,
            unreadable: 0,
            cpus: None,
        }
    }

    #[test]
    fn rerun_after_giving_up_joins_the_first_attempt() {
        let entries = [
            entry(1, "test", Outcome::GaveUp, 1_000, 600),
            exited(2, 1_200, 0, 50, 0),
            // Re-run 400 ms after giving up, starts 300 ms later.
            exited(1, 2_000, 300, 500, 0),
        ];
        let checks = checks(&entries);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[1].queued_at, 1_000);
        assert_eq!(checks[1].wait_ms, 1_300);
        assert_eq!(checks[1].run_ms, Some(500));
        assert_eq!(checks[1].result, CheckResult::Passed);
    }

    #[test]
    fn an_abandoned_check_ends_with_its_last_attempt() {
        let entries = [
            entry(1, "test", Outcome::GaveUp, 0, 60_000),
            entry(1, "test", Outcome::GaveUp, 100_000, 60_000),
        ];
        let checks = checks(&entries);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].wait_ms, 160_000);
        assert_eq!(checks[0].result, CheckResult::NotRun);
    }

    #[test]
    fn a_lost_rerun_joins_its_earlier_attempts() {
        // Gave up twice, then the resumed run's loadmaster was killed; the
        // lost record carries the time the job first got in line.
        let entries = [
            entry(1, "test", Outcome::GaveUp, 0, 60_000),
            entry(1, "test", Outcome::GaveUp, 100_000, 60_000),
            entry(1, "test", Outcome::Lost, 0, 250_000),
        ];
        let checks = checks(&entries);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].wait_ms, 250_000);
        assert_eq!(checks[0].result, CheckResult::Killed);
    }

    #[test]
    fn only_the_same_command_from_the_same_directory_resumes() {
        let elsewhere = Entry {
            dir: "/src/other".to_owned(),
            ..exited(1, 2_000, 0, 10, 0)
        };
        let other_args = Entry {
            args: vec!["b".to_owned()],
            ..exited(1, 2_000, 0, 10, 0)
        };
        for next in [elsewhere, other_args] {
            let checks = checks(&[entry(1, "test", Outcome::GaveUp, 0, 1_000), next]);
            assert_eq!(checks.len(), 2);
        }
    }

    #[test]
    fn a_reused_job_id_is_not_taken_for_a_rerun() {
        // Gave up, never resumed; the database was recreated and job 1 is new.
        let day = 86_400_000;
        let checks = checks(&[
            entry(1, "test", Outcome::GaveUp, 0, 1_000),
            exited(1, day, 0, 10, 0),
            exited(1, 2 * day, 0, 10, 1),
        ]);
        let summary: Vec<(u64, CheckResult)> =
            checks.iter().map(|c| (c.wait_ms, c.result)).collect();
        use CheckResult::*;
        assert_eq!(summary, [(1_000, NotRun), (0, Passed), (0, Failed)]);
    }

    #[test]
    fn results_are_classified() {
        let by_signal = Entry {
            signal: Some(Some(15)),
            ..exited(3, 0, 0, 10, 143)
        };
        let older_record = Entry {
            signal: None,
            ..exited(4, 0, 0, 10, 143)
        };
        let entries = [
            exited(1, 0, 0, 10, 0),
            exited(2, 0, 0, 10, 1),
            by_signal,
            older_record,
            // A task may exit with 143 on its own.
            exited(5, 0, 0, 10, 143),
            entry(6, "test", Outcome::Interrupted, 0, 10),
            entry(7, "test", Outcome::Lost, 0, 10),
            entry(8, "test", Outcome::GaveUp, 0, 10),
            entry(9, "test", Outcome::SpawnFailed, 0, 0),
        ];
        let results: Vec<CheckResult> = checks(&entries).iter().map(|c| c.result).collect();
        use CheckResult::*;
        assert_eq!(
            results,
            [
                Passed, Failed, Killed, Killed, Failed, Killed, Killed, NotRun, NotRun
            ]
        );
    }

    #[test]
    fn overlap_compares_demand_with_what_ran() {
        // Three weight-4 checks asked at once with capacity 8: one waited.
        let checks = checks(&[
            exited(1, 0, 0, 1_000, 0),
            exited(2, 0, 0, 1_000, 0),
            exited(3, 0, 1_000, 1_000, 0),
        ]);
        let o = overlap(&checks, 8);
        assert_eq!(
            o.demand,
            Stacking {
                busy_ms: 1_000,
                over_ms: 1_000,
                peak: 12
            }
        );
        assert_eq!(
            o.in_use,
            Stacking {
                busy_ms: 2_000,
                over_ms: 0,
                peak: 8
            }
        );
    }

    #[test]
    fn measure_only_reports_what_really_ran_at_once() {
        // Got in line apart, but started together.
        let checks = [
            Entry {
                measure_only: true,
                ..exited(1, 0, 200, 80, 0)
            },
            Entry {
                measure_only: true,
                ..exited(2, 100, 100, 80, 0)
            },
        ];
        let text = render(&checks, &options());
        assert!(
            text.contains("weight in use: above capacity 0% of the time checks ran, peak 8"),
            "{text}"
        );
        let o = overlap(&self::checks(&checks), 4);
        assert_eq!(o.in_use.over_ms, 80);
        assert_eq!(o.demand.peak, 4);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let values = [5, 1, 4, 2, 3];
        assert_eq!(percentile(&values, 0.5), Some(3));
        assert_eq!(percentile(&values, 0.9), Some(5));
        assert_eq!(percentile::<u64>(&[], 0.5), None);
    }

    #[test]
    fn render_separates_measure_only_from_admission() {
        let baseline = Entry {
            measure_only: true,
            load: Some(20.0),
            ..exited(1, 0, 0, 30_000, 1)
        };
        let admitted = Entry {
            cpu_ms: Some(38_000),
            ..exited(2, 100_000, 5_000, 10_000, 0)
        };
        let text = render(
            &[baseline, admitted],
            &Options {
                unreadable: 2,
                cpus: Some(14),
                ..options()
            },
        );
        let measure = text
            .find("== measure-only (capacity 8): 1 check(s)")
            .unwrap();
        let admission = text.find("== admission (capacity 8): 1 check(s)").unwrap();
        assert!(measure < admission, "{text}");
        assert!(text.contains("skipped 2 unreadable line(s)"), "{text}");
        assert!(text.contains("median 20.0, p90 20.0 (14 CPUs)"), "{text}");
        assert!(text.contains("demand: above capacity 0%"), "{text}");
        let row = text
            .lines()
            .filter(|l| l.starts_with("test "))
            .nth(1)
            .unwrap();
        let cells: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(
            cells,
            [
                "test", "4", "3.8", "1", "1", "0", "0", "0", "10s", "10s", "5.0s", "5.0s", "5.0s"
            ]
        );
    }

    #[test]
    fn waits_of_checks_that_never_started_count() {
        let text = render(
            &[
                exited(1, 0, 0, 10, 0),
                entry(2, "test", Outcome::Interrupted, 0, 60_000),
            ],
            &options(),
        );
        let row = text.lines().find(|l| l.starts_with("test ")).unwrap();
        assert!(row.ends_with("1m00s"), "{row}");
    }

    #[test]
    fn filters_by_time_and_directory() {
        let other = Entry {
            dir: "/src/other".to_owned(),
            ..exited(2, 9_000, 0, 10, 0)
        };
        let entries = [exited(1, 1_000, 0, 10, 0), other];
        let since = |since| Options {
            since: Some(since),
            ..options()
        };
        assert!(render(&entries, &since(5_000)).contains("1 check(s)"));
        assert!(render(&entries, &since(10_000)).contains("no checks recorded yet"));
        let text = render(
            &entries,
            &Options {
                dir: Some("app-wt"),
                ..options()
            },
        );
        assert!(text.contains("directories containing `app-wt`"), "{text}");
        assert!(text.contains("1 check(s)"), "{text}");
    }

    #[test]
    fn since_accepts_minutes_to_weeks() {
        assert_eq!(parse_since("90m"), Ok(Duration::from_secs(5_400)));
        assert_eq!(parse_since("7d"), Ok(Duration::from_secs(604_800)));
        assert_eq!(parse_since("2w"), Ok(Duration::from_secs(1_209_600)));
        for bad in ["", "7", "d", "7s", "-1d", "1.5h"] {
            assert!(parse_since(bad).is_err(), "{bad}");
        }
    }
}
