//! Acceptance criterion 5: the task's exit code comes back unchanged, and its
//! output, arguments, working directory and environment pass through.

mod common;

use common::*;

#[test]
fn exit_code_is_passed_through() {
    for code in [0, 1, 3, 42, 255] {
        let sandbox = Sandbox::new(4, &sh_task("t", 1, &format!("exit {code}")));
        let output = sandbox.run(&["run", "t"]);
        assert_eq!(output.status.code(), Some(code));
    }
}

#[test]
fn output_and_arguments_pass_through() {
    let sandbox = Sandbox::new(
        4,
        &(sh_task("io", 1, "echo out; echo err >&2") + &task("args", 1, &["printf", "[%s]"])),
    );
    let output = sandbox.run(&["run", "io"]);
    assert_eq!(stdout(&output), "out\n");
    assert_eq!(stderr(&output), "err\n");

    // No shell in between: arguments arrive exactly as given.
    let output = sandbox.run(&["run", "args", "--", "a b", "$HOME", "'q'", ""]);
    assert_eq!(stdout(&output), "[a b][$HOME]['q'][]");
}

#[test]
fn task_runs_in_the_config_directory_with_loadmaster_env() {
    let sandbox = Sandbox::new(
        4,
        &sh_task(
            "env",
            3,
            "pwd -P; echo \"weight=$LOADMASTER_WEIGHT held=${LOADMASTER_HELD:+set}\"",
        ),
    );
    let nested = sandbox.repo.join("packages/app");
    std::fs::create_dir_all(&nested).unwrap();
    let mut cmd = sandbox.cmd(&["run", "env"]);
    cmd.current_dir(&nested);
    let output = output(cmd);
    let repo = sandbox.repo.canonicalize().unwrap();
    assert_eq!(
        stdout(&output),
        format!("{}\nweight=3 held=set\n", repo.display())
    );
}

#[test]
fn missing_program_exits_127() {
    let sandbox = Sandbox::new(4, &task("t", 1, &["loadmaster-test-no-such-program"]));
    let output = sandbox.run(&["run", "t"]);
    assert_eq!(output.status.code(), Some(127));
    assert!(
        stderr(&output).contains("cannot run `loadmaster-test-no-such-program`"),
        "{}",
        stderr(&output)
    );
    assert!(sandbox.snapshot().jobs.is_empty(), "the slot was released");
}

#[test]
fn history_records_each_job() {
    let sandbox = Sandbox::new(4, &sh_task("t", 2, "sleep 0.1; exit 3"));
    assert_eq!(sandbox.run(&["run", "t", "--", "x"]).status.code(), Some(3));
    let history = std::fs::read_to_string(sandbox.state.join("history.jsonl")).unwrap();
    let lines: Vec<&str> = history.lines().collect();
    assert_eq!(lines.len(), 1);
    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(record["task"], "t");
    assert_eq!(record["weight"], 2);
    assert_eq!(record["args"], serde_json::json!(["x"]));
    assert_eq!(record["outcome"], "exited");
    assert_eq!(record["exit_code"], 3);
    assert!(record["run_ms"].as_u64().unwrap() >= 100);
    assert!(record["wait_ms"].is_u64());
    assert!(record["queued_at"].as_u64().unwrap() > 1_600_000_000_000);
}
