//! Acceptance criterion 11: after `--max-wait` gives up, running the same
//! command again resumes from the original place in line.

mod common;

use std::time::{Duration, Instant};

use common::*;
use loadmaster::queue::JobState;

fn sandbox(log: &std::path::Path) -> Sandbox {
    let probe = Sandbox::new(4, "");
    let tasks = sh_task("hold", 4, &probe.hold_script("hold"))
        + &sh_task("test", 4, &format!("echo \"test $*\" >> {}", q(log)))
        + &sh_task("lint", 4, &format!("echo lint >> {}", q(log)));
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    probe
}

#[test]
fn rerun_after_max_wait_resumes_the_original_place() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = sandbox(log.path());
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));

    let started = Instant::now();
    let gave_up = sandbox.run(&["run", "--max-wait", "1", "test", "--", "a"]);
    assert_eq!(gave_up.status.code(), Some(75), "{}", stderr(&gave_up));
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(
        stderr(&gave_up).contains(&format!(
            "loadmaster: next: run `loadmaster run test --max-wait 1 -- a` again from {} \
             within 120s: it resumes its place in line",
            std::fs::canonicalize(&sandbox.repo).unwrap().display()
        )),
        "{}",
        stderr(&gave_up)
    );
    let parked = sandbox.job("test").unwrap();
    assert_eq!(parked.state, JobState::Parked);
    let history = std::fs::read_to_string(sandbox.state.join("history.jsonl")).unwrap();
    let record: serde_json::Value = serde_json::from_str(history.lines().last().unwrap()).unwrap();
    assert_eq!(record["outcome"], "gave_up");
    assert_eq!(record["exit_code"], 75);
    assert!(record["wait_ms"].as_u64().unwrap() >= 1000);

    // Someone else gets in line behind the parked job.
    let mut lint = sandbox.spawn(&["run", "lint"]);
    wait_until("lint to get in line", || sandbox.job("lint").is_some());

    // Different arguments are a different request: no resume.
    let other = sandbox.run(&["run", "--max-wait", "0", "test", "--", "b"]);
    assert_eq!(other.status.code(), Some(75));
    assert!(!stderr(&other).contains("resumed"), "{}", stderr(&other));

    // The same command resumes the parked job, ahead of lint.
    let mut resumed = sandbox
        .cmd(&["run", "test", "--", "a"])
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(sandbox.path("resumed.err")).unwrap())
        .spawn()
        .unwrap();
    wait_until("the resume", || {
        sandbox
            .read("resumed.err")
            .contains("resumed its place in line")
    });
    let job = sandbox.job("test").unwrap();
    assert_eq!(job.id, parked.id, "kept the original place");
    assert_eq!(job.state, JobState::Waiting);

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    assert!(wait(&mut resumed).success());
    assert!(wait(&mut lint).success());
    let order: Vec<String> = std::fs::read_to_string(log.path())
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    // The resumed job ran before lint, which got in line after it was parked.
    // (The parked `-- b` job is behind lint and simply expires later.)
    assert_eq!(order, ["test a", "lint"]);
}

#[test]
fn max_wait_can_come_from_the_environment() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = sandbox(log.path());
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));

    let output = sandbox
        .cmd(&["run", "test"])
        .env("LOADMASTER_MAX_WAIT", "0")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(75), "{}", stderr(&output));

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
}

#[test]
fn an_empty_max_wait_in_the_environment_means_no_limit() {
    // So a script can turn it off for its steps without unsetting it.
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = sandbox(log.path());
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));

    let mut waiter = sandbox
        .cmd(&["run", "test"])
        .env("LOADMASTER_MAX_WAIT", "")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_until("a waiting job", || {
        sandbox
            .job("test")
            .is_some_and(|j| j.state == JobState::Waiting)
    });
    std::thread::sleep(Duration::from_millis(1500));
    assert!(waiter.try_wait().unwrap().is_none(), "still waiting");

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    assert!(wait(&mut waiter).success());
    assert_eq!(std::fs::read_to_string(log.path()).unwrap(), "test \n");
}

#[test]
fn a_blank_max_wait_in_the_environment_means_no_limit_too() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = sandbox(log.path());
    let output = sandbox
        .cmd(&["run", "test"])
        .env("LOADMASTER_MAX_WAIT", "  ")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn an_invalid_max_wait_in_the_environment_is_an_error() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = sandbox(log.path());
    let output = sandbox
        .cmd(&["run", "test"])
        .env("LOADMASTER_MAX_WAIT", "ten")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(125));
    assert!(
        stderr(&output).contains("LOADMASTER_MAX_WAIT=ten"),
        "{}",
        stderr(&output)
    );
    assert!(sandbox.snapshot().jobs.is_empty(), "nothing was queued");
}

#[test]
fn max_wait_does_not_apply_once_admitted() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = sandbox(log.path());
    let output = sandbox.run(&["run", "--max-wait", "0", "test", "--", "now"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(std::fs::read_to_string(log.path()).unwrap(), "test now\n");
}
