//! `loadmaster cancel` stops chosen jobs, and only those: through their
//! loadmaster process, so they leave the queue as after any signal, and the
//! report leaves them out.

mod common;

use std::process::Stdio;

use common::*;
use loadmaster::queue::JobState;

/// `hold` runs until released, logging each SIGTERM to `term`; `test` needs
/// the whole capacity.
fn sandbox() -> Sandbox {
    let probe = Sandbox::new(4, "");
    let hold = format!(
        "trap 'echo term >> {}; exit 3' TERM; {}",
        q(&probe.path("term")),
        probe.hold_script("hold")
    );
    let tasks = sh_task("hold", 3, &hold) + &sh_task("test", 4, "true");
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    probe
}

fn wait_for_state(sandbox: &Sandbox, task: &str, state: JobState) {
    wait_until(&format!("`{task}` to be {state:?}"), || {
        sandbox.job(task).is_some_and(|j| j.state == state)
    });
}

#[test]
fn cancels_a_waiting_job() {
    let sandbox = sandbox();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let mut waiter = sandbox.spawn(&["run", "test"]);
    wait_for_state(&sandbox, "test", JobState::Waiting);

    let output = sandbox.run(&["cancel", "2"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("canceled #2 `test` (waiting for "),
        "{}",
        stdout(&output)
    );
    // Gone when cancel returns, and the run left the line as on a SIGTERM.
    assert!(sandbox.job("test").is_none());
    assert_eq!(wait(&mut waiter).code(), Some(143));
    assert_eq!(sandbox.job("hold").unwrap().state, JobState::Running);

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    let report = stdout(&sandbox.run(&["report"]));
    assert!(
        report.contains("left out 1 check(s) stopped with `loadmaster cancel`\n"),
        "{report}"
    );
    assert!(!report.contains("\ntest "), "{report}");
}

#[test]
fn cancels_a_running_job_through_its_loadmaster() {
    let sandbox = sandbox();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));

    let output = sandbox.run(&["cancel", "1"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("canceled #1 `hold` (running for "),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        wait(&mut holder).code(),
        Some(3),
        "the task's own exit code"
    );
    assert_eq!(
        sandbox.read("term"),
        "term\n",
        "one SIGTERM, forwarded once"
    );
    assert!(sandbox.snapshot().jobs.is_empty());
}

#[test]
fn kill_ends_a_task_that_ignores_sigterm() {
    let sandbox = Sandbox::new(4, "");
    let tasks = sh_task(
        "stubborn",
        1,
        &format!("trap '' TERM; {}", sandbox.hold_script("hold")),
    );
    std::fs::write(sandbox.repo.join(".loadmaster.toml"), tasks).unwrap();
    let mut run = sandbox.spawn(&["run", "stubborn"]);
    wait_until("the task to start", || sandbox.started("hold"));

    let output = sandbox.run(&["cancel", "--kill", "1"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(wait(&mut run).code(), Some(137));
    assert!(sandbox.snapshot().jobs.is_empty());
}

#[test]
fn here_cancels_only_this_checkouts_jobs() {
    let sandbox = sandbox();
    let other = sandbox.path("repo2");
    std::fs::create_dir_all(&other).unwrap();
    let hold2 = sandbox.hold_script("hold2");
    std::fs::write(other.join(".loadmaster.toml"), sh_task("hold", 1, &hold2)).unwrap();
    let mut theirs = sandbox
        .cmd(&["run", "hold"])
        .current_dir(&other)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the other checkout's task to start", || {
        sandbox.started("hold2")
    });
    let mut ours = sandbox.spawn(&["run", "hold"]);
    wait_until("this checkout's task to start", || sandbox.started("hold"));

    let output = sandbox.run(&["cancel", "--here"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("canceled #2 `hold`"),
        "{}",
        stdout(&output)
    );
    assert_eq!(wait(&mut ours).code(), Some(3));
    let left = sandbox.snapshot().jobs;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, 1, "the other checkout's job keeps running");

    sandbox.release("hold2");
    assert!(wait(&mut theirs).success());
    let output = sandbox.run(&["cancel", "--here"]);
    assert_eq!(stdout(&output), "no jobs to cancel\n");
}

#[test]
fn a_job_does_not_cancel_itself() {
    let probe = Sandbox::new(4, "");
    let tasks = sh_task(
        "sweep",
        1,
        &format!(
            "{bin} cancel --here; {bin} cancel $LOADMASTER_HELD",
            bin = q(std::path::Path::new(BIN))
        ),
    );
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let output = probe.run(&["run", "sweep"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "no jobs to cancel\nno jobs to cancel\n");
    assert!(
        stderr(&output).contains("loadmaster: not canceling job #1: this command runs in it"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn drops_a_kept_place_and_names_unknown_jobs() {
    let sandbox = sandbox();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let gave_up = sandbox.run(&["run", "--max-wait", "0", "test"]);
    assert_eq!(gave_up.status.code(), Some(75));
    assert_eq!(sandbox.job("test").unwrap().state, JobState::Parked);

    let output = sandbox.run(&["cancel", "2", "9"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "dropped the kept place of #2 `test`\n");
    assert!(stderr(&output).contains("loadmaster: job #9 is not in the queue"));
    assert!(sandbox.job("test").is_none());

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
}
