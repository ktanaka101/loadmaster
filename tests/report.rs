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
    assert!(
        text.lines().next().unwrap().ends_with("): 4 in use"),
        "{text}"
    );
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

    let none = stdout(&sandbox.run(&["report", "--until", "2000-01-01"]));
    assert!(none.contains("no checks in this period"), "{none}");
    let report = sandbox.run(&["report", "--since", "1h", "--until", "2999-01-01T00:00"]);
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
        [0, 1, 6, 7, 8, 9, 10, 11]
            .map(|i| cells[i].to_owned())
            .to_vec()
    };
    // TASK WEIGHT CHECKS PASSED FAILED KILLED LEFT-LINE NOT-RUN
    assert_eq!(row("pass"), ["pass", "1", "2", "2", "0", "0", "0", "0"]);
    assert_eq!(row("fail"), ["fail", "1", "1", "0", "1", "0", "0", "0"]);
    assert_eq!(row("gone"), ["gone", "1", "1", "0", "0", "0", "0", "1"]);

    // The same, as data.
    let json: serde_json::Value =
        serde_json::from_slice(&sandbox.run(&["report", "--json"]).stdout).unwrap();
    let section = &json["sections"][0];
    assert_eq!(section["mode"], "admission");
    assert_eq!(section["capacity"], 4);
    assert_eq!(section["checks"], 4);
    let of = |task: &str| {
        section["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["task"] == task)
            .unwrap()
            .clone()
    };
    let pass = of("pass");
    assert_eq!(
        (&pass["weight"], &pass["checks"], &pass["passed"]),
        (
            &serde_json::json!(1),
            &serde_json::json!(2),
            &serde_json::json!(2)
        )
    );
    assert!(pass["run_ms"]["median"].is_u64(), "{json:#}");
    assert_eq!(of("fail")["failed"], 1);
    assert_eq!(of("gone")["not_run"], 1);
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
    let backwards = sandbox.run(&["report", "--since", "2026-10-12", "--until", "2026-10-11"]);
    assert_eq!(backwards.status.code(), Some(125));
    assert!(
        stderr(&backwards).contains("--until must be after --since"),
        "{}",
        stderr(&backwards)
    );
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
    // TASK WEIGHT USES CORES-MED CORES-P90 OVER CHECKS PASSED FAILED KILLED
    assert_eq!(cells[6..10], ["1", "0", "0", "1"], "{text}");
    assert_eq!(records[0]["started"], true);
}

#[test]
fn a_run_killed_while_waiting_left_the_line() {
    // Killed before it started: a cost of waiting, not a run killed under
    // load.
    let probe = Sandbox::new(4, "");
    let tasks = sh_task("hold", 4, &probe.hold_script("hold")) + &sh_task("waits", 1, "true");
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = probe;
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let mut waiter = sandbox.spawn(&["run", "waits"]);
    wait_until("a waiting job", || sandbox.job("waits").is_some());
    waiter.kill().unwrap();
    waiter.wait().unwrap();
    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    wait_until("the job to be reaped", || {
        sandbox.snapshot().jobs.is_empty()
    });

    let records = history(&sandbox);
    let lost = records
        .iter()
        .find(|r| r["task"] == "waits")
        .unwrap_or_else(|| panic!("{records:?}"));
    assert_eq!(lost["outcome"], "lost");
    assert_eq!(lost["started"], false);
    let text = stdout(&sandbox.run(&["report"]));
    let row = text.lines().find(|l| l.starts_with("waits ")).unwrap();
    let cells: Vec<&str> = row.split_whitespace().collect();
    // TASK WEIGHT USES CORES-MED CORES-P90 OVER CHECKS PASSED FAILED KILLED
    // LEFT-LINE
    assert_eq!(cells[6..11], ["1", "0", "0", "0", "1"], "{text}");
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
        row.split_whitespace().nth(9).unwrap().to_owned()
    };
    assert_eq!(killed("killed "), "1", "{text}");
    assert_eq!(killed("exits "), "0", "{text}");
}

#[test]
fn a_task_can_count_other_exit_codes_as_passed() {
    let sandbox = Sandbox::new(4, "");
    let tasks =
        sh_task("diff", 1, "exit 2").replacen("cmd = ", "ok_exit_codes = [0, 2]\ncmd = ", 1)
            + &sh_task("broken", 1, "exit 3").replacen(
                "cmd = ",
                "ok_exit_codes = [0, 2]\ncmd = ",
                1,
            );
    std::fs::write(sandbox.repo.join(".loadmaster.toml"), tasks).unwrap();
    // loadmaster still returns the task's own exit code.
    assert_eq!(sandbox.run(&["run", "diff"]).status.code(), Some(2));
    assert_eq!(sandbox.run(&["run", "broken"]).status.code(), Some(3));
    let records = history(&sandbox);
    assert_eq!(records[0]["ok"], true);
    assert_eq!(records[1]["ok"], false);
    let text = stdout(&sandbox.run(&["report"]));
    let counts = |task: &str| {
        let row = text
            .lines()
            .find(|l| l.starts_with(&format!("{task} ")))
            .unwrap();
        let cells: Vec<&str> = row.split_whitespace().collect();
        // PASSED FAILED
        (cells[7].to_owned(), cells[8].to_owned())
    };
    assert_eq!(counts("diff"), ("1".to_owned(), "0".to_owned()), "{text}");
    assert_eq!(counts("broken"), ("0".to_owned(), "1".to_owned()), "{text}");
}

#[test]
fn the_resources_a_task_used_are_logged_and_shown() {
    let sandbox = Sandbox::new(4, "");
    let tasks = "[resources.db]\ncapacity = 1\n\n".to_owned()
        + &sh_task("e2e", 1, "true").replacen("cmd = ", "uses = { db = 1 }\ncmd = ", 1);
    std::fs::write(sandbox.repo.join(".loadmaster.toml"), tasks).unwrap();
    assert!(sandbox.run(&["run", "e2e"]).status.success());
    let records = history(&sandbox);
    assert_eq!(records[0]["uses"], serde_json::json!({ "db": 1 }));
    let text = stdout(&sandbox.run(&["report"]));
    let row = text.lines().find(|l| l.starts_with("e2e ")).unwrap();
    // TASK WEIGHT USES
    assert_eq!(row.split_whitespace().nth(2), Some("db=1"), "{text}");
}

#[test]
fn local_times_that_do_not_exist_are_rejected() {
    let sandbox = Sandbox::new(4, &task("pass", 1, &["true"]));
    let report = |tz: &str, since: &str| {
        let mut cmd = sandbox.cmd(&["report", "--since", since]);
        cmd.env("TZ", tz);
        output(cmd)
    };
    // Clocks in New York skip from 02:00 to 03:00 on 2026-03-08.
    let skipped = report("America/New_York", "2026-03-08T02:30");
    assert_eq!(skipped.status.code(), Some(125), "{}", stderr(&skipped));
    assert!(
        report("America/New_York", "2026-03-08T03:30")
            .status
            .success()
    );
    // Before 1970 in UTC, which is still a valid start.
    let early = report("Asia/Tokyo", "1970-01-01");
    assert!(early.status.success(), "{}", stderr(&early));
}
