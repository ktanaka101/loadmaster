//! `loadmaster status`.

use crate::cli::run::name_group;
use crate::*;
use loadmaster::report::{Check, CheckResult};
use loadmaster::usual::{self, Usual};

pub(crate) fn status(args: StatusArgs) -> Result<i32> {
    let machine = MachineConfig::load()?;
    let (state_dir, queue) = open_queue()?;
    let snapshot = queue.snapshot()?;
    // Each job adapts as the machine config it got in line with said.
    let adapting = machine.adapt_to_outside_cpu && !machine.measure_only;
    let outside = if adapting
        || snapshot
            .jobs
            .iter()
            .any(|job| job.state == JobState::Waiting && job.adapt_cpu)
    {
        queue.outside(&snapshot.jobs)?
    } else {
        None
    };
    // Until the queue has moved to this version's protocol, the jobs in it
    // are those of 0.12 to 0.14, which their own loadmaster admits by its
    // rule.
    let moving = snapshot.moving;
    let ours = snapshot.protocol == queue::PROTOCOL;
    let explained = if ours {
        queue::explain_with_cores(&snapshot.jobs, outside)
    } else {
        Vec::new()
    };
    // The checkout this is run from, if any: its jobs are marked, and what
    // became of its latest checks is told.
    let here = env::current_dir()
        .ok()
        .and_then(|cwd| RepoConfig::find(&cwd))
        .and_then(|config| config.parent().and_then(|dir| fs::canonicalize(dir).ok()));
    // One read of the end of the history, for both.
    let entries = history::read_recent(&state_dir.join(HISTORY_FILE), usual::RECENT_BYTES)
        .unwrap_or_default();
    let usual = Usual::of(&entries);
    let recent = here
        .as_deref()
        .map_or_else(Vec::new, |here| recent_checks(&entries, here));
    if args.json {
        let others = queue::jobs_in_other_queues(&state_dir);
        let value = json::status(&json::Status {
            machine: &machine,
            snapshot: &snapshot,
            outside,
            resources: &resources_in_use(&snapshot.jobs, &machine),
            others: &others,
            explained: &explained,
            moving,
            here: here.as_deref(),
            usual: &usual,
            recent: &recent,
        });
        print(&format!("{value:#}\n"));
        return Ok(0);
    }
    let mut out = format!(
        "capacity {} ({}): {} in use\n",
        machine.capacity,
        machine.capacity_source,
        snapshot.used(),
    );
    for (name, used, capacity) in resources_in_use(&snapshot.jobs, &machine) {
        out += &match capacity {
            Some(capacity) => format!("`{name}`: {used} of {capacity} in use\n"),
            None => format!("`{name}`: {used} in use (no machine-wide limit)\n"),
        };
    }
    out += &load_line();
    if let Some(outside) = outside.filter(|_| adapting) {
        out += &format!(
            "CPU busy beyond what running jobs got: {:.1} of {} cores",
            outside.busy, outside.cores
        );
        let limit = outside.cpu_limit(machine.capacity);
        // Until the queue has moved, its jobs start by their own
        // loadmaster's rule, not this one.
        if limit < machine.capacity && ours {
            out += &format!(
                ", so jobs start with at most {limit} cores, or the least they may start with \
                 if that is more (their min_weight, else their weight)"
            );
        }
        // The measurement cannot tell a job that uses more than it got from
        // load outside the queue; the history can, per task.
        if snapshot
            .jobs
            .iter()
            .any(|job| job.state == JobState::Running)
        {
            out += " (load outside the queue, or jobs using more than they got: see CORES in \
                    `loadmaster report`)";
        }
        out += "\n";
    }
    if let Some(others) = other_queues(&state_dir) {
        out += &format!("{others}\n");
    }
    match moving {
        Some(left) => {
            out += &format!(
                "the queue is moving to the admission protocol of loadmaster 0.15 and later: \
                 new runs wait until the {left} job(s) of loadmaster 0.12 to 0.14 in it are \
                 done, which their own loadmaster admits; give every agent 0.15 or later\n"
            );
        }
        None if !ours => {
            out += &format!(
                "the queue keeps the admission protocol of loadmaster 0.12 to 0.14, which admits \
                 the {} job(s) in it: the first `loadmaster run` of this version closes it to \
                 new jobs, so runs of 0.12 to 0.14 fail from then on, and moves it to that of \
                 0.15 and later once they are done; give every agent 0.15 or later at once\n",
                snapshot.jobs.len()
            );
        }
        None => {}
    }
    if machine.measure_only {
        out += "measure-only: jobs start at once and are only recorded (see `loadmaster report`)\n";
    }
    if !snapshot.reaped.is_empty() {
        out += &format!(
            "reclaimed {} job(s) whose processes were gone\n",
            snapshot.reaped.len()
        );
    }
    if snapshot.jobs.is_empty() {
        out += "no jobs\n";
    } else {
        out += "\njobs, in line order:\n";
        out += &job_lines(&snapshot.jobs, &explained, here.as_deref(), &usual);
    }
    out += &recent_lines(&recent);
    print(&out);
    Ok(0)
}

/// The machine's load, which also counts work outside the queue.
fn load_line() -> String {
    let Some(load) = process::load_average() else {
        return String::new();
    };
    let Some(cores) = process::online_cpus() else {
        return format!("load average {load:.1} over the last minute\n");
    };
    let mut line = format!("load average {load:.1} over the last minute, on {cores} cores");
    if load > cores as f64 {
        line += ": more than the cores, often from work outside the queue or tasks that \
                 use more cores than their weight (see `loadmaster report`); on Linux, \
                 processes waiting for disks count too";
    }
    line + "\n"
}

/// Each machine-wide resource that any job uses: how much running jobs use,
/// and the machine config's capacity, else the one jobs recorded.
fn resources_in_use<'j>(
    jobs: &'j [Job],
    machine: &MachineConfig,
) -> Vec<(&'j str, u64, Option<u32>)> {
    let mut resources: std::collections::BTreeMap<&str, (u64, Option<u32>)> =
        std::collections::BTreeMap::new();
    for job in jobs {
        for u in &job.uses {
            let entry = resources
                .entry(&u.name)
                .or_insert((0, machine.resources.get(&u.name).copied()));
            entry.1 = entry.1.or(u.capacity);
            if job.state == JobState::Running {
                entry.0 += u64::from(u.capacity.map_or(u.amount, |c| u.amount.min(c)));
            }
        }
    }
    resources
        .into_iter()
        .map(|(name, (used, capacity))| (name, used, capacity))
        .collect()
}

/// A job's state as `status` shows it, and since when it has been in it: a
/// running job whose loadmaster is gone is orphaned while its task's command
/// runs, and lingering once that exited and processes it left in its group
/// hold the slot.
pub(crate) fn shown_state(job: &Job) -> (&'static str, Option<SystemTime>) {
    match job.state {
        JobState::Running if job.is_orphaned() => {
            // Handed over to its task's group, it got there then; when a
            // loadmaster that died left it so, nobody saw.
            let since = job.state_since.filter(|_| job.owner.is_none());
            match job.child {
                Some(group) if group.leader_exited() => ("lingering", since),
                _ => ("orphaned", since),
            }
        }
        JobState::Running => ("running", job.state_since),
        JobState::Waiting => ("waiting", job.state_since),
        JobState::Parked => ("parked", job.state_since),
    }
}

/// One line per job, in line order: its state, task, what it holds or may
/// start with, for how long (and how long it usually runs), where from, and
/// what an agent needs to know of it: why it waits, until when a parked
/// place is kept, what holds the slot of one whose loadmaster is gone.
fn job_lines(
    jobs: &[Job],
    explained: &[(JobId, Admission, Option<u32>)],
    here: Option<&Path>,
    usual: &Usual,
) -> String {
    let now = SystemTime::now();
    let mut out = String::new();
    for job in jobs {
        let (state, at) = shown_state(job);
        let mut line = format!("#{} {state} `{}`", job.id, job.task);
        line += &match job.granted.filter(|_| job.state == JobState::Running) {
            Some(granted) if granted < job.weight => {
                format!(" with {granted} of its {} cores", job.weight)
            }
            Some(granted) => format!(" with {granted} core{}", plural(granted)),
            None if job.least() < job.weight => {
                format!(" (weight {}, from {})", job.weight, job.least())
            }
            None => format!(" (weight {})", job.weight),
        };
        if !job.uses.is_empty() {
            let uses: Vec<String> = job
                .uses
                .iter()
                .map(|u| match u.amount {
                    1 => format!("`{}`", u.name),
                    n => format!("{n} of `{}`", u.name),
                })
                .collect();
            line += &format!(", using {}", uses.join(" and "));
        }
        if let Some(since) = at.and_then(|t| now.duration_since(t).ok()) {
            line += &format!(", for {}", format_duration(since));
        }
        if job.state == JobState::Running
            && let Some(usual) = usual.of_job(job)
        {
            line += &format!(" (usually {})", format_duration(usual.median));
        }
        line += &format!(", from {}", tilde(&job.dir));
        if here == Some(job.checkout.as_path()) {
            line += " (same checkout)";
        }
        let decision = explained.iter().find(|(id, _, _)| *id == job.id);
        let why = match state {
            "lingering" => job.child.map(|group| {
                format!(
                    "its command exited, but {} still runs and holds the slot; `loadmaster \
                     cancel {}` stops it",
                    name_group(&group),
                    job.id
                )
            }),
            "orphaned" => Some(
                "its loadmaster is gone; the task still runs and holds the slot until its \
                 process group exits"
                    .to_owned(),
            ),
            "parked" => {
                let left = job
                    .parked_at
                    .and_then(|at| (at + PARK_TTL).duration_since(now).ok())
                    .unwrap_or_default();
                Some(format!(
                    "gave up waiting; its place is kept {} more for a re-run of the same \
                     command from there",
                    format_duration(left)
                ))
            }
            "waiting" if job.measure_only => None,
            "waiting" => decision.map(|(_, admission, cores)| match (admission, cores) {
                (Admission::Admitted, Some(cores)) if *cores < job.weight => {
                    format!("about to start, with {cores} of its {} cores", job.weight)
                }
                (Admission::Admitted, _) => "about to start".to_owned(),
                (Admission::Waiting(wait), _) => {
                    format!(
                        "{}; waits for {}",
                        format_ahead(wait.ahead),
                        wait.waits_for()
                    )
                }
            }),
            _ => None,
        };
        if let Some(why) = why {
            line += &format!(": {why}");
        }
        out += &line;
        out += "\n";
    }
    out
}

/// The checks of the checkout `here` that ended lately, latest first: what
/// became of a run whose output an agent lost (its tool timed out, say).
fn recent_lines(recent: &[Check]) -> String {
    if recent.is_empty() {
        return String::new();
    }
    let now = history::unix_ms(SystemTime::now());
    let mut out = "\nended lately, from this checkout:\n".to_owned();
    for check in recent {
        let ago = format_duration(Duration::from_millis(now.saturating_sub(check.end())));
        let ran = check.run_ms.map_or(String::new(), |ms| {
            format!(" in {}", format_duration(Duration::from_millis(ms)))
        });
        let code = check
            .exit_code
            .map_or(String::new(), |code| format!(" (exit {code})"));
        out += &format!(
            "#{} `{}` {}{ran}{code}, {ago} ago\n",
            check.job,
            check.task,
            result_words(check.result)
        );
    }
    out
}

/// A check's result in `status --json`: one of a fixed set of values.
pub(crate) fn result_code(result: CheckResult) -> &'static str {
    match result {
        CheckResult::Passed => "passed",
        CheckResult::Failed => "failed",
        CheckResult::Killed => "killed",
        CheckResult::LeftLine => "left_line",
        CheckResult::NotRun => "not_run",
        CheckResult::Canceled => "canceled",
    }
}

/// A check's result, as `status` words it.
fn result_words(result: CheckResult) -> &'static str {
    match result {
        CheckResult::Passed => "passed",
        CheckResult::Failed => "failed",
        CheckResult::Killed => "was killed or stopped",
        CheckResult::LeftLine => "left the line without running",
        CheckResult::NotRun => "could not start",
        CheckResult::Canceled => "was canceled",
    }
}

/// How long ago a check may have ended to count as lately.
const LATELY: Duration = Duration::from_secs(30 * 60);
/// At most this many checks that ended lately are shown.
const RECENT: usize = 5;

/// The checks of `checkout` that ended lately, latest first, from the
/// latest `entries` of the history.
fn recent_checks(entries: &[history::Entry], checkout: &Path) -> Vec<Check> {
    let since = history::unix_ms(SystemTime::now() - LATELY);
    let checkout = checkout.to_string_lossy();
    let mut checks: Vec<Check> = report::checks(entries)
        .into_iter()
        .filter(|c| c.checkout == checkout && c.end() >= since)
        .collect();
    checks.sort_by_key(|c| std::cmp::Reverse(c.end()));
    checks.truncate(RECENT);
    checks
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}
