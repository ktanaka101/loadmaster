//! `loadmaster check`: what a repository's config means on this machine,
//! without touching the queue. Loading the configs validates them; this adds
//! what the admission rule will make of them here.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::config::{MachineConfig, RepoConfig, Task};

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
             than the {spare} of {cores} cores the capacity leaves free\n"
        );
    }

    let mut rows =
        vec![["TASK", "WEIGHT", "USES", "OK EXIT CODES", "STOP SIGNAL"].map(String::from)];
    for task in repo.tasks() {
        rows.push([
            task.name.clone(),
            if task.weight > capacity {
                format!("{} (counts as {capacity})", task.weight)
            } else {
                task.weight.to_string()
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
    warnings.extend(set_aside_while_waiting(repo, machine));
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
        Ordering::Greater => format!(
            "`{}` has weight {}, above this machine's capacity of {capacity}, so it counts \
             as {capacity}",
            task.name, task.weight
        ),
    };
    Some(format!(
        "{what}: it runs alone, and every job that gets in line after it waits until it has \
         finished. If the point is to keep its runs from overlapping, give it a resource with \
         capacity 1 instead"
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

/// A job waiting for a resource sets aside the cores that the run holding
/// it does not free when it ends, so that later jobs cannot delay it once
/// the resource is free; if they do not fit beside that run, it holds back
/// the cores of every job behind it. Next to a long run, either can keep
/// other tasks out. One warning per waiting task, since its weight is what
/// can be changed.
fn set_aside_while_waiting(repo: &RepoConfig, machine: &MachineConfig) -> Vec<String> {
    // In u64: sums of u32 amounts cannot overflow it.
    let capacity = u64::from(machine.capacity);
    let weight = |task: &Task| u64::from(task.weight).min(capacity);
    let limit_of = |name: &str| {
        machine
            .resources
            .get(name)
            .copied()
            .or(repo.resource(name)?.capacity)
            .map(u64::from)
    };
    // The tightest limit on a resource that a task beside the holder could
    // meet: in the holder's checkout, its per-checkout one.
    let tightest = |name: &str| {
        let per_checkout = repo
            .resource(name)
            .and_then(|r| r.per_checkout)
            .map(u64::from);
        match (limit_of(name), per_checkout) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    };
    let amount = |task: &Task, name: &str| task.uses.get(name).map_or(0, |&a| u64::from(a));
    let mut warned: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
    let mut warnings = Vec::new();
    for (name, resource) in repo.resources() {
        let users: Vec<&Task> = repo
            .tasks()
            .filter(|task| task.uses.contains_key(name))
            .collect();
        let scopes = [
            (limit_of(name), ""),
            (resource.per_checkout.map(u64::from), " in its checkout"),
        ];
        for (limit, scope) in scopes {
            let Some(limit) = limit else { continue };
            for &waiter in &users {
                let w = weight(waiter);
                let mut clauses = Vec::new();
                for &holder in &users {
                    let (held, wanted) = (amount(holder, name), amount(waiter, name));
                    if held.min(limit) + wanted.min(limit) <= limit
                        || !warned.insert((name, &waiter.name, &holder.name))
                    {
                        continue;
                    }
                    let h = weight(holder);
                    let short_of_cores = h + w > capacity;
                    let set_aside = w.saturating_sub(h);
                    let free = if short_of_cores {
                        0
                    } else {
                        capacity - h - set_aside
                    };
                    // Tasks that would run beside the holder but for the
                    // waiting run's cores: not ones that use this resource
                    // (the waiting run, short of it, holds it back from
                    // everyone behind), nor ones that some other resource
                    // keeps out anyway: one the waiting run is short of too,
                    // or one it fits in, where it sets aside only what the
                    // holder does not free.
                    let fits_beside = |task: &Task, limit: &dyn Fn(&str) -> Option<u64>| {
                        task.uses.keys().all(|other| {
                            limit(other).is_none_or(|l| {
                                let held = amount(holder, other).min(l);
                                let wanted = amount(waiter, other).min(l);
                                held + wanted <= l
                                    && held
                                        + wanted.saturating_sub(held)
                                        + amount(task, other).min(l)
                                        <= l
                            })
                        })
                    };
                    let crowded: Vec<String> = repo
                        .tasks()
                        .filter(|task| !task.uses.contains_key(name))
                        .filter(|task| (free + 1..=capacity - h).contains(&weight(task)))
                        .filter(|task| fits_beside(task, &limit_of))
                        .map(|task| {
                            // From the holder's checkout, a per-checkout
                            // limit may keep it out anyway.
                            let elsewhere = if fits_beside(task, &tightest) {
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
                    let holder = if holder.name == waiter.name {
                        format!("another `{}` run", holder.name)
                    } else {
                        format!("`{}` (weight {h})", holder.name)
                    };
                    let verb = if crowded.len() == 1 { "waits" } else { "wait" };
                    let effect = if short_of_cores {
                        format!(
                            "{h} + {w} cores are more than {capacity}: it holds back the \
                             cores of every job behind it"
                        )
                    } else {
                        format!(
                            "it sets aside the {set_aside} of its {w} cores that the holder does \
                             not free when it ends, leaving {free} of {capacity}"
                        )
                    };
                    clauses.push(format!(
                        "while {holder} holds it, {effect}, so {} {verb} too",
                        join_and(crowded)
                    ));
                }
                if !clauses.is_empty() {
                    warnings.push(format!(
                        "a `{}` run (weight {w}) waiting for `{name}`{scope} keeps other tasks \
                         out: {}. A lower weight for `{}` leaves them room",
                        waiter.name,
                        clauses.join("; "),
                        waiter.name
                    ));
                }
            }
        }
    }
    warnings
}

/// "a", "a and b", "a, b and c".
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
        // clew's case: a long UI capture holds the browser. A unit run that
        // needs it counts on the capture's cores and sets none aside; a
        // capture waiting for it beside a unit run sets aside the 5 cores the
        // unit run does not free, and a 5-core check stops.
        let text = "[resources.browser]\ncapacity = 1\n\n".to_owned()
            + &task("ui", 8, "browser = 1")
            + &task("unit", 3, "browser = 1")
            + &task("types", 2, "")
            + &task("lint", 1, "")
            + &task("big", 5, "");
        assert_eq!(
            warnings_of(&text, 12),
            [
                "a `ui` run (weight 8) waiting for `browser` keeps other tasks out: while \
                 another `ui` run holds it, 8 + 8 cores are more than 12: it holds back the \
                 cores of every job behind it, so `lint` (1) and `types` (2) wait too; while \
                 `unit` (weight 3) holds it, it sets aside the 5 of its 8 cores that the holder \
                 does not free when it ends, leaving 4 of 12, so `big` (5) waits too. A lower \
                 weight for `ui` leaves them room",
            ]
        );
        // A heavier unit run frees more, and the capture waiting beside it
        // sets aside less.
        let heavier = text.replace("weight = 3", "weight = 4");
        assert!(
            warnings_of(&heavier, 12)[0].contains("the 4 of its 8 cores"),
            "{:?}",
            warnings_of(&heavier, 12)
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
    fn runs_of_one_task_set_no_cores_aside_for_each_other() {
        // One `e2e` run waits for the other's db, and gets its cores when
        // that one ends: `test` fits beside them, unless the two together are
        // more than the capacity.
        let text = "[resources.db]\nper_checkout = 1\n\n".to_owned()
            + &task("e2e", 4, "db = 1")
            + &task("test", 2, "");
        assert!(warnings_of(&text, 8).is_empty());
        assert_eq!(
            warnings_of(&text.replace("weight = 4", "weight = 5"), 8),
            [
                "a `e2e` run (weight 5) waiting for `db` in its checkout keeps other tasks out: \
                 while another `e2e` run holds it, 5 + 5 cores are more than 8: it holds back \
                 the cores of every job behind it, so `test` (2) waits too. A lower weight for \
                 `e2e` leaves them room"
            ]
        );
    }

    #[test]
    fn what_the_holder_frees_of_another_resource_is_not_set_aside_either() {
        // `b` waits for the browser that `a` holds, and counts on the 2 gpus
        // `a` frees: `c` fits beside them on gpus, and only the 5 cores `b`
        // sets aside keep it out.
        let text = "[resources.browser]\ncapacity = 1\n\n[resources.gpu]\ncapacity = 3\n\n"
            .to_owned()
            + &task("a", 3, "browser = 1, gpu = 2")
            + &task("b", 8, "browser = 1, gpu = 1")
            + &task("c", 5, "gpu = 1");
        let warnings = warnings_of(&text, 12);
        assert!(
            warnings.iter().any(|w| w.starts_with(
                "a `b` run (weight 8) waiting for `browser` keeps other tasks out: while `a` \
                 (weight 3) holds it, it sets aside the 5 of its 8 cores that the holder does \
                 not free when it ends, leaving 4 of 12, so `c` (5) waits too"
            )),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_task_kept_out_by_what_the_waiting_run_is_short_of_is_not_counted() {
        // `b` waits for the browser and for gpus too, beside `a`: no job
        // behind it may use a gpu, whatever its weight.
        let text = "[resources.browser]\ncapacity = 1\n\n[resources.gpu]\ncapacity = 3\n\n"
            .to_owned()
            + &task("a", 3, "browser = 1, gpu = 2")
            + &task("b", 8, "browser = 1, gpu = 2")
            + &task("c", 5, "gpu = 1");
        let warnings = warnings_of(&text, 12);
        assert!(
            !warnings.iter().any(|w| w.contains("`c` (5)")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_task_kept_out_by_another_resource_is_not_counted() {
        // `report` needs the gpu that `e2e` holds: it waits for that, not for
        // the cores the waiting `e2e` run sets aside.
        let text = "[resources.db]\ncapacity = 1\n\n[resources.gpu]\ncapacity = 1\n\n".to_owned()
            + &task("e2e", 4, "db = 1, gpu = 1")
            + &task("report", 1, "gpu = 1");
        assert!(warnings_of(&text, 8).is_empty());
    }

    #[test]
    fn a_task_kept_out_in_the_holders_checkout_counts_from_others() {
        // `report` needs the gpu, which `smoke` and the waiting `e2e` run
        // hold within the checkout: only runs from other checkouts are kept
        // out by cores alone.
        let text = "[resources.db]\ncapacity = 1\n\n[resources.gpu]\nper_checkout = 1\n\n"
            .to_owned()
            + &task("e2e", 4, "db = 1, gpu = 1")
            + &task("smoke", 1, "db = 1, gpu = 1")
            + &task("report", 5, "gpu = 1");
        assert_eq!(
            warnings_of(&text, 8),
            [
                "a `e2e` run (weight 4) waiting for `db` keeps other tasks out: while `smoke` \
                 (weight 1) holds it, it sets aside the 3 of its 4 cores that the holder does \
                 not free when it ends, leaving 4 of 8, so `report` (5, from other checkouts) \
                 waits too. A lower weight for `e2e` leaves them room"
            ]
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
             than the {} of {cores} cores the capacity leaves free\n\n\
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
