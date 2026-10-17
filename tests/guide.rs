//! `loadmaster guide` tells an agent how to run checks here: the rules, and
//! this repository's tasks, even without a working setup.

mod common;

use common::*;

#[test]
fn the_guide_lists_the_tasks_with_what_an_agent_needs_to_choose_one() {
    let tasks = sh_task("lint", 1, "true").replacen(
        "cmd = ",
        "description = \"Lint the repository\"\ncmd = ",
        1,
    ) + &sh_task("test", 8, "true").replacen(
        "cmd = ",
        "min_weight = 2\nok_exit_codes = [0, 2]\ncmd = ",
        1,
    );
    let sandbox = Sandbox::new(4, &tasks);
    for _ in 0..3 {
        assert!(sandbox.run(&["run", "lint"]).status.success());
    }
    let output = sandbox.run(&["guide"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Machine: capacity 4 cores ("), "{out}");
    let lint = out.lines().find(|l| l.starts_with("- lint ")).unwrap();
    assert!(lint.starts_with("- lint (weight 1; usually "), "{out}");
    assert!(lint.ends_with("): Lint the repository"), "{out}");
    // Capped at the capacity, as it counts here.
    assert!(
        out.contains("- test (weight 4; may start with 2; passes on exit 0 or 2)\n"),
        "{out}"
    );
    for rule in [
        "\nRules:\n1. Run checks only through loadmaster",
        "`gave up ... (exit 75)`: the `loadmaster: next:` line",
        "`loadmaster cancel <ID>`",
        "`loadmaster doctor`",
    ] {
        assert!(out.contains(rule), "{rule:?} in {out}");
    }
}

#[test]
fn the_guide_says_the_rules_without_a_working_setup() {
    // No config: how to set one up.
    let sandbox = Sandbox::new(4, "");
    std::fs::remove_file(sandbox.repo.join(".loadmaster.toml")).unwrap();
    let out = stdout(&sandbox.run(&["guide"]));
    assert!(
        out.contains("No checks here go through loadmaster: "),
        "{out}"
    );
    assert!(out.contains("[tasks.test]\n"), "{out}");
    assert!(out.contains("\nRules:\n"), "{out}");
    // A broken one: what is wrong, and the rules.
    std::fs::write(
        sandbox.repo.join(".loadmaster.toml"),
        "[tasks.x]\nweight = 0\n",
    )
    .unwrap();
    let output = sandbox.run(&["guide"]);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains(".loadmaster.toml: invalid ("), "{out}");
    assert!(out.contains("`loadmaster check` says what to fix"), "{out}");
    assert!(out.contains("\nRules:\n"), "{out}");
}
