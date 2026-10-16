//! `loadmaster check` validates the configs and says what they mean on this
//! machine, without touching the queue: usable in CI.

mod common;

use common::*;

#[test]
fn a_valid_config_passes_with_its_warnings() {
    let sandbox = Sandbox::new(
        4,
        &(task("lint", 1, &["true"]) + &task("e2e", 8, &["true"])),
    );
    // In CI there is no queue yet; checking must not create one.
    std::fs::remove_dir(&sandbox.state).unwrap();
    let output = sandbox.run(&["check"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let out = stdout(&output);
    // The repository's path as the OS resolves the working directory.
    let config = std::fs::canonicalize(&sandbox.repo)
        .unwrap()
        .join(".loadmaster.toml");
    assert!(
        out.starts_with(&format!(
            "{}: valid\ncapacity: 4 (set in {})\n",
            config.display(),
            sandbox.machine_config.display()
        )),
        "{out}"
    );
    assert!(
        out.contains("\ne2e   8 (counts as 4)  -     0              SIGTERM\n"),
        "{out}"
    );
    assert!(
        out.contains("warning: `e2e` has weight 8, above this machine's capacity of 4"),
        "{out}"
    );
    assert!(!sandbox.state.exists(), "check created the state directory");
}

#[test]
fn an_invalid_config_fails() {
    let sandbox = Sandbox::new(4, "[tasks.lint]\nwieght = 1\ncmd = [\"true\"]\n");
    let output = sandbox.run(&["check"]);
    assert_eq!(output.status.code(), Some(125));
    let err = stderr(&output);
    assert!(err.contains("unknown field `wieght`"), "{err}");
    assert!(
        err.contains("an unknown key is a typo, or one that a newer version added"),
        "{err}"
    );
}

#[test]
fn a_config_for_a_newer_version_says_so() {
    let sandbox = Sandbox::new(
        4,
        "min_version = \"99.0\"\n[tasks.lint]\nweight = 1\nnew_key = true\ncmd = [\"true\"]\n",
    );
    for args in [&["check"][..], &["run", "lint"]] {
        let output = sandbox.run(args);
        assert_eq!(output.status.code(), Some(125), "{args:?}");
        let err = stderr(&output);
        assert!(
            err.contains("needs loadmaster 99.0 or newer (its `min_version`), but this is"),
            "{args:?}: {err}"
        );
    }
}

#[test]
fn a_broken_machine_config_fails() {
    let sandbox = Sandbox::new(4, &task("lint", 1, &["true"]));
    std::fs::write(&sandbox.machine_config, "capasity = 4\n").unwrap();
    let output = sandbox.run(&["check"]);
    assert_eq!(output.status.code(), Some(125));
    assert!(stderr(&output).contains("unknown field `capasity`"));
}

#[test]
fn a_scenario_is_judged_as_the_queue_it_was_captured_from() {
    let sandbox = Sandbox::new(4, "");
    std::fs::write(
        sandbox.repo.join(".loadmaster.toml"),
        sh_task("hold", 3, &sandbox.hold_script("hold"))
            + &task("e2e", 4, &["true"])
            + &task("lint", 1, &["true"]),
    )
    .unwrap();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let mut waiter = sandbox.spawn(&["run", "e2e"]);
    wait_until("e2e to wait", || {
        sandbox
            .job("e2e")
            .is_some_and(|j| j.state == loadmaster::queue::JobState::Waiting)
    });
    let captured = sandbox.path("status.json");
    std::fs::write(&captured, sandbox.run(&["status", "--json"]).stdout).unwrap();
    let status = stdout(&sandbox.run(&["status"]));
    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    assert!(wait(&mut waiter).success());

    // The queue is empty now; the scenario is the queue as captured.
    let output = sandbox.run(&["check", "--scenario", captured.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("#1 `hold` holds 3 cores\n"), "{out}");
    let waits = "#2 `e2e` waits for all 4 cores (3 in use)";
    assert!(out.contains(&format!("{waits}\n")), "{out}");
    assert!(status.contains(waits), "{status}");
}

#[test]
fn a_scenario_gives_the_cpu_busy_outside_the_queue() {
    // Nothing runs; `lint` adapts. With 12 of 14 cores busy outside the
    // queue, of a capacity of 8 that leaves 2 (the 6 cores it leaves free
    // are busy, and 6 more).
    let scenario = |cpu: &str| {
        format!(
            r#"{{"format": 2, "cpu": {cpu}, "jobs": [{{"id": 7, "state": "waiting",
                "task": "lint", "weight": 4, "cores": null, "uses": {{}},
                "checkout": "/repo", "capacity": 8, "adapts_to_outside_cpu": true,
                "measure_only": false, "pid": 1}}]}}"#
        )
    };
    let sandbox = Sandbox::new(4, "");
    let file = sandbox.path("scenario.json");
    let judge = |cpu: &str| {
        std::fs::write(&file, scenario(cpu)).unwrap();
        let output = sandbox.run(&["check", "--scenario", file.to_str().unwrap()]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        stdout(&output)
    };
    assert!(judge("null").contains("#7 `lint` starts now with 4 cores\n"));
    let busy = judge(r#"{"cores": 14, "busy_beyond_running_jobs": 12}"#);
    assert!(busy.contains("12 of 14 cores"), "{busy}");
    assert!(
        busy.contains("#7 `lint` starts now with 2 cores\n"),
        "{busy}"
    );

    std::fs::write(&file, r#"{"format": 1, "jobs": []}"#).unwrap();
    let output = sandbox.run(&["check", "--scenario", file.to_str().unwrap()]);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("is not a scenario: it is not a status of format 2"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_scenario_starts_measure_only_jobs_at_once_and_refuses_duplicate_ids() {
    let sandbox = Sandbox::new(4, "");
    let file = sandbox.path("scenario.json");
    let job = |id: u32, weight: u32, measure_only: bool| {
        format!(
            r#"{{"id": {id}, "state": "waiting", "task": "t{id}", "weight": {weight},
                "cores": null, "uses": {{}}, "checkout": "/repo", "capacity": 8,
                "adapts_to_outside_cpu": true, "measure_only": {measure_only}}}"#
        )
    };
    let judge = |jobs: &[String]| {
        let cpu = r#"{"cores": 8, "busy_beyond_running_jobs": 6}"#;
        std::fs::write(
            &file,
            format!(
                r#"{{"format": 2, "cpu": {cpu}, "jobs": [{}]}}"#,
                jobs.join(",")
            ),
        )
        .unwrap();
        sandbox.run(&["check", "--scenario", file.to_str().unwrap()])
    };
    // Whatever the CPU outside the queue and the jobs ahead.
    let output = judge(&[job(1, 8, false), job(2, 4, true)]);
    assert!(
        stdout(&output).contains("#2 `t2` is measure-only: it starts at once with 4 cores\n"),
        "{}",
        stdout(&output)
    );
    let parked = job(3, 4, true).replace("\"waiting\"", "\"parked\"");
    let output = judge(&[job(1, 8, false), parked]);
    assert!(
        stdout(&output).contains(
            "#3 `t3` is parked and measure-only: resumed, it starts at once with 4 cores\n"
        ),
        "{}",
        stdout(&output)
    );
    let output = judge(&[job(1, 8, false), job(1, 1, false)]);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("two jobs have the ID 1"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_scenario_refuses_a_min_weight_not_from_1_to_its_weight() {
    let sandbox = Sandbox::new(4, "");
    let file = sandbox.path("scenario.json");
    let judge = |min_weight: &str| {
        std::fs::write(
            &file,
            format!(
                r#"{{"format": 2, "cpu": null, "jobs": [{{"id": 7, "state": "waiting",
                    "task": "test", "weight": 8, "cores": null, "uses": {{}},
                    "checkout": "/repo", "capacity": 4, "adapts_to_outside_cpu": true,
                    "measure_only": false{min_weight}}}]}}"#
            ),
        )
        .unwrap();
        sandbox.run(&["check", "--scenario", file.to_str().unwrap()])
    };
    // Without one, as 0.14 wrote it, and with one; above the capacity, like
    // the weight, it counts as all of it.
    for min_weight in ["", r#", "min_weight": 2"#, r#", "min_weight": 6"#] {
        let output = judge(min_weight);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(
            stdout(&output).contains("#7 `test` starts now with 4 cores\n"),
            "{}",
            stdout(&output)
        );
    }
    for least in [0, 9] {
        let output = judge(&format!(r#", "min_weight": {least}"#));
        assert_ne!(output.status.code(), Some(0));
        assert!(
            stderr(&output).contains(&format!(
                "job #7 has min_weight {least}, which is not from 1 to its weight 8"
            )),
            "{}",
            stderr(&output)
        );
    }
}
