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
    }

    let mut rows = vec![["TASK", "WEIGHT", "USES", "OK EXIT CODES"].map(String::from)];
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
                "`{name}` has no limit on this machine, so it holds no job back: set its \
                 capacity under [resources] in the machine config"
            )
        })
        .collect()
}

/// A job waiting for a resource still sets aside its cores, so that later
/// jobs cannot delay it once the resource is free. Next to a long run that
/// holds the resource, that can leave too few cores for other tasks. One
/// warning per waiting task, since its weight is what can be changed.
fn set_aside_while_waiting(repo: &RepoConfig, machine: &MachineConfig) -> Vec<String> {
    let capacity = machine.capacity;
    let mut warned: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
    let mut warnings = Vec::new();
    for (name, resource) in repo.resources() {
        let machine_wide = machine.resources.get(name).copied().or(resource.capacity);
        let users: Vec<(&Task, u32)> = repo
            .tasks()
            .filter_map(|task| Some((task, *task.uses.get(name)?)))
            .collect();
        for (limit, scope) in [
            (machine_wide, ""),
            (resource.per_checkout, " in its checkout"),
        ] {
            let Some(limit) = limit else { continue };
            for &(waiter, wanted) in &users {
                let w = waiter.weight.min(capacity);
                let mut clauses = Vec::new();
                for &(holder, held) in &users {
                    if held.min(limit) + wanted.min(limit) <= limit
                        || !warned.insert((name, &waiter.name, &holder.name))
                    {
                        continue;
                    }
                    let h = holder.weight.min(capacity);
                    let free = capacity.saturating_sub(h + w);
                    // Tasks that would fit beside the holder but for what
                    // the waiting run sets aside.
                    let crowded: Vec<String> = repo
                        .tasks()
                        .filter(|task| !task.uses.contains_key(name))
                        .filter(|task| (free + 1..=capacity - h).contains(&task.weight))
                        .map(|task| format!("`{}` ({})", task.name, task.weight))
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
                    clauses.push(format!(
                        "while {holder} holds it, that leaves {free} of {capacity}, so {} \
                         {verb} too",
                        join_and(crowded)
                    ));
                }
                if !clauses.is_empty() {
                    warnings.push(format!(
                        "a `{}` run waiting for `{name}`{scope} sets aside its {w} cores \
                         meanwhile: {}. A lower weight for `{}` leaves them room",
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
        // clew's case: a long UI capture holds the browser; a unit run that
        // needs it sets aside 3 cores, and the 2-core checks stop.
        let text = "[resources.browser]\ncapacity = 1\n\n".to_owned()
            + &task("ui", 8, "browser = 1")
            + &task("unit", 3, "browser = 1")
            + &task("types", 2, "")
            + &task("lint", 1, "")
            + &task("big", 5, "");
        assert_eq!(
            warnings_of(&text, 12),
            [
                "a `ui` run waiting for `browser` sets aside its 8 cores meanwhile: while \
                 another `ui` run holds it, that leaves 0 of 12, so `lint` (1) and `types` (2) \
                 wait too; while `unit` (weight 3) holds it, that leaves 1 of 12, so `big` (5) \
                 and `types` (2) wait too. A lower weight for `ui` leaves them room",
                // `big` does not fit beside `ui` anyway, and `lint` still fits.
                "a `unit` run waiting for `browser` sets aside its 3 cores meanwhile: while \
                 `ui` (weight 8) holds it, that leaves 1 of 12, so `types` (2) waits too. A \
                 lower weight for `unit` leaves them room",
            ]
        );
        let lighter = text.replace("weight = 3", "weight = 1");
        assert_eq!(warnings_of(&lighter, 12).len(), 1, "only `ui`'s is left");
    }

    fn warnings_of(text: &str, capacity: u32) -> Vec<String> {
        warnings(&repo(text), &machine(capacity))
    }

    #[test]
    fn runs_of_one_task_can_crowd_out_others_in_a_checkout() {
        let text = "[resources.db]\nper_checkout = 1\n\n".to_owned()
            + &task("e2e", 4, "db = 1")
            + &task("test", 4, "");
        assert_eq!(
            warnings_of(&text, 8),
            [
                "a `e2e` run waiting for `db` in its checkout sets aside its 4 cores meanwhile: \
              while another `e2e` run holds it, that leaves 0 of 8, so `test` (4) waits too. \
              A lower weight for `e2e` leaves them room"
            ]
        );
    }

    #[test]
    fn a_resource_without_a_limit_is_pointed_out() {
        let text = "[resources.gpu]\n\n".to_owned() + &task("train", 1, "gpu = 1");
        assert_eq!(
            warnings_of(&text, 8),
            [
                "`gpu` has no limit on this machine, so it holds no job back: set its capacity \
              under [resources] in the machine config"
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
            + "[tasks.ui]\nweight = 2\nok_exit_codes = [0, 2]\ncmd = [\"true\"]\n";
        let repo = repo(&text);
        let out = render(&repo, &machine(8));
        let expected = format!(
            "{}: valid\ncapacity: 8 (default: 8 cores minus 2)\n\n\
             TASK  WEIGHT           USES  OK EXIT CODES\n\
             e2e   9 (counts as 8)  db=1  0\n\
             ui    2                -     0, 2\n\n\
             RESOURCE  CAPACITY  PER CHECKOUT\n\
             db        2         1\n\n\
             warning: `e2e` has weight 9",
            repo.path().display()
        );
        assert!(out.starts_with(&expected), "{out}");
    }
}
