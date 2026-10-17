//! `loadmaster status` shows running and waiting jobs and the capacity in use.

mod common;

use common::*;
use loadmaster::queue::JobState;

#[test]
fn empty_queue() {
    let sandbox = Sandbox::new(6, "");
    let output = sandbox.run(&["status"]);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.starts_with("capacity 6 (set in "), "{out}");
    assert!(
        out.lines().next().unwrap().ends_with("): 0 in use"),
        "{out}"
    );
    assert!(out.contains("no jobs"), "{out}");
}

#[test]
fn shows_running_and_waiting_jobs() {
    let probe = Sandbox::new(4, "");
    let tasks = sh_task("hold", 3, &probe.hold_script("hold")) + &sh_task("e2e", 4, "true");
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = probe;

    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let mut waiter = sandbox.spawn(&["run", "e2e"]);
    wait_until("e2e to wait", || {
        sandbox
            .job("e2e")
            .is_some_and(|j| j.state == JobState::Waiting)
    });

    let out = stdout(&sandbox.run(&["status"]));
    assert!(
        out.lines().next().unwrap().ends_with("): 3 in use"),
        "{out}"
    );
    // One line per job, in line order, saying what an agent needs of it.
    let rows: Vec<&str> = out.lines().filter(|l| l.starts_with('#')).collect();
    assert_eq!(rows.len(), 2, "{out}");
    assert!(
        rows[0].starts_with("#1 running `hold` with 3 cores, for "),
        "{out}"
    );
    assert!(
        rows[1].starts_with("#2 waiting `e2e` (weight 4), for "),
        "{out}"
    );
    assert!(
        rows[1].ends_with(": next in line; waits for all 4 cores (3 in use)"),
        "{out}"
    );
    // Both were run from the checkout `status` is run from.
    assert!(
        rows.iter().all(|row| row.contains(" (same checkout)")),
        "{out}"
    );
    let elsewhere = sandbox
        .cmd(&["status"])
        .current_dir(&sandbox.root)
        .output()
        .unwrap();
    assert!(
        !stdout(&elsewhere).contains("same checkout"),
        "{}",
        stdout(&elsewhere)
    );

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    assert!(wait(&mut waiter).success());
}

#[test]
fn waiting_job_reports_progress_on_stderr() {
    let probe = Sandbox::new(4, "");
    let tasks = sh_task("hold", 3, &probe.hold_script("hold")) + &sh_task("test", 4, "true");
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = probe;
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));

    let mut waiter = sandbox
        .cmd(&["run", "test"])
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(sandbox.path("waiter.err")).unwrap())
        .spawn()
        .unwrap();
    wait_until("the waiting message", || {
        sandbox.read("waiter.err").contains(
            "loadmaster: #2 `test` is waiting (weight 4, next in line) for all 4 cores (3 in use); \
             running: #1 `hold` for ",
        )
    });
    sandbox.release("hold");
    assert!(wait(&mut waiter).success());
    assert!(
        sandbox
            .read("waiter.err")
            .contains("loadmaster: #2 `test` started after waiting"),
        "{}",
        sandbox.read("waiter.err")
    );
    wait(&mut holder);
}

/// Many processes opening a brand-new queue at once must all succeed: SQLite
/// switches to WAL without consulting the busy timeout, which used to surface
/// as an occasional "database is locked".
#[test]
fn fresh_queue_survives_a_stampede() {
    for _ in 0..40 {
        let sandbox = Sandbox::new(4, "");
        let mut children: Vec<_> = (0..20)
            .map(|_| {
                sandbox
                    .cmd(&["status"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in &mut children {
            let stderr = std::io::read_to_string(child.stderr.take().unwrap()).unwrap();
            assert!(wait(child).success(), "{stderr}");
        }
    }
}

#[test]
fn jobs_running_in_another_versions_queue_are_told_of() {
    let sandbox = Sandbox::new(4, &sh_task("lint", 1, "true"));
    // The queue of another layout, holding a job for a live loadmaster.
    let mut owner = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = owner.id();
    let start = loadmaster::process::ProcessId::of(pid)
        .unwrap()
        .unwrap()
        .start_time;
    std::fs::create_dir_all(&sandbox.state).unwrap();
    let other = rusqlite::Connection::open(sandbox.state.join("queue-v2.db")).unwrap();
    other
        .execute_batch(&format!(
            "CREATE TABLE jobs (id INTEGER PRIMARY KEY, task TEXT, state TEXT,
                 owner_pid INTEGER, owner_start INTEGER, child_pgid INTEGER,
                 child_start INTEGER);
             INSERT INTO jobs (task, state, owner_pid, owner_start)
                 VALUES ('e2e', 'running', {pid}, {start}), ('e2e', 'waiting', {pid}, {start});"
        ))
        .unwrap();
    drop(other);
    let told = "2 job(s) in the queue of another loadmaster version, which this one does not \
                count (queue-v2.db: running `e2e`, waiting `e2e`)";

    let out = stdout(&sandbox.run(&["status"]));
    assert!(out.contains(&format!("\n{told}\n")), "{out}");
    let json: serde_json::Value =
        serde_json::from_slice(&sandbox.run(&["status", "--json"]).stdout).unwrap();
    assert_eq!(
        json["other_queues"],
        serde_json::json!([{
            "file": "queue-v2.db",
            "readable": true,
            "running": ["e2e"],
            "waiting": ["e2e"],
            "parked": [],
        }])
    );
    let output = sandbox.run(&["run", "lint"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!(
            "loadmaster: warning: {told}; give every agent on this machine 0.12 or later\n"
        )),
        "{}",
        stderr(&output)
    );

    owner.kill().unwrap();
    owner.wait().unwrap();
    let out = stdout(&sandbox.run(&["status"]));
    assert!(!out.contains("another loadmaster version"), "{out}");
}

#[test]
fn status_as_json_says_what_the_table_says() {
    let probe = Sandbox::new(4, "");
    let tasks = sh_task("hold", 3, &probe.hold_script("hold")) + &sh_task("e2e", 4, "true");
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = probe;
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let mut waiter = sandbox.spawn(&["run", "e2e"]);
    wait_until("e2e to wait", || {
        sandbox
            .job("e2e")
            .is_some_and(|j| j.state == JobState::Waiting)
    });

    let output = sandbox.run(&["status", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["format"], 2);
    assert_eq!(status["capacity"], 4);
    assert_eq!(status["cores_in_use"], 3);
    let jobs = status["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0]["state"], "running");
    assert_eq!(jobs[0]["task"], "hold");
    assert_eq!(jobs[0]["cores"], 3);
    assert_eq!(jobs[0]["wait"], serde_json::Value::Null);
    assert_eq!(jobs[1]["state"], "waiting");
    // When each got there, as the database recorded it.
    let near = |job: &serde_json::Value, field: &str| {
        let since = job["state_since_ms"].as_u64().unwrap();
        let at = job[field].as_u64().unwrap();
        assert!(since.abs_diff(at) < 1_000, "{since} vs {at}");
    };
    near(&jobs[0], "started_at_ms");
    near(&jobs[1], "enqueued_at_ms");
    let why = &jobs[1]["wait"];
    assert_eq!(why["starts_now"], false);
    assert_eq!(why["ahead"], 0);
    assert_eq!(why["for"][0]["need"], "cores");
    assert_eq!(why["for"][0]["reason"], "in_use");
    assert_eq!(why["for"][0]["used"], 3);
    assert_eq!(why["for"][0]["limit"], 4);
    assert_eq!(why["for"][0]["held_by"], serde_json::json!([1]));
    assert_eq!(why["text"], "all 4 cores (3 in use)");

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    assert!(wait(&mut waiter).success());
}

#[test]
fn status_says_what_became_of_the_latest_checks_of_this_checkout() {
    let sandbox = Sandbox::new(
        4,
        &(sh_task("ok", 1, "true") + &sh_task("bad", 1, "exit 3")),
    );
    assert!(sandbox.run(&["run", "ok"]).status.success());
    assert_eq!(sandbox.run(&["run", "bad"]).status.code(), Some(3));
    let out = stdout(&sandbox.run(&["status"]));
    let lately = out
        .split("ended lately, from this checkout:\n")
        .nth(1)
        .unwrap_or("");
    let lines: Vec<&str> = lately.lines().collect();
    assert!(lines[0].starts_with("#2 `bad` failed in "), "{out}");
    assert!(lines[0].contains(" (exit 3), "), "{out}");
    assert!(lines[1].starts_with("#1 `ok` passed in "), "{out}");
    // As data too; from elsewhere, none.
    let json: serde_json::Value =
        serde_json::from_slice(&sandbox.run(&["status", "--json"]).stdout).unwrap();
    assert_eq!(json["recent"][0]["id"], 2);
    assert_eq!(json["recent"][0]["result"], "failed");
    assert_eq!(json["recent"][0]["exit_code"], 3);
    assert_eq!(json["recent"][1]["result"], "passed");
    let elsewhere = sandbox
        .cmd(&["status", "--json"])
        .current_dir(&sandbox.root)
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&elsewhere.stdout).unwrap();
    assert_eq!(json["recent"], serde_json::json!([]));
}
