//! `loadmaster status`.

use crate::cli::run::name_group;
use crate::*;

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
        });
        print(&format!("{value:#}\n"));
        return Ok(0);
    }
    let mut out = format!(
        "{}/{} in use (capacity {})\n",
        snapshot.used(),
        machine.capacity,
        machine.capacity_source
    );
    for (name, used, capacity) in resources_in_use(&snapshot.jobs, &machine) {
        out += &match capacity {
            Some(capacity) => format!("{name}: {used}/{capacity} in use\n"),
            None => format!("{name}: {used} in use (no machine-wide limit)\n"),
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
        out += "\n";
        out += &jobs_table(&snapshot.jobs);
        out += &why_jobs_wait(&snapshot.jobs, &explained);
    }
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

/// One line per waiting job: what the admission rule says it waits for.
fn why_jobs_wait(jobs: &[Job], explained: &[(JobId, Admission, Option<u32>)]) -> String {
    let lines: Vec<String> = explained
        .iter()
        .filter_map(|(id, admission, cores)| {
            let job = jobs.iter().find(|job| job.id == *id)?;
            if job.state != JobState::Waiting || job.measure_only {
                return None;
            }
            let who = format!("#{id} `{}`", job.task);
            Some(match (admission, cores) {
                (Admission::Admitted, Some(cores)) if *cores < job.weight => {
                    format!(
                        "  {who} is about to start, with {cores} of its {} cores\n",
                        job.weight
                    )
                }
                (Admission::Admitted, _) => format!("  {who} is about to start\n"),
                (Admission::Waiting(wait), _) => {
                    format!("  {who} waits for {}\n", wait.waits_for())
                }
            })
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!("\nwhy jobs wait:\n{}", lines.concat())
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

fn jobs_table(jobs: &[Job]) -> String {
    let now = SystemTime::now();
    let since = |t: Option<SystemTime>| {
        t.and_then(|t| now.duration_since(t).ok())
            .map_or_else(|| "-".to_owned(), format_duration)
    };
    let mut rows = vec![
        [
            "ID", "STATE", "TASK", "WEIGHT", "USES", "PID", "PGID", "FOR", "DIR",
        ]
        .map(String::from),
    ];
    let (mut orphaned, mut parked) = (false, false);
    // Jobs whose task's command exited, and what holds their slot.
    let mut lingering = Vec::new();
    for job in jobs {
        let (state, at) = shown_state(job);
        match state {
            "lingering" => {
                if let Some(group) = job.child {
                    lingering.push(format!(
                        "  #{} `{}`: {}\n",
                        job.id,
                        job.task,
                        name_group(&group)
                    ));
                }
            }
            "orphaned" => orphaned = true,
            "parked" => parked = true,
            _ => {}
        }
        rows.push([
            job.id.to_string(),
            state.to_owned(),
            job.task.clone(),
            // "6 of 8": it started with fewer cores, for CPU busy outside.
            match job.granted.filter(|&granted| granted < job.weight) {
                Some(granted) => format!("{granted} of {}", job.weight),
                None if job.state != JobState::Running && job.least() < job.weight => {
                    format!("{} (from {})", job.weight, job.least())
                }
                None => job.weight.to_string(),
            },
            if job.uses.is_empty() {
                "-".to_owned()
            } else {
                job.uses
                    .iter()
                    .map(|u| format!("{}={}", u.name, u.amount))
                    .collect::<Vec<_>>()
                    .join(",")
            },
            job.owner.map_or("-".to_owned(), |o| o.pid.to_string()),
            job.child.map_or("-".to_owned(), |c| c.pgid.to_string()),
            since(at),
            job.dir.display().to_string(),
        ]);
    }
    let widths: Vec<usize> = (0..rows[0].len())
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
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        out += cells.join("  ").trim_end();
        out += "\n";
    }
    if orphaned {
        out += "\norphaned: loadmaster is gone but the task is still running; the slot is held \
                until its process group exits\n";
    }
    if !lingering.is_empty() {
        out += "\nlingering: the task's command exited, but processes it started in its process \
                group still run, and hold the slot until they exit (`loadmaster cancel ID` stops \
                them):\n";
        out += &lingering.concat();
    }
    if parked {
        out += &format!(
            "\nparked: gave up waiting (--max-wait); keeps its place for {}s for a re-run \
             of the same command from the same directory\n",
            PARK_TTL.as_secs()
        );
    }
    out
}
