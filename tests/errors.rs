//! Acceptance criterion 6: unknown tasks, weights above the capacity and broken
//! config files fail immediately with a clear message, without queueing.

mod common;

use std::time::{Duration, Instant};

use common::*;

/// Runs `loadmaster run` while the capacity is fully taken, so any attempt to
/// queue would hang, and checks it fails fast with exit code 125.
fn assert_fails_fast(sandbox: &Sandbox, args: &[&str], expected: &[&str]) {
    let started = Instant::now();
    let output = sandbox.run(args);
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(125), "{args:?}: {message}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{args:?} took {:?}",
        started.elapsed()
    );
    for text in expected {
        assert!(
            message.contains(text),
            "{args:?}: expected {text:?} in:\n{message}"
        );
    }
    assert!(message.starts_with("loadmaster: error: "), "{message}");
}

fn busy_sandbox() -> (Sandbox, std::process::Child) {
    let probe = Sandbox::new(4, "");
    let tasks = sh_task("hold", 4, &probe.hold_script("hold"))
        + &sh_task("lint", 1, "true")
        + &sh_task("e2e", 8, "true");
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let holder = probe.spawn(&["run", "hold"]);
    wait_until("the task to start", || probe.started("hold"));
    (probe, holder)
}

#[test]
fn unknown_task_lists_available_tasks() {
    let (sandbox, mut holder) = busy_sandbox();
    assert_fails_fast(
        &sandbox,
        &["run", "lnit"],
        &[
            "unknown task `lnit`",
            "available tasks: e2e (weight 8), hold (weight 4), lint (weight 1)",
        ],
    );
    assert_eq!(sandbox.snapshot().jobs.len(), 1, "nothing was queued");
    sandbox.release("hold");
    wait(&mut holder);
}

#[test]
fn weight_above_capacity_is_rejected() {
    let (sandbox, mut holder) = busy_sandbox();
    assert_fails_fast(
        &sandbox,
        &["run", "e2e"],
        &[
            "task `e2e` has weight 8",
            "capacity is 4",
            "machine.toml",
            "could never start",
        ],
    );
    assert_eq!(sandbox.snapshot().jobs.len(), 1, "nothing was queued");
    sandbox.release("hold");
    wait(&mut holder);
}

#[test]
fn broken_repo_config_is_rejected() {
    let (sandbox, mut holder) = busy_sandbox();
    let config = sandbox.repo.join(".loadmaster.toml");
    let original = std::fs::read_to_string(&config).unwrap();
    for (text, expected) in [
        ("[tasks.lint\nweight = 1", "TOML parse error"),
        (
            "[tasks.lint]\nweight = 1\ncmd = \"npm test\"\n",
            "must be an array of strings",
        ),
        (
            "[tasks.lint]\nweight = 0\ncmd = [\"true\"]\n",
            "weight must be at least 1",
        ),
        (
            "[tasks.lint]\nwieght = 1\ncmd = [\"true\"]\n",
            "unknown field `wieght`",
        ),
        ("[tasks.lint]\ncmd = [\"true\"]\n", "missing field `weight`"),
    ] {
        std::fs::write(&config, text).unwrap();
        assert_fails_fast(&sandbox, &["run", "lint"], &[expected, ".loadmaster.toml"]);
    }
    std::fs::write(&config, original).unwrap();
    sandbox.release("hold");
    wait(&mut holder);
}

#[test]
fn missing_repo_config_is_rejected() {
    let sandbox = Sandbox::new(4, "");
    let outside = sandbox.path("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let mut cmd = sandbox.cmd(&["run", "lint"]);
    cmd.current_dir(&outside);
    let output = output(cmd);
    assert_eq!(output.status.code(), Some(125));
    assert!(
        stderr(&output).contains("no .loadmaster.toml found in"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn broken_machine_config_is_rejected() {
    let (sandbox, mut holder) = busy_sandbox();
    for (text, expected) in [
        ("capacity = 0\n", "capacity must be at least 1"),
        ("capacity = \"eight\"\n", "invalid type"),
        ("capasity = 8\n", "unknown field `capasity`"),
    ] {
        std::fs::write(&sandbox.machine_config, text).unwrap();
        assert_fails_fast(&sandbox, &["run", "lint"], &[expected, "machine.toml"]);
    }
    std::fs::write(&sandbox.machine_config, "capacity = 4\n").unwrap();
    sandbox.release("hold");
    wait(&mut holder);
}

#[test]
fn usage_errors_exit_125() {
    let sandbox = Sandbox::new(4, &sh_task("lint", 1, "true"));
    let output = sandbox.run(&["run"]);
    assert_eq!(output.status.code(), Some(125));
    let output = sandbox.run(&["run", "lint", "extra"]);
    assert_eq!(output.status.code(), Some(125), "args need `--`");
    assert!(sandbox.run(&["--help"]).status.success());
}
