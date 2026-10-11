//! Acceptance criterion 2: while small jobs keep streaming in, a big job is
//! not overtaken by small jobs that arrived after it, and it does start.

mod common;

use std::process::Stdio;
use std::thread;
use std::time::Duration;

use common::*;
use loadmaster::queue::JobState;

#[test]
fn big_job_is_not_overtaken_by_later_small_jobs() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let probe = Sandbox::new(4, "");
    // `hold` keeps 3 of the 4 slots until released: small jobs (1) still flow
    // through the free slot, but the big job (4) has to wait.
    let tasks = sh_task("hold", 3, &probe.hold_script("hold"))
        + &sh_task(
            "small",
            1,
            &format!("echo \"start $1\" >> {}; sleep 0.2", q(log.path())),
        )
        + &sh_task(
            "big",
            4,
            &format!("echo \"start big\" >> {}; sleep 0.1", q(log.path())),
        );
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = probe;
    // The label is passed as an argument, so the job is recognizable in the queue.
    let small = |label: &str| {
        sandbox
            .cmd(&["run", "small", "--", label])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let read_log = || std::fs::read_to_string(log.path()).unwrap();
    let got_in_line = |label: &str| {
        read_log().contains(&format!("start {label}\n"))
            || sandbox.snapshot().jobs.iter().any(|j| j.args == [label])
    };

    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("hold to start", || sandbox.started("hold"));
    // Every earlier small job is in line before the big one arrives, while
    // they are still flowing through the free slot.
    let mut before: Vec<_> = (0..5)
        .map(|i| {
            let label = format!("before-{i}");
            let child = small(&label);
            wait_until("the small job to get in line", || got_in_line(&label));
            child
        })
        .collect();
    assert!(
        before.iter_mut().any(|c| c.try_wait().unwrap().is_none()),
        "small jobs should still be flowing"
    );

    let mut big = sandbox.spawn(&["run", "big"]);
    wait_until("the big job to wait", || {
        sandbox
            .job("big")
            .is_some_and(|j| j.state == JobState::Waiting)
    });
    let mut after: Vec<_> = (0..5).map(|i| small(&format!("after-{i}"))).collect();

    // Once the earlier small jobs are done, a slot is free and a later small
    // job would fit; give an overtaking implementation every chance to use it.
    for child in &mut before {
        assert!(wait(child).success());
    }
    thread::sleep(Duration::from_millis(500));
    assert!(
        !read_log().contains("start after-"),
        "a small job overtook the waiting big job:\n{}",
        read_log()
    );

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    assert!(wait(&mut big).success(), "the big job must start");
    for child in &mut after {
        assert!(wait(child).success());
    }
    let starts: Vec<String> = read_log()
        .lines()
        .map(|l| l.trim_start_matches("start ").to_owned())
        .collect();
    let big_at = starts.iter().position(|s| s == "big").unwrap();
    let after_at: Vec<usize> = starts
        .iter()
        .enumerate()
        .filter(|(_, s)| s.starts_with("after-"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(after_at.len(), 5);
    assert!(after_at.iter().all(|&i| i > big_at), "{starts:?}");
}
