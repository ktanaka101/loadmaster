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
    assert!(out.starts_with("0/6 in use (capacity set in "), "{out}");
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
    assert!(out.starts_with("3/4 in use"), "{out}");
    let rows: Vec<&str> = out
        .lines()
        .filter(|l| matches!(l.split_whitespace().nth(1), Some("running" | "waiting")))
        .collect();
    assert_eq!(rows.len(), 2, "{out}");
    assert!(
        rows[0].contains("running") && rows[0].contains("hold"),
        "{out}"
    );
    assert!(
        rows[1].contains("waiting") && rows[1].contains("e2e"),
        "{out}"
    );
    assert!(out.contains(&sandbox.repo.display().to_string()), "{out}");
    assert!(
        out.ends_with("why jobs wait:\n  #2 `e2e` waits for all 4 cores (3 in use)\n"),
        "{out}"
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
            "loadmaster: waiting to run `test` (weight 4, next in line) for all 4 cores (3 in use)",
        )
    });
    sandbox.release("hold");
    assert!(wait(&mut waiter).success());
    assert!(
        sandbox
            .read("waiter.err")
            .contains("loadmaster: starting `test` after waiting"),
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
    let told = "1 job(s) run in the queue of another loadmaster version, which this one does \
                not count (queue-v2.db: `e2e`)";

    let out = stdout(&sandbox.run(&["status"]));
    assert!(out.contains(&format!("\n{told}\n")), "{out}");
    let output = sandbox.run(&["run", "lint"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!(
            "loadmaster: warning: {told}; give every agent on this machine the same version\n"
        )),
        "{}",
        stderr(&output)
    );

    owner.kill().unwrap();
    owner.wait().unwrap();
    let out = stdout(&sandbox.run(&["status"]));
    assert!(!out.contains("another loadmaster version"), "{out}");
}
