//! Acceptance criteria 4 and 10: nested `loadmaster run` runs under the
//! holder's slot without deadlocking, and a stale, bogus or leaked
//! LOADMASTER_HELD, or a nested task that does not fit in the holder's slot,
//! is rejected instead of silently bypassing the queue.

mod common;

use common::*;
use loadmaster::queue::JobState;

fn sandbox(ran_marker: &str) -> Sandbox {
    let probe = Sandbox::new(4, "");
    // `outer` takes the whole capacity, then calls `loadmaster run inner`,
    // which also weighs 4: without nesting support this would deadlock.
    let tasks = task("outer", 4, &[BIN, "run", "inner", "--", "from-outer"])
        + &sh_task(
            "inner",
            4,
            &format!(
                "echo ran > {}; echo \"inner held=$LOADMASTER_HELD weight=$LOADMASTER_WEIGHT args=$*\"",
                q(&probe.path(ran_marker))
            ),
        )
        + &sh_task("hold", 4, &probe.hold_script("hold"));
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    probe
}

#[test]
fn nested_run_does_not_deadlock() {
    let sandbox = sandbox("inner.ran");
    let output = sandbox.run(&["run", "outer"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    let held: i64 = out
        .trim()
        .strip_prefix("inner held=")
        .and_then(|rest| rest.split(' ').next())
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("unexpected output: {out:?}"));
    assert!(held > 0);
    assert!(out.ends_with("weight=4 args=from-outer\n"), "{out}");
    assert!(sandbox.snapshot().jobs.is_empty());
}

#[test]
fn stale_or_bogus_held_is_rejected() {
    let sandbox = sandbox("inner.ran");

    // A job that finished: its ID is dead.
    assert!(sandbox.run(&["run", "outer"]).status.success());
    std::fs::remove_file(sandbox.path("inner.ran")).unwrap();
    let finished = sandbox.run(&["status"]); // sanity: queue is empty
    assert!(stdout(&finished).contains("no jobs"));

    // A job that is waiting, not running, is not a holder either.
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let mut waiter = sandbox.spawn(&["run", "inner"]);
    wait_until("a waiting job", || {
        sandbox
            .job("inner")
            .is_some_and(|j| j.state == JobState::Waiting)
    });
    let waiting_id = sandbox.job("inner").unwrap().id.to_string();
    let running_id = sandbox.job("hold").unwrap().id;
    let assert_rejected = |value: &str| {
        let output = sandbox
            .cmd(&["run", "inner"])
            .env("LOADMASTER_HELD", value)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(125), "HELD={value:?}");
        assert!(
            stderr(&output).contains("does not refer to a running job"),
            "HELD={value:?}: {}",
            stderr(&output)
        );
        assert!(
            !sandbox.path("inner.ran").exists(),
            "HELD={value:?} ran the task"
        );
    };
    assert_rejected(&waiting_id);
    signal(waiter.id(), libc::SIGTERM);
    wait(&mut waiter);

    for value in [
        "1",         // the finished outer job
        "999999",    // never existed
        &waiting_id, // gone now
        "not-a-number",
        " ",
    ] {
        assert_rejected(value);
    }

    // A live holder is accepted, even while the capacity is full.
    let output = sandbox
        .cmd(&["run", "inner"])
        .env("LOADMASTER_HELD", running_id.to_string())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(sandbox.path("inner.ran").exists());

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
}

#[test]
fn nested_run_finds_the_queue_with_a_relative_state_dir() {
    let sandbox = sandbox("inner.ran");
    let sub = sandbox.repo.join("packages/app");
    std::fs::create_dir_all(&sub).unwrap();
    // Relative to `packages/app`; the task itself runs in the repo root.
    let mut cmd = sandbox.cmd(&["run", "outer"]);
    cmd.current_dir(&sub)
        .env("LOADMASTER_STATE_DIR", "../../../state");
    let output = output(cmd);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(sandbox.path("inner.ran").exists());
}

fn with_uses(task: String, uses: &str) -> String {
    task.replacen("cmd = ", &format!("uses = {uses}\ncmd = "), 1)
}

#[test]
fn a_nested_run_must_fit_in_the_slot_it_runs_in() {
    let sandbox = Sandbox::new(8, "");
    let tasks = "[resources.db]\ncapacity = 1\n\n".to_owned()
        + &task("small-outer", 2, &[BIN, "run", "heavy"])
        + &task("no-db-outer", 4, &[BIN, "run", "needs-db"])
        + &with_uses(task("db-outer", 4, &[BIN, "run", "needs-db"]), "{ db = 1 }")
        + &sh_task("heavy", 4, "echo ran")
        + &with_uses(sh_task("needs-db", 1, "echo ran"), "{ db = 1 }");
    std::fs::write(sandbox.repo.join(".loadmaster.toml"), tasks).unwrap();

    let too_heavy = sandbox.run(&["run", "small-outer"]);
    assert_eq!(too_heavy.status.code(), Some(125));
    let err = stderr(&too_heavy);
    assert!(
        err.contains("`heavy` has weight 4, more than the 2 of job #"),
        "{err}"
    );
    assert!(
        err.contains("give `small-outer` a weight of at least 4"),
        "{err}"
    );
    assert!(stdout(&too_heavy).is_empty());

    let no_db = sandbox.run(&["run", "no-db-outer"]);
    assert_eq!(no_db.status.code(), Some(125));
    let err = stderr(&no_db);
    assert!(err.contains("`needs-db` uses 1 of `db`"), "{err}");
    assert!(
        err.contains("add `db = 1` to the `uses` of `no-db-outer`"),
        "{err}"
    );

    let fits = sandbox.run(&["run", "db-outer"]);
    assert!(fits.status.success(), "{}", stderr(&fits));
    assert_eq!(stdout(&fits), "ran\n");
}

fn with_stop(task: String, signal: &str) -> String {
    task.replacen("cmd = ", &format!("stop_signal = \"{signal}\"\ncmd = "), 1)
}

#[test]
fn a_nested_run_must_be_stopped_as_its_holder_is() {
    // It runs in the holder's process group, which gets the holder's stop
    // signal.
    let sandbox = Sandbox::new(8, "");
    let tasks = task("term-outer", 4, &[BIN, "run", "e2e"])
        + &with_stop(task("int-outer", 4, &[BIN, "run", "e2e"]), "SIGINT")
        + &with_stop(task("int-for-default", 4, &[BIN, "run", "lint"]), "SIGINT")
        + &with_stop(sh_task("e2e", 1, "echo ran"), "SIGINT")
        + &sh_task("lint", 1, "echo ran");
    std::fs::write(sandbox.repo.join(".loadmaster.toml"), tasks).unwrap();

    let refused = sandbox.run(&["run", "term-outer"]);
    assert_eq!(refused.status.code(), Some(125));
    let err = stderr(&refused);
    assert!(
        err.contains("`e2e` stops on SIGINT, but job #")
            && err.contains(
                "(`term-outer`) that it would run in is stopped with SIGTERM: set \
                 `stop_signal = \"SIGINT\"` for `term-outer`, or run `e2e` on its own"
            ),
        "{err}"
    );
    for fits in ["int-outer", "int-for-default"] {
        let output = sandbox.run(&["run", fits]);
        assert!(output.status.success(), "{fits}: {}", stderr(&output));
        assert_eq!(stdout(&output), "ran\n");
    }
}

#[test]
fn a_nested_run_from_another_checkout_is_rejected() {
    // A LOADMASTER_HELD that leaked into another worktree.
    let sandbox = sandbox("inner.ran");
    let other = sandbox.path("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::copy(
        sandbox.repo.join(".loadmaster.toml"),
        other.join(".loadmaster.toml"),
    )
    .unwrap();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let id = sandbox.job("hold").unwrap().id.to_string();

    let mut cmd = sandbox.cmd(&["run", "inner"]);
    cmd.current_dir(&other).env("LOADMASTER_HELD", &id);
    let output = output(cmd);
    assert_eq!(output.status.code(), Some(125));
    assert!(
        stderr(&output).contains("was started from another checkout"),
        "{}",
        stderr(&output)
    );
    assert!(!sandbox.path("inner.ran").exists());

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
}

#[test]
fn a_relative_machine_config_means_the_same_file_in_a_nested_run() {
    // Started from `repo/sub` with LOADMASTER_CONFIG=../machine.toml: the
    // outer run reads `repo/machine.toml` (capacity 4) and caps the weight
    // of 8 at 4. The task runs in `repo`, where the same relative path names
    // the sandbox's machine.toml (capacity 8): the nested run must judge by
    // the same capacity, and fit.
    let sandbox = Sandbox::new(8, "");
    let tasks = task("outer", 8, &[BIN, "run", "inner"]) + &sh_task("inner", 8, "echo ran");
    std::fs::write(sandbox.repo.join(".loadmaster.toml"), tasks).unwrap();
    std::fs::write(
        sandbox.repo.join("machine.toml"),
        format!("capacity = 4\n{STATIC_CPU}"),
    )
    .unwrap();
    let sub = sandbox.repo.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let mut cmd = sandbox.cmd(&["run", "outer"]);
    cmd.current_dir(&sub)
        .env("LOADMASTER_CONFIG", "../machine.toml");
    let output = output(cmd);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "ran\n");
}
