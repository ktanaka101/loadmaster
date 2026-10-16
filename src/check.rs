//! `loadmaster check`: what a repository's config means on this machine,
//! without touching the queue. Loading the configs validates them; this adds
//! what the admission rule will make of them here.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use crate::config::{MachineConfig, RepoConfig, Task};
use crate::queue::{self, Job, JobId, JobState, Use};

/// The report `loadmaster check` prints.
pub fn render(repo: &RepoConfig, machine: &MachineConfig) -> String {
    let capacity = machine.capacity;
    let mut out = format!(
        "{}: valid\ncapacity: {capacity} ({})\n",
        repo.path().display(),
        machine.capacity_source
    );
    if machine.measure_only {
        out += "measure-only: jobs start at once and are only recorded\n";
    } else if !machine.adapt_to_outside_cpu {
        out += "CPU busy beyond what running jobs got: ignored (adapt_to_outside_cpu = false)\n";
    } else if let Some(cores) = crate::process::online_cpus() {
        let spare = (cores as u64).saturating_sub(u64::from(capacity));
        out += &format!(
            "CPU busy beyond what running jobs got: lowers what jobs start with once it is more \
             than the {spare} of {cores} cores the capacity leaves free, down to their \
             min_weight (a task without one starts with its weight, once nothing runs ahead of \
             it)\n"
        );
    }

    let mut rows =
        vec![["TASK", "WEIGHT", "USES", "OK EXIT CODES", "STOP SIGNAL"].map(String::from)];
    for task in repo.tasks() {
        rows.push([
            task.name.clone(),
            {
                let weight = if task.weight > capacity {
                    format!("{} (counts as {capacity})", task.weight)
                } else {
                    task.weight.to_string()
                };
                match task
                    .min_weight
                    .filter(|&least| least < task.weight.min(capacity))
                {
                    Some(least) => format!("{weight} (from {least})"),
                    None => weight,
                }
            },
            if task.uses.is_empty() {
                "-".to_owned()
            } else {
                task.uses
                    .iter()
                    .map(|(name, amount)| format!("{name}={amount}"))
                    .collect::<Vec<_>>()
                    .join(",")
            },
            task.ok_exit_codes
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            crate::runner::signal_name(task.stop_signal),
        ]);
    }
    if rows.len() == 1 {
        out += "\nno tasks defined\n";
    } else {
        out += "\n";
        out += &table(&rows);
    }

    if !repo.resources().is_empty() {
        let mut rows = vec![["RESOURCE", "CAPACITY", "PER CHECKOUT"].map(String::from)];
        for (name, resource) in repo.resources() {
            rows.push([
                name.clone(),
                match (machine.resources.get(name), resource.capacity) {
                    (Some(c), _) => format!("{c} (machine config)"),
                    (None, Some(c)) => c.to_string(),
                    (None, None) => "no limit".to_owned(),
                },
                resource
                    .per_checkout
                    .map_or("-".to_owned(), |limit| limit.to_string()),
            ]);
        }
        out += "\n";
        out += &table(&rows);
    }

    let warnings = warnings(repo, machine);
    if !warnings.is_empty() {
        out += "\n";
        for warning in warnings {
            out += &format!("warning: {warning}\n");
        }
    }
    out
}

/// What the admission rule will make of this config on this machine that
/// its author may not expect.
pub fn warnings(repo: &RepoConfig, machine: &MachineConfig) -> Vec<String> {
    // Measure-only admits every job at once: nothing waits, nothing is
    // limited.
    if machine.measure_only {
        return Vec::new();
    }
    // With a capacity of 1, as on a small CI runner, every task runs alone
    // and saying so for each says nothing.
    let mut warnings: Vec<String> = repo
        .tasks()
        .filter(|_| machine.capacity > 1)
        .filter_map(|task| whole_machine(task, machine.capacity))
        .collect();
    warnings.extend(unlimited_resources(repo, machine));
    warnings.extend(kept_out_while_waiting(repo, machine));
    warnings
}

/// A note for a task whose weight takes this machine's whole capacity.
pub fn whole_machine(task: &Task, capacity: u32) -> Option<String> {
    let what = match task.weight.cmp(&capacity) {
        Ordering::Less => return None,
        Ordering::Equal => format!(
            "`{}` has weight {}, this machine's whole capacity",
            task.name, task.weight
        ),
        Ordering::Greater => match task.min_weight.filter(|&least| least > capacity) {
            None => format!(
                "`{}` has weight {}, above this machine's capacity of {capacity}, so it counts \
                 as {capacity}",
                task.name, task.weight
            ),
            Some(least) => format!(
                "`{}` has weight {} and min_weight {least}, above this machine's capacity of \
                 {capacity}, so both count as {capacity}",
                task.name, task.weight
            ),
        },
    };
    // Jobs behind count it at its weight while it waits, and at what it
    // started with once it runs.
    let alone = match task.min_weight.filter(|&least| least < capacity) {
        None => "it runs alone, and every job that gets in line after it waits until it has \
                 finished"
            .to_owned(),
        Some(least) => format!(
            "every job that gets in line after it waits until it has started, though it may \
             start beside other jobs with as few as {least} cores (min_weight)"
        ),
    };
    Some(format!(
        "{what}: {alone}. If the point is to keep its runs from overlapping, give it a \
         resource with capacity 1 instead"
    ))
}

/// Resources used by a task and limited nowhere on this machine.
fn unlimited_resources(repo: &RepoConfig, machine: &MachineConfig) -> Vec<String> {
    repo.resources()
        .iter()
        .filter(|(name, resource)| {
            resource.capacity.is_none()
                && resource.per_checkout.is_none()
                && !machine.resources.contains_key(*name)
                && repo.tasks().any(|task| task.uses.contains_key(*name))
        })
        .map(|(name, _)| {
            format!(
                "`{name}` has no capacity in this file or the machine config, so runs that use \
                 it are not kept apart: set one under [resources] in the machine config"
            )
        })
        .collect()
}

/// A run waiting for a resource that a long run holds starts only once the
/// long run ends, and every job behind it leaves room for that start: a task
/// that fits beside the long run but not beside the waiting run's start is
/// kept out by the waiting run. Worked out by the admission rule itself on
/// that picture. One warning per waiting task, since its weight is what can
/// be changed.
fn kept_out_while_waiting(repo: &RepoConfig, machine: &MachineConfig) -> Vec<String> {
    let capacity = machine.capacity;
    let limit_of = |name: &str| {
        machine
            .resources
            .get(name)
            .copied()
            .or(repo.resource(name)?.capacity)
    };
    let job = |id: JobId, task: &Task, state: JobState, checkout: &str| {
        let weight = task.weight.min(capacity);
        Job {
            id,
            task: task.name.clone(),
            weight,
            state,
            dir: PathBuf::from(checkout),
            args: Vec::new(),
            command: String::new(),
            owner: None,
            child: None,
            enqueued_at: UNIX_EPOCH,
            parked_at: None,
            started_at: None,
            capacity,
            measure_only: false,
            uses: task
                .uses
                .iter()
                .map(|(name, &amount)| Use {
                    name: name.clone(),
                    amount,
                    capacity: limit_of(name),
                    per_checkout: repo.resource(name).and_then(|r| r.per_checkout),
                })
                .collect(),
            checkout: PathBuf::from(checkout),
            adapt_cpu: false,
            stop_signal: libc::SIGTERM,
            granted: (state == JobState::Running).then_some(weight),
            state_since: None,
            min_weight: Some(task.min_weight.unwrap_or(weight).min(weight)),
        }
    };
    // In u64: sums of u32 amounts cannot overflow it.
    let amount = |task: &Task, name: &str| u64::from(task.uses.get(name).copied().unwrap_or(0));
    // One clause per waiting task and holder, whichever resource keeps them
    // apart.
    let mut warned: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut warnings = Vec::new();
    for (name, resource) in repo.resources() {
        let users: Vec<&Task> = repo
            .tasks()
            .filter(|task| task.uses.contains_key(name))
            .collect();
        // Machine-wide, the runs are in checkouts of their own; within a
        // checkout, the waiting run is in the holder's.
        let scopes = [
            (limit_of(name), "", "/waiter"),
            (resource.per_checkout, " in its checkout", "/holder"),
        ];
        for (limit, scope, waiter_checkout) in scopes {
            let Some(limit) = limit.map(u64::from) else {
                continue;
            };
            for &waiter in &users {
                let mut clauses = Vec::new();
                for &holder in &users {
                    let held = amount(holder, name).min(limit) + amount(waiter, name).min(limit);
                    if held <= limit || warned.contains(&(&waiter.name, &holder.name)) {
                        continue;
                    }
                    let h = job(1, holder, JobState::Running, "/holder");
                    let w = job(2, waiter, JobState::Waiting, waiter_checkout);
                    // The waiting run at its weight, at one core, or absent.
                    let starts = |task: &Task, checkout: &str, waiter: Option<u32>| {
                        let t = job(3, task, JobState::Waiting, checkout);
                        let jobs = match waiter {
                            Some(weight) => vec![
                                h.clone(),
                                Job {
                                    weight,
                                    ..w.clone()
                                },
                                t,
                            ],
                            None => vec![h.clone(), t],
                        };
                        queue::would_start(&jobs)
                    };
                    // Kept out by its weight: it starts beside the holder,
                    // and beside the waiting run's start if that took one
                    // core, but not as it is.
                    let crowded_in = |task: &Task, checkout: &str| {
                        starts(task, checkout, None)
                            && starts(task, checkout, Some(1))
                            && !starts(task, checkout, Some(w.weight))
                    };
                    let crowded: Vec<String> = repo
                        .tasks()
                        .filter(|task| crowded_in(task, "/other"))
                        .map(|task| {
                            // In the holder's checkout, a per-checkout limit
                            // may keep it out whatever the waiting run's
                            // weight.
                            let elsewhere = if crowded_in(task, "/holder") {
                                ""
                            } else {
                                ", from other checkouts"
                            };
                            format!("`{}` ({}{elsewhere})", task.name, task.weight)
                        })
                        .collect();
                    if crowded.is_empty() {
                        continue;
                    }
                    warned.insert((&waiter.name, &holder.name));
                    let holder = if holder.name == waiter.name {
                        format!("another `{}` run", holder.name)
                    } else {
                        format!("`{}` (weight {})", holder.name, holder.weight.min(capacity))
                    };
                    let verb = if crowded.len() == 1 { "does" } else { "do" };
                    clauses.push(format!(
                        "while {holder} holds it, {} {verb} not fit beside the waiting run when \
                         it starts",
                        join_and(crowded)
                    ));
                }
                if !clauses.is_empty() {
                    warnings.push(format!(
                        "a `{}` run (weight {}) waiting for `{name}`{scope} keeps other tasks \
                         out: {}. A lower weight for `{}` leaves them room",
                        waiter.name,
                        waiter.weight.min(capacity),
                        clauses.join("; "),
                        waiter.name
                    ));
                }
            }
        }
    }
    warnings
}

fn join_and(mut parts: Vec<String>) -> String {
    match parts.pop() {
        None => String::new(),
        Some(last) if parts.is_empty() => last,
        Some(last) => format!("{} and {last}", parts.join(", ")),
    }
}

fn table<const N: usize>(rows: &[[String; N]]) -> String {
    let widths: Vec<usize> = (0..N)
        .map(|col| {
            rows.iter()
                .map(|r| r[col].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        out += cells.join("  ").trim_end();
        out += "\n";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CapacitySource;

    fn machine(capacity: u32) -> MachineConfig {
        MachineConfig {
            capacity,
            capacity_source: CapacitySource::Default { cores: capacity },
            measure_only: false,
            adapt_to_outside_cpu: true,
            resources: Default::default(),
        }
    }

    fn repo(text: &str) -> RepoConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".loadmaster.toml");
        std::fs::write(&path, text).unwrap();
        RepoConfig::load(&path).unwrap()
    }

    fn task(name: &str, weight: u32, uses: &str) -> String {
        format!("[tasks.{name}]\nweight = {weight}\nuses = {{ {uses} }}\ncmd = [\"true\"]\n")
    }

    #[test]
    fn a_weight_at_or_above_the_capacity_takes_the_machine() {
        let repo = repo(&(task("e2e", 64, "") + &task("all", 12, "") + &task("lint", 1, "")));
        let warnings = warnings(&repo, &machine(12));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0]
                .starts_with("`all` has weight 12, this machine's whole capacity: it runs alone")
        );
        assert!(warnings[1].starts_with(
            "`e2e` has weight 64, above this machine's capacity of 12, so it counts as 12"
        ));
        assert!(warnings[1].ends_with("give it a resource with capacity 1 instead"));
        assert!(super::warnings(&repo, &machine(1)).is_empty());
    }

    #[test]
    fn a_run_waiting_for_a_resource_can_crowd_out_other_tasks() {
        // clew's case: a long UI capture holds the browser. A unit run waiting
        // for it starts only when the capture ends, and every check that fits
        // beside the capture fits beside it. A capture waiting beside a unit
        // run takes 8 cores when it starts, and a 5-core check that fits
        // beside the unit run now does not fit then.
        let text = "[resources.browser]\ncapacity = 1\n\n".to_owned()
            + &task("ui", 8, "browser = 1")
            + &task("unit", 3, "browser = 1")
            + &task("types", 2, "")
            + &task("lint", 1, "")
            + &task("big", 5, "");
        assert_eq!(
            warnings_of(&text, 12),
            [
                "a `ui` run (weight 8) waiting for `browser` keeps other tasks out: while `unit` \
                 (weight 3) holds it, `big` (5) does not fit beside the waiting run when it \
                 starts. A lower weight for `ui` leaves them room",
            ]
        );
        // Measure-only admits everything at once.
        let mut measuring = machine(12);
        measuring.measure_only = true;
        assert!(warnings(&repo(&text), &measuring).is_empty());
    }

    fn warnings_of(text: &str, capacity: u32) -> Vec<String> {
        warnings(&repo(text), &machine(capacity))
    }

    #[test]
    fn runs_of_one_task_keep_nothing_from_others() {
        // One `e2e` run waits for the other's db and starts when that one
        // ends, with what it had: `test` fits beside it whenever it fits beside
        // the other.
        let text = "[resources.db]\nper_checkout = 1\n\n".to_owned()
            + &task("e2e", 4, "db = 1")
            + &task("test", 2, "");
        assert!(warnings_of(&text, 8).is_empty());
        assert!(warnings_of(&text.replace("weight = 4", "weight = 5"), 8).is_empty());
    }

    #[test]
    fn a_task_kept_out_by_a_waiting_runs_weight_is_named_once() {
        // `b` waits for the browser and the gpus that `a` holds; `c` fits
        // beside `a`, and beside `b`'s start if `b` took fewer cores.
        let text = "[resources.browser]\ncapacity = 1\n\n[resources.gpu]\ncapacity = 3\n\n"
            .to_owned()
            + &task("a", 3, "browser = 1, gpu = 2")
            + &task("b", 8, "browser = 1, gpu = 2")
            + &task("c", 5, "gpu = 1");
        assert_eq!(
            warnings_of(&text, 12),
            [
                "a `b` run (weight 8) waiting for `browser` keeps other tasks out: while `a` \
                 (weight 3) holds it, `c` (5) does not fit beside the waiting run when it \
                 starts. A lower weight for `b` leaves them room"
            ]
        );
    }

    #[test]
    fn a_task_kept_out_by_another_resource_is_not_counted() {
        // `report` needs the gpu that `e2e` holds: it waits for that, not for
        // the cores of the waiting `e2e` run.
        let text = "[resources.db]\ncapacity = 1\n\n[resources.gpu]\ncapacity = 1\n\n".to_owned()
            + &task("e2e", 4, "db = 1, gpu = 1")
            + &task("report", 1, "gpu = 1");
        assert!(warnings_of(&text, 8).is_empty());
        // `smoke` waits for `e2e`'s db too, which no weight of `e2e` changes.
        let text = text + &task("smoke", 1, "db = 1");
        assert!(
            !warnings_of(&text, 8)
                .iter()
                .any(|w| w.contains("`smoke` (1")),
            "{:?}",
            warnings_of(&text, 8)
        );
    }

    #[test]
    fn a_task_kept_out_in_the_holders_checkout_counts_from_others() {
        // `report` needs the gpu, which `smoke` holds within its checkout:
        // only runs from other checkouts are kept out by cores alone.
        let text = "[resources.db]\ncapacity = 1\n\n[resources.gpu]\nper_checkout = 1\n\n"
            .to_owned()
            + &task("e2e", 4, "db = 1, gpu = 1")
            + &task("smoke", 1, "db = 1, gpu = 1")
            + &task("report", 5, "gpu = 1");
        assert_eq!(
            warnings_of(&text, 8),
            [
                "a `e2e` run (weight 4) waiting for `db` keeps other tasks out: while `smoke` \
                 (weight 1) holds it, `report` (5, from other checkouts) does not fit beside the \
                 waiting run when it starts. A lower weight for `e2e` leaves them room",
                "a `report` run (weight 5) waiting for `gpu` in its checkout keeps other tasks \
                 out: while `smoke` (weight 1) holds it, `report` (5, from other checkouts) does \
                 not fit beside the waiting run when it starts. A lower weight for `report` \
                 leaves them room",
            ]
        );
    }

    #[test]
    fn a_task_kept_out_only_from_other_checkouts_says_so() {
        // In the holder's checkout, `t` and a one-core `w` already take 3 of
        // its 2 dbs: only from other checkouts does `w`'s weight matter.
        let text = "[resources.db]\nper_checkout = 2\n\n".to_owned()
            + &task("h", 1, "db = 1")
            + &task("w", 6, "db = 2")
            + &task("t", 3, "db = 1");
        let warnings = warnings_of(&text, 8);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("`t` (3, from other checkouts)")),
            "{warnings:?}"
        );
    }

    #[test]
    fn huge_limits_do_not_overflow() {
        let text = "[resources.db]\ncapacity = 4294967295\n\n".to_owned()
            + &task("a", 4, "db = 2147483648")
            + &task("b", 4, "db = 2147483648")
            + &task("c", 4294967295, "");
        let warnings = warnings_of(&text, 4294967295);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("`c` has weight 4294967295"));
    }

    #[test]
    fn a_resource_without_a_limit_is_pointed_out() {
        let text = "[resources.gpu]\n\n".to_owned() + &task("train", 1, "gpu = 1");
        assert_eq!(
            warnings_of(&text, 8),
            [
                "`gpu` has no capacity in this file or the machine config, so runs that use it \
              are not kept apart: set one under [resources] in the machine config"
            ]
        );
        let mut limited = machine(8);
        limited.resources.insert("gpu".into(), 1);
        assert!(warnings(&repo(&text), &limited).is_empty());
    }

    #[test]
    fn renders_tasks_resources_and_warnings() {
        let text = "[resources.db]\ncapacity = 2\nper_checkout = 1\n\n".to_owned()
            + &task("e2e", 9, "db = 1")
            + "[tasks.ui]\nweight = 2\nok_exit_codes = [0, 2]\nstop_signal = \"SIGINT\"\n\
               cmd = [\"true\"]\n";
        let repo = repo(&text);
        let out = render(&repo, &machine(8));
        let cores = crate::process::online_cpus().unwrap();
        let expected = format!(
            "{}: valid\ncapacity: 8 (default: 8 cores minus 2)\n\
             CPU busy beyond what running jobs got: lowers what jobs start with once it is more \
             than the {} of {cores} cores the capacity leaves free, down to their min_weight (a \
             task without one starts with its weight, once nothing runs ahead of it)\n\n\
             TASK  WEIGHT           USES  OK EXIT CODES  STOP SIGNAL\n\
             e2e   9 (counts as 8)  db=1  0              SIGTERM\n\
             ui    2                -     0, 2           SIGINT\n\n\
             RESOURCE  CAPACITY  PER CHECKOUT\n\
             db        2         1\n\n\
             warning: `e2e` has weight 9",
            repo.path().display(),
            cores.saturating_sub(8)
        );
        assert!(out.starts_with(&expected), "{out}");
        let fixed = MachineConfig {
            adapt_to_outside_cpu: false,
            ..machine(8)
        };
        assert!(render(&repo, &fixed).contains(
            "\nCPU busy beyond what running jobs got: ignored (adapt_to_outside_cpu = false)\n"
        ));
    }
}
