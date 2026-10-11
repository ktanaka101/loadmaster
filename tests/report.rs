//! Measure-only mode and `loadmaster report`: a baseline is recorded without
//! anyone waiting, and the report sets it apart from runs under admission.

mod common;

use common::*;

fn history(sandbox: &Sandbox) -> Vec<serde_json::Value> {
    std::fs::read_to_string(sandbox.state.join("history.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn measure_only_starts_everything_at_once_and_records_it() {
    let probe = Sandbox::new(2, "");
    let tasks =
        sh_task("a", 2, &probe.hold_script("a")) + &sh_task("b", 2, &probe.hold_script("b"));
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    std::fs::write(&probe.machine_config, "capacity = 2\nmeasure_only = true\n").unwrap();
    let sandbox = probe;

    let mut a = sandbox.spawn(&["run", "a"]);
    let mut b = sandbox.spawn(&["run", "b"]);
    // Together they weigh twice the capacity, yet both run.
    wait_until("both tasks to start", || {
        sandbox.started("a") && sandbox.started("b")
    });
    let status = sandbox.run(&["status"]);
    let text = stdout(&status);
    assert!(text.starts_with("4/2 in use"), "{text}");
    assert!(text.contains("measure-only: jobs start at once"), "{text}");

    sandbox.release("a");
    sandbox.release("b");
    assert!(wait(&mut a).success());
    assert!(wait(&mut b).success());
    for record in history(&sandbox) {
        assert_eq!(record["measure_only"], true, "{record}");
        assert_eq!(record["capacity"], 2, "{record}");
        assert!(record["load"].as_f64().is_some(), "{record}");
        assert!(record["cpu_ms"].as_u64().is_some(), "{record}");
    }

    let report = sandbox.run(&["report"]);
    assert_eq!(report.status.code(), Some(0));
    let text = stdout(&report);
    assert!(
        text.contains("== measure-only (capacity 2): 2 check(s)"),
        "{text}"
    );
    assert!(text.contains("of the time checks ran, peak 4"), "{text}");
}

#[test]
fn report_summarizes_results_per_task() {
    let sandbox = Sandbox::new(
        4,
        &(task("pass", 1, &["true"])
            + &task("fail", 1, &["false"])
            + &task("gone", 1, &["no-such-program"])),
    );
    for task in ["pass", "pass", "fail", "gone"] {
        sandbox.run(&["run", task]);
    }

    let report = sandbox.run(&["report", "--since", "1h"]);
    assert_eq!(report.status.code(), Some(0), "{}", stderr(&report));
    let text = stdout(&report);
    assert!(
        text.contains("== admission (capacity 4): 4 check(s)"),
        "{text}"
    );
    let row = |task: &str| -> Vec<String> {
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("{task} ")))
            .unwrap_or_else(|| panic!("no row for {task}: {text}"));
        // Everything but CORES, which depends on the machine.
        let cells: Vec<&str> = line.split_whitespace().collect();
        [0, 1, 3, 4, 5, 6, 7].map(|i| cells[i].to_owned()).to_vec()
    };
    // TASK WEIGHT CHECKS PASSED FAILED KILLED NOT-RUN
    assert_eq!(row("pass"), ["pass", "1", "2", "2", "0", "0", "0"]);
    assert_eq!(row("fail"), ["fail", "1", "1", "0", "1", "0", "0"]);
    assert_eq!(row("gone"), ["gone", "1", "1", "0", "0", "0", "1"]);
}

#[test]
fn report_without_history_or_with_bad_arguments() {
    let sandbox = Sandbox::new(4, &task("pass", 1, &["true"]));
    let empty = sandbox.run(&["report"]);
    assert_eq!(empty.status.code(), Some(0));
    assert!(
        stdout(&empty).contains("no checks recorded yet"),
        "{}",
        stdout(&empty)
    );

    let bad = sandbox.run(&["report", "--since", "7"]);
    assert_eq!(bad.status.code(), Some(125));
    assert!(stderr(&bad).contains("like 7d"), "{}", stderr(&bad));
}

#[test]
fn a_run_whose_loadmaster_is_killed_is_still_logged() {
    let probe = Sandbox::new(4, "");
    let tasks = sh_task("hold", 2, &probe.hold_script("hold"));
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = probe;
    let mut run = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    run.kill().unwrap();
    run.wait().unwrap();
    // The task outlives loadmaster and holds the slot until it exits; then
    // whoever reaps the job logs it.
    sandbox.release("hold");
    wait_until("the job to be reaped", || {
        sandbox.snapshot().jobs.is_empty()
    });

    let records = history(&sandbox);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["outcome"], "lost");
    assert_eq!(records[0]["task"], "hold");
    let text = stdout(&sandbox.run(&["report"]));
    let row = text.lines().find(|l| l.starts_with("hold ")).unwrap();
    let cells: Vec<&str> = row.split_whitespace().collect();
    // TASK WEIGHT CORES CHECKS PASSED FAILED KILLED
    assert_eq!(cells[3..7], ["1", "0", "0", "1"], "{text}");
}

#[test]
fn a_signal_is_told_apart_from_an_exit_code() {
    let sandbox = Sandbox::new(
        4,
        &(sh_task("killed", 1, "kill -TERM $$") + &sh_task("exits", 1, "exit 143")),
    );
    assert_eq!(sandbox.run(&["run", "killed"]).status.code(), Some(143));
    assert_eq!(sandbox.run(&["run", "exits"]).status.code(), Some(143));
    let records = history(&sandbox);
    assert_eq!(records[0]["signal"], 15);
    assert_eq!(records[1]["signal"], serde_json::Value::Null);
    let text = stdout(&sandbox.run(&["report"]));
    let killed = |task: &str| {
        let row = text.lines().find(|l| l.starts_with(task)).unwrap();
        row.split_whitespace().nth(6).unwrap().to_owned()
    };
    assert_eq!(killed("killed "), "1", "{text}");
    assert_eq!(killed("exits "), "0", "{text}");
}
