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
    // The scenarios: as configured, with measured weights, with the
    // agent's own changes, which are checked first, whatever the history.
    // As the config declares them: the replay caps them at the capacity,
    // as the machine counts them.
    let current: Settings = repo
        .tasks()
        .map(|t| {
            let setting = TaskSetting {
                weight: t.weight,
                min_weight: t.min_weight,
            };
            (t.name.clone(), setting)
        })
        .collect();
    let yours = if args.set.is_empty() {
        None
    } else {
        Some(with_changes(&current, &args.set)?)
    };
    if arrivals.len() < LEAST_CHECKS || ours == 0 {
        let message = format!(
            "too few checks to replay: {} on this machine, {ours} of this repository ({} at \
             least, and some of it, are needed); run checks through loadmaster for a while, then \
             again",
            arrivals.len(),
            LEAST_CHECKS
        );
        if args.json {
            let value = serde_json::json!({
                "checks": arrivals.len(),
                "of_this_repository": ours,
                "replayed": false,
                "message": message,
            });
            print(&format!("{value:#}\n"));
        } else {
            print(&format!("loadmaster tune: {message}\n"));
        }
        return Ok(0);
    }
    let mut measured = current.clone();
    let mut changes = Vec::new();
    for (task, setting) in &mut measured {
        if let Some((weight, p90, runs)) =
            tune::measured_weight(&arrivals, &workload, task, capacity)
            && weight != setting.weight.min(capacity)
        {
            // A min_weight above the new weight comes down with it.
            let least = setting.min_weight.filter(|&l| l > weight).map(|_| weight);
            changes.push(Change {
                task: task.clone(),
                from: setting.weight,
                to: weight,
                min_weight: least,
                p90,
                runs,
            });
            setting.weight = weight;
            setting.min_weight = setting.min_weight.map(|l| l.min(weight));
        }
    }
    let mut scenarios: Vec<(&str, Settings)> = vec![("current", current.clone())];
    if !changes.is_empty() {
        scenarios.push(("measured weights", measured));
    }
    if let Some(yours) = yours {
        scenarios.push(("your changes", yours));
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
            "measured": changes.iter().map(|c| serde_json::json!({
                "task": c.task, "from": c.from, "to": c.to, "min_weight": c.min_weight,
                "p90_cores": c.p90, "runs": c.runs,
            })).collect::<Vec<_>>(),
            "replayed": true,
            "assumptions": ASSUMPTIONS,
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
            .map(|c| {
                format!(
                    "`{}` {} -> {} (p90 {:.1} of {} runs)",
                    c.task, c.from, c.to, c.p90, c.runs
                )
            })
            .collect();
        out += &parts.join(", ");
        out += "\nto try them, in .loadmaster.toml:";
        for c in &changes {
            out += &format!(" [tasks.{}] weight = {}", c.task, c.to);
            if let Some(least) = c.min_weight {
                out += &format!(", min_weight = {least}");
            }
            out += ";";
        }
        out += " then compare `loadmaster report --since` the change with before.\n";
    }
    out += &format!(
        "\nA scenario, not a promise: {} The median wait of these checks was {} as recorded, {} \
         as replayed with the current settings: the closer, the better the replay fits this \
         machine.\n",
        ASSUMPTIONS.join(" "),
        d(percentile(&recorded, 0.5)),
        d(percentile(&outcomes[0].waits, 0.5)),
    );
    print(&out);
    Ok(0)
}

/// What the replay assumes, said with every result.
const ASSUMPTIONS: &[&str] = &[
    "Checks arrive when they did: an agent that waited less might have asked for its next \
     check sooner, and a retried failure is demand another setting might not have had.",
    "No CPU is busy outside the queue, and the machine has the capacity it has now.",
    "A task with a min_weight runs longer on fewer cores than it had, as its CPU work would; \
     one without runs as long as it did, whatever cores it gets (true of one that does not \
     size itself by LOADMASTER_WEIGHT); more cores shorten nothing.",
    "Over capacity is estimated from the cores each check kept busy on average, not bounded: \
     checks may peak at other times than their averages say. A setting that raises it makes \
     checks slower and can make them time out.",
    "Measure-only checks are replayed as admitted, and a resource whose limits a record lacks \
     (from 0.15 and before) as unlimited.",
    "Only this repository's tasks change; every other check keeps what it recorded.",
];

/// A measured weight that differs from the current one.
struct Change {
    task: String,
    from: u32,
    to: u32,
    /// The min_weight that comes down with it, if it was above.
    min_weight: Option<u32>,
    p90: f64,
    runs: usize,
}

/// `current` with the agent's `--set task.key=value` changes, all applied
/// before the result is checked.
fn with_changes(current: &Settings, sets: &[String]) -> Result<Settings> {
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
            "weight" => setting.weight = number()?,
            "min_weight" if value.trim() == "none" => setting.min_weight = None,
            "min_weight" => setting.min_weight = Some(number()?),
            _ => return Err(invalid(&format!("`{field}` cannot be tuned"))),
        }
    }
    for (task, setting) in &settings {
        if let Some(least) = setting.min_weight.filter(|&l| l > setting.weight) {
            return Err(Error::Usage(format!(
                "--set: `{task}` would have min_weight {least}, above its weight {}: \
                 min_weight must not be above the weight",
                setting.weight
            )));
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
