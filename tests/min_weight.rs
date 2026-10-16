//! A task that declares `min_weight` may start with fewer cores than its
//! weight, where that fits beside what runs and waits ahead of it; the task
//! learns how many from LOADMASTER_WEIGHT. One that does not, starts with its
//! weight.

mod common;

use common::*;
use loadmaster::queue::JobState;

fn with_least(task: String, least: u32) -> String {
    task.replacen("cmd = ", &format!("min_weight = {least}\ncmd = "), 1)
}

#[test]
fn a_task_that_may_start_with_fewer_cores_takes_what_is_left() {
    let probe = Sandbox::new(4, "");
    let tasks = sh_task("hold", 3, &probe.hold_script("hold"))
        + &with_least(sh_task("some", 4, "echo $LOADMASTER_WEIGHT"), 1)
        + &sh_task("all", 4, "echo $LOADMASTER_WEIGHT");
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = probe;
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));

    // One core is left: `some` takes it, `all` waits for its four.
    let output = sandbox.run(&["run", "some"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "1");
    assert!(
        stderr(&output).contains("starting `some` with 1 of its 4 cores, the most that fit now"),
        "{}",
        stderr(&output)
    );
    let output = sandbox.run(&["run", "all", "--max-wait", "0"]);
    assert_eq!(output.status.code(), Some(75), "{}", stderr(&output));

    // It says so, as data too.
    let mut waiter = sandbox.spawn(&["run", "all"]);
    wait_until("`all` to wait", || {
        sandbox
            .job("all")
            .is_some_and(|j| j.state == JobState::Waiting)
    });
    let status: serde_json::Value =
        serde_json::from_slice(&sandbox.run(&["status", "--json"]).stdout).unwrap();
    assert_eq!(status["protocol"], 2);
    assert_eq!(status["moving"], serde_json::Value::Null);
    let jobs = status["jobs"].as_array().unwrap();
    let all = jobs.iter().find(|j| j["task"] == "all").unwrap();
    assert_eq!(all["min_weight"], 4);
    assert_eq!(all["wait"]["for"][0]["amount"], 4);

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    assert!(wait(&mut waiter).success());
}

#[test]
fn a_nested_run_fits_in_what_its_holder_got() {
    let probe = Sandbox::new(4, "");
    let nested = |inner: &str| format!("{BIN} run {inner}");
    let tasks = sh_task("hold", 2, &probe.hold_script("hold"))
        + &with_least(sh_task("outer", 4, &nested("fixed")), 1)
        + &with_least(sh_task("outer2", 4, &nested("some")), 1)
        + &sh_task("outer3", 4, &nested("wide"))
        + &with_least(sh_task("wide", 8, "echo $LOADMASTER_WEIGHT"), 6)
        + &sh_task("fixed", 4, "echo $LOADMASTER_WEIGHT")
        + &with_least(sh_task("some", 4, "echo $LOADMASTER_WEIGHT"), 2);
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = probe;
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));

    // `outer` starts with the 2 cores left: `fixed` needs its 4.
    let output = sandbox.run(&["run", "outer"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output)
            .contains("`fixed` has weight 4, more than the 2 cores (of its weight 4) that started"),
        "{}",
        stderr(&output)
    );
    // `some` may start with 2: it runs in them.
    let output = sandbox.run(&["run", "outer2"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "2");

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    // Like its weight, a least above the capacity counts as all of it: on
    // its own and nested, `wide` runs with the whole machine.
    let output = sandbox.run(&["run", "wide"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "4");
    assert!(
        stderr(&output).contains(
            "`wide` has weight 8 and min_weight 6, above this machine's capacity of 4, so both \
             count as 4"
        ),
        "{}",
        stderr(&output)
    );
    let output = sandbox.run(&["run", "outer3"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "4");
}
