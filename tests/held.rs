//! `loadmaster held` tells a script whether it runs inside a loadmaster job
//! started from this checkout, so a tool can refuse to run outside the queue
//! whatever started it.

mod common;

use std::path::Path;
use std::process::Output;

use common::*;
use loadmaster::queue::JobState;

const NOT_HELD: i32 = 1;

/// `check` prints the exit code of `loadmaster held`, run from the directory
/// given as its argument (relative to the repo); `outer` runs `check` nested.
fn sandbox() -> Sandbox {
    let sandbox = Sandbox::new(4, "");
    let tasks = sh_task(
        "check",
        1,
        &format!(
            "cd \"${{1:-.}}\" && {}; echo \"held=$?\"",
            held_cmd()
        ),
    ) + &task("outer", 1, &[BIN, "run", "check"])
        + &sh_task("hold", 1, &sandbox.hold_script("hold"))
        // Does not fit next to `hold`, so it waits.
        + &sh_task("big", 4, "true");
    std::fs::write(sandbox.repo.join(".loadmaster.toml"), tasks).unwrap();
    std::fs::create_dir_all(sandbox.repo.join("packages/app")).unwrap();
    sandbox
}

fn held_cmd() -> String {
    format!("{} held", q(Path::new(BIN)))
}

/// Runs `loadmaster held` outside any task, with `LOADMASTER_HELD` set to
/// `value`, from `dir`.
fn held_with(sandbox: &Sandbox, value: &str, dir: &Path) -> Output {
    let mut cmd = sandbox.cmd(&["held"]);
    cmd.env("LOADMASTER_HELD", value).current_dir(dir);
    output(cmd)
}

#[test]
fn a_task_is_held() {
    let sandbox = sandbox();
    for args in [
        &["run", "check"][..],
        &["run", "check", "--", "packages/app"],
    ] {
        let output = sandbox.run(args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        assert_eq!(stdout(&output), "held=0\n", "{args:?}: {}", stderr(&output));
    }
    // A nested run holds the outer job's slot, and is held too.
    let output = sandbox.run(&["run", "outer"]);
    assert_eq!(stdout(&output), "held=0\n", "{}", stderr(&output));
}

#[test]
fn outside_a_job_is_not_held() {
    let sandbox = sandbox();
    let output = sandbox.run(&["held"]);
    assert_eq!(output.status.code(), Some(NOT_HELD));
    let err = stderr(&output);
    assert!(err.contains("not running through loadmaster"), "{err}");
    assert!(err.contains("LOADMASTER_HELD is not set"), "{err}");
    assert!(stdout(&output).is_empty());
}

#[test]
fn a_stale_bogus_or_waiting_id_is_not_held() {
    let sandbox = sandbox();
    assert!(sandbox.run(&["run", "check"]).status.success());
    let finished = "1";

    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let mut waiter = sandbox.spawn(&["run", "big"]);
    wait_until("a waiting job", || {
        sandbox
            .job("big")
            .is_some_and(|j| j.state == JobState::Waiting)
    });
    let waiting = sandbox.job("big").unwrap().id.to_string();

    for value in [finished, "999999", &waiting, "not-a-number", " "] {
        let output = held_with(&sandbox, value, &sandbox.repo);
        assert_eq!(output.status.code(), Some(NOT_HELD), "HELD={value:?}");
        let err = stderr(&output);
        assert!(
            err.contains("does not refer to a running job"),
            "HELD={value:?}: {err}"
        );
    }

    signal(waiter.id(), libc::SIGTERM);
    wait(&mut waiter);
    sandbox.release("hold");
    assert!(wait(&mut holder).success());
}

#[test]
fn a_job_of_another_checkout_is_not_held() {
    let sandbox = sandbox();
    let other = sandbox.path("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join(".loadmaster.toml"), "").unwrap();
    let link = sandbox.path("link");
    std::os::unix::fs::symlink(&sandbox.repo, &link).unwrap();

    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let id = sandbox.job("hold").unwrap().id.to_string();

    let output = held_with(&sandbox, &id, &other);
    assert_eq!(output.status.code(), Some(NOT_HELD));
    let err = stderr(&output);
    assert!(err.contains("another checkout"), "{err}");
    assert!(err.contains(&format!("job #{id}")), "{err}");

    // The same job is accepted in its own checkout, also through a symlink
    // and from a subdirectory.
    for dir in [
        sandbox.repo.clone(),
        link.clone(),
        sandbox.repo.join("packages/app"),
    ] {
        let output = held_with(&sandbox, &id, &dir);
        assert!(
            output.status.success(),
            "{}: {}",
            dir.display(),
            stderr(&output)
        );
    }

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
}

#[test]
fn no_repo_config_is_an_error() {
    let sandbox = sandbox();
    let empty = sandbox.path("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let output = held_with(&sandbox, "1", &empty);
    assert_eq!(output.status.code(), Some(125));
    assert!(
        stderr(&output).contains("no .loadmaster.toml found"),
        "{}",
        stderr(&output)
    );
}
