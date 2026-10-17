//! `loadmaster tune`: what other weights and min_weights of this repository's
//! tasks would have done to this machine's checks (see [`loadmaster::tune`]),
//! for an agent about to change them.

use crate::*;
use loadmaster::report::percentile;
use loadmaster::tune::{self, Arrival, Outcome, Settings, TaskSetting};

/// Fewer checks than this say too little of how the line behaves.
const LEAST_CHECKS: usize = 20;

pub(crate) fn tune(args: TuneArgs) -> Result<i32> {
    let cwd = env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
    let repo = RepoConfig::discover(&cwd)?;
    let machine = MachineConfig::load()?;
    let capacity = machine.capacity;
    // The repository's, which all its worktrees share.
    let checkout = fs::canonicalize(repo.dir()).unwrap_or_else(|_| repo.dir().to_owned());
    let workload = config::workload(&checkout);
    let path = config::state_dir()?.join(HISTORY_FILE);
    let entries = match history::read(&path) {
        Ok((entries, _)) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(Error::io(format!("cannot read {}", path.display()), e)),
    };
    let now = history::unix_ms(SystemTime::now());
    let since = args.since.map(|t| t.at(now));
    let until = args.until.map(|t| t.at(now));
    let mut workloads = config::Workloads::default();
    let arrivals: Vec<Arrival> = report::checks(&entries)
        .iter()
        .filter(|c| since.is_none_or(|s| c.queued_at >= s) && until.is_none_or(|u| c.queued_at < u))
        .filter_map(|c| Arrival::of(c, &mut workloads))
        .collect();
    let ours = arrivals.iter().filter(|a| a.workload == workload).count();
    if arrivals.len() < LEAST_CHECKS || ours == 0 {
        print(&format!(
            "loadmaster tune: too few checks to replay: {} on this machine, {ours} of this \
             repository ({} at least, and some of it, are needed); run checks through \
             loadmaster for a while, then again\n",
            arrivals.len(),
            LEAST_CHECKS
        ));
        return Ok(0);
    }

    // The scenarios: as configured, with measured weights, with the
    // agent's own changes.
    let current: Settings = repo
        .tasks()
        .map(|t| {
            let setting = TaskSetting {
                weight: t.weight.min(capacity),
                min_weight: t.min_weight.map(|l| l.min(t.weight.min(capacity))),
            };
            (t.name.clone(), setting)
        })
        .collect();
    let mut measured = current.clone();
    let mut changes = Vec::new();
    for (task, setting) in &mut measured {
        if let Some((weight, p90, runs)) =
            tune::measured_weight(&arrivals, &workload, task, capacity)
            && weight != setting.weight
        {
            changes.push((task.clone(), setting.weight, weight, p90, runs));
            setting.weight = weight;
            setting.min_weight = setting.min_weight.map(|l| l.min(weight));
        }
    }
    let mut scenarios: Vec<(&str, Settings)> = vec![("current", current.clone())];
    if !changes.is_empty() {
        scenarios.push(("measured weights", measured));
    }
    if !args.set.is_empty() {
        scenarios.push(("your changes", with_changes(&current, &args.set, capacity)?));
    }
    let outcomes: Vec<Outcome> = scenarios
        .iter()
        .map(|(_, settings)| tune::replay(&arrivals, capacity, &workload, settings))
        .collect();
    let recorded: Vec<u64> = arrivals.iter().map(|a| a.waited_ms).collect();
    let first = arrivals.iter().map(|a| a.at).min().unwrap_or(0);
    let last = arrivals.iter().map(|a| a.at).max().unwrap_or(0);

    if args.json {
        let ms = |values: &[u64], p: f64| percentile(values, p);
        let value = serde_json::json!({
            "checks": arrivals.len(),
            "of_this_repository": ours,
            "capacity": capacity,
            "from_ms": first,
            "to_ms": last,
            "recorded_wait_median_ms": ms(&recorded, 0.5),
            "scenarios": scenarios.iter().zip(&outcomes).map(|((name, settings), out)| serde_json::json!({
                "name": name,
                "settings": settings.iter().map(|(task, s)| (task.clone(), serde_json::json!({
                    "weight": s.weight,
                    "min_weight": s.min_weight,
                }))).collect::<serde_json::Map<_, _>>(),
                "wait_ms": {"median": ms(&out.waits, 0.5), "p90": ms(&out.waits, 0.9)},
                "in_line_to_end_ms": {"median": ms(&out.turnarounds, 0.5), "p90": ms(&out.turnarounds, 0.9)},
                "over_capacity": out.over,
                "tasks": out.tasks.iter().map(|(task, (waits, ends))| (task.clone(), serde_json::json!({
                    "wait_median_ms": ms(waits, 0.5),
                    "in_line_to_end_median_ms": ms(ends, 0.5),
                }))).collect::<serde_json::Map<_, _>>(),
            })).collect::<Vec<_>>(),
            "measured": changes.iter().map(|(task, from, to, p90, runs)| serde_json::json!({
                "task": task, "from": from, "to": to, "p90_cores": p90, "runs": runs,
            })).collect::<Vec<_>>(),
        });
        print(&format!("{value:#}\n"));
        return Ok(0);
    }

    let d = |ms: Option<u64>| {
        ms.map_or("-".to_owned(), |ms| {
            format_duration(Duration::from_millis(ms))
        })
    };
    let mut out = format!(
        "loadmaster tune: what other settings of {} would have done to the {} checks of this \
         machine that ran from {} to {} ({ours} of them of this repository, from any of its \
         worktrees), replayed through the admission rule itself on {capacity} cores.\n\n",
        tilde(repo.path()),
        arrivals.len(),
        report::format_time(first),
        report::format_time(last),
    );
    let mut rows = vec![
        [
            "setting",
            "wait med",
            "wait p90",
            "in line to end med",
            "p90",
            "over capacity",
        ]
        .map(String::from),
    ];
    for ((name, _), o) in scenarios.iter().zip(&outcomes) {
        rows.push([
            (*name).to_owned(),
            d(percentile(&o.waits, 0.5)),
            d(percentile(&o.waits, 0.9)),
            d(percentile(&o.turnarounds, 0.5)),
            d(percentile(&o.turnarounds, 0.9)),
            format!("{:.1}%", 100.0 * o.over),
        ]);
    }
    out += &table(&rows);
    out += "\nthis repository, by task (wait median / in line to end median):\n";
    let mut rows = vec![
        std::iter::once("task".to_owned())
            .chain(scenarios.iter().map(|(n, _)| (*n).to_owned()))
            .collect::<Vec<_>>(),
    ];
    let tasks: std::collections::BTreeSet<&String> =
        outcomes.iter().flat_map(|o| o.tasks.keys()).collect();
    for task in tasks {
        let mut row = vec![task.clone()];
        for o in &outcomes {
            row.push(o.tasks.get(task).map_or("-".to_owned(), |(w, t)| {
                format!("{} / {}", d(percentile(w, 0.5)), d(percentile(t, 0.5)))
            }));
        }
        rows.push(row);
    }
    out += &table(&rows);
    if changes.is_empty() {
        out += &format!(
            "\nmeasured weights: every task with {} passed runs or more already has the weight \
             its runs kept busy\n",
            tune::LEAST_RUNS
        );
    } else {
        out += "\nmeasured weights (each the 90th percentile of the cores its passed runs kept \
                busy, rounded up): ";
        let parts: Vec<String> = changes
            .iter()
            .map(|(task, from, to, p90, runs)| {
                format!("`{task}` {from} -> {to} (p90 {p90:.1} of {runs} runs)")
            })
            .collect();
        out += &parts.join(", ");
        out += "\nto try them, in .loadmaster.toml:";
        for (task, _, to, _, _) in &changes {
            out += &format!(" [tasks.{task}] weight = {to};");
        }
        out += " then compare `loadmaster report --since` the change with before.\n";
    }
    out += &format!(
        "\nA scenario, not a promise: checks arrive when they did (an agent that waited less \
         might have asked sooner), no CPU is busy outside the queue, a task with min_weight runs \
         longer on fewer cores as its CPU work would, others as long as they ran, and more cores \
         shorten nothing. Over capacity is from the cores each check kept busy on average: a \
         lower bound; a setting that raises it makes checks slower and can make them time out. \
         Only this repository's tasks change; every other check keeps what it recorded. The \
         median wait of these checks was {} as recorded, {} as replayed with the current \
         settings: the closer, the better the replay fits this machine.\n",
        d(percentile(&recorded, 0.5)),
        d(percentile(&outcomes[0].waits, 0.5)),
    );
    print(&out);
    Ok(0)
}

/// `current` with the agent's `--set task.key=value` changes.
fn with_changes(current: &Settings, sets: &[String], capacity: u32) -> Result<Settings> {
    let mut settings = current.clone();
    for set in sets {
        let invalid = |why: &str| {
            Error::Usage(format!(
                "--set {set}: {why}; expected TASK.weight=N or TASK.min_weight=N (or `none`)"
            ))
        };
        let (key, value) = set.split_once('=').ok_or_else(|| invalid("no `=`"))?;
        let (task, field) = key.rsplit_once('.').ok_or_else(|| invalid("no `.`"))?;
        let setting = settings
            .get_mut(task)
            .ok_or_else(|| invalid(&format!("no task `{task}` in .loadmaster.toml")))?;
        let number = || -> Result<u32> {
            value
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|&n| n >= 1)
                .ok_or_else(|| invalid("the value must be a whole number, 1 or more"))
        };
        match field {
            "weight" => setting.weight = number()?.min(capacity),
            "min_weight" if value.trim() == "none" => setting.min_weight = None,
            "min_weight" => setting.min_weight = Some(number()?),
            _ => return Err(invalid(&format!("`{field}` cannot be tuned"))),
        }
        if setting.min_weight.is_some_and(|l| l > setting.weight) {
            return Err(invalid("min_weight must not be above the weight"));
        }
    }
    Ok(settings)
}

/// Columns padded to their widest cell.
fn table(rows: &[impl AsRef<[String]>]) -> String {
    let cols = rows.iter().map(|r| r.as_ref().len()).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.as_ref().get(c))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let cells: Vec<String> = row
            .as_ref()
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}"))
            .collect();
        out += cells.join("  ").trim_end();
        out += "\n";
    }
    out
}
