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
