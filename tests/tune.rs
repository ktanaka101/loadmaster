//! `loadmaster tune` replays this machine's checks with other settings of
//! this repository's tasks, through the admission rule itself.

mod common;

use common::*;

/// A record of a check that ran: `test` of this checkout, got in line at
/// `at` (ms), ran `run_ms` keeping `busy` cores busy.
fn record(
    sandbox: &Sandbox,
    job: u32,
    task: &str,
    weight: u32,
    at: u64,
    run_ms: u64,
    busy: f64,
) -> String {
    let checkout = std::fs::canonicalize(&sandbox.repo).unwrap();
    let checkout = checkout.display();
    serde_json::json!({
        "job": job, "task": task, "weight": weight, "capacity": 4, "measure_only": false,
        "dir": checkout.to_string(), "checkout": checkout.to_string(),
        "workload": checkout.to_string(), "args": [], "outcome": "exited",
        "queued_at": at, "wait_ms": 0, "run_ms": run_ms,
        "cpu_ms": (run_ms as f64 * busy) as u64, "load": null, "signal": null,
        "exit_code": 0, "ok": true,
    })
    .to_string()
}

#[test]
fn tune_replays_the_history_with_other_weights() {
    let sandbox = Sandbox::new(
        4,
        &(sh_task("test", 4, "true") + &sh_task("lint", 1, "true")),
    );
    // Thirty `test`s, a second apart, each 5 s keeping 1.5 cores busy: at
    // weight 4 on 4 cores they wait for each other; their cores kept busy
    // say 2.
    let mut lines: Vec<String> = (0..30)
        .map(|i| {
            record(
                &sandbox,
                i + 1,
                "test",
                4,
                1_000_000 + u64::from(i) * 1_000,
                5_000,
                1.5,
            )
        })
        .collect();
    lines.push(String::new());
    std::fs::write(sandbox.state.join("history.jsonl"), lines.join("\n")).unwrap();

    let output = sandbox.run(&["tune", "--set", "test.weight=1"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("to the 30 checks of this machine"), "{out}");
    assert!(out.contains("(30 of them of this repository"), "{out}");
    assert!(
        out.contains(
            "measured weights (each the 90th percentile of the cores its passed runs \
                      kept busy, rounded up): `test` 4 -> 2 (p90 1.5 of 30 runs)"
        ),
        "{out}"
    );
    assert!(out.contains("[tasks.test] weight = 2;"), "{out}");
    assert!(out.contains("A scenario, not a promise"), "{out}");

    let json: serde_json::Value = serde_json::from_slice(
        &sandbox
            .run(&["tune", "--json", "--set", "test.weight=1"])
            .stdout,
    )
    .unwrap();
    let scenarios = json["scenarios"].as_array().unwrap();
    let names: Vec<&str> = scenarios
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["current", "measured weights", "your changes"]);
    let wait = |i: usize| scenarios[i]["wait_ms"]["median"].as_u64().unwrap();
    let over = |i: usize| scenarios[i]["over_capacity"].as_f64().unwrap();
    // One at a time, they queue up; two at a time, less; four at a time
    // (1.5 cores each on 4), less still, but more cores are busy than there
    // are.
    assert!(wait(0) > wait(1) && wait(1) > wait(2), "{json:#}");
    assert_eq!(over(0), 0.0, "{json:#}");
    assert_eq!(over(1), 0.0, "{json:#}");
    assert!(over(2) > 0.5, "{json:#}");
    assert_eq!(scenarios[2]["settings"]["test"]["weight"], 1);
    assert_eq!(json["measured"][0]["to"], 2);
}

#[test]
fn tune_says_when_there_is_too_little_to_replay_and_refuses_bad_changes() {
    let sandbox = Sandbox::new(
        4,
        &sh_task("test", 4, "true").replacen("cmd = ", "min_weight = 4\ncmd = ", 1),
    );
    let out = stdout(&sandbox.run(&["tune"]));
    assert!(
        out.starts_with("loadmaster tune: too few checks to replay: 0 on this machine"),
        "{out}"
    );
    // As data too; and a change is checked whatever the history.
    let json: serde_json::Value =
        serde_json::from_slice(&sandbox.run(&["tune", "--json"]).stdout).unwrap();
    assert_eq!(json["replayed"], false);
    assert_eq!(json["checks"], 0);
    let output = sandbox.run(&["tune", "--set", "test.weight=x"]);
    assert_eq!(output.status.code(), Some(125));
    // Changes are taken together, in either order.
    for sets in [
        ["test.weight=2", "test.min_weight=1"],
        ["test.min_weight=1", "test.weight=2"],
    ] {
        let output = sandbox.run(&["tune", "--set", sets[0], "--set", sets[1]]);
        assert!(output.status.success(), "{sets:?}: {}", stderr(&output));
    }
    let lines: Vec<String> = (0..25)
        .map(|i| {
            record(
                &sandbox,
                i + 1,
                "test",
                4,
                u64::from(i) * 10_000,
                1_000,
                1.0,
            )
        })
        .collect();
    std::fs::write(sandbox.state.join("history.jsonl"), lines.join("\n") + "\n").unwrap();
    for (set, why) in [
        ("nope.weight=2", "no task `nope` in .loadmaster.toml"),
        ("test.cores=2", "`cores` cannot be tuned"),
        (
            "test.weight=0",
            "the value must be a whole number, 1 or more",
        ),
        (
            "test.min_weight=8",
            "min_weight must not be above the weight",
        ),
    ] {
        let output = sandbox.run(&["tune", "--set", set]);
        assert_eq!(output.status.code(), Some(125), "{set}");
        assert!(stderr(&output).contains(why), "{set}: {}", stderr(&output));
    }
}
