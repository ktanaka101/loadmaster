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

    // Named twice, signalled once.
    let output = sandbox.run(&["cancel", "1", "1"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("canceled #1 `hold` (running for "),
        "{}",
        stdout(&output)
    );
    assert_eq!(stdout(&output).lines().count(), 1, "{}", stdout(&output));
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

#[test]
fn continues_a_stopped_loadmaster_whose_task_has_ended() {
    let sandbox = sandbox();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the child group to be recorded", || {
        sandbox.job("hold").is_some_and(|j| j.child.is_some())
    });
    let group = sandbox.job("hold").unwrap().child.unwrap();
    // Ctrl-Z on loadmaster; the task then ends, but nobody frees its slot.
    signal(holder.id(), libc::SIGSTOP);
    sandbox.release("hold");
    wait_until("the task to end", || !group.is_alive());

    let output = sandbox.run(&["cancel", "--kill", "1"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("canceled #1 `hold`"),
        "{}",
        stdout(&output)
    );
    assert!(wait(&mut holder).success(), "the task had passed");
    assert!(sandbox.snapshot().jobs.is_empty());
}

#[test]
fn a_waiting_job_leaves_on_request_without_a_signal() {
    // Its loadmaster ignores SIGTERM, as under `nohup`-like setups, so only
    // the request can make it leave.
    let sandbox = sandbox();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let mut cmd = sandbox.cmd(&["run", "test"]);
    cmd.stdout(Stdio::null())
        .stderr(std::fs::File::create(sandbox.path("test.err")).unwrap());
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut waiter = cmd.spawn().unwrap();
    wait_for_state(&sandbox, "test", JobState::Waiting);

    let output = sandbox.run(&["cancel", "2"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(wait(&mut waiter).code(), Some(143));
    assert!(
        sandbox
            .read("test.err")
            .contains("`test` was canceled with `loadmaster cancel` while it waited"),
        "{}",
        sandbox.read("test.err")
    );
    sandbox.release("hold");
    assert!(wait(&mut holder).success());
}

/// `cancel` run where it may not signal processes it did not start, as in
/// the macOS sandboxes of agents such as Codex.
#[cfg(target_os = "macos")]
fn cancel_in_sandbox(sandbox: &Sandbox, args: &[&str]) -> std::process::Output {
    let mut cmd = std::process::Command::new("sandbox-exec");
    cmd.args([
        "-p",
        // As Codex's sandbox: signals only to processes in the same sandbox.
        "(version 1)(allow default)(deny signal)(allow signal (target same-sandbox))",
        BIN,
        "cancel",
    ])
    .args(args)
    .current_dir(&sandbox.repo)
    .env("LOADMASTER_CONFIG", &sandbox.machine_config)
    .env("LOADMASTER_STATE_DIR", &sandbox.state)
    .env_remove("LOADMASTER_HELD");
    output(cmd)
}

#[cfg(target_os = "macos")]
#[test]
fn cancels_where_it_may_not_signal() {
    let sandbox = sandbox();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));

    let output = cancel_in_sandbox(&sandbox, &["--here"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).starts_with("canceled #1 `hold` (running for "));
    assert_eq!(wait(&mut holder).code(), Some(3));
    assert_eq!(sandbox.read("term"), "term\n");
}

#[cfg(target_os = "macos")]
#[test]
fn says_why_when_it_must_signal_but_may_not() {
    // A stopped loadmaster takes no request; the signal that would replace
    // it is not allowed.
    let sandbox = sandbox();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the child group to be recorded", || {
        sandbox.job("hold").is_some_and(|j| j.child.is_some())
    });
    signal(holder.id(), libc::SIGSTOP);

    let output = cancel_in_sandbox(&sandbox, &["1"]);
    assert_eq!(output.status.code(), Some(125));
    let err = stderr(&output);
    assert!(
        err.contains("cannot stop job #1: its loadmaster (process ")
            && err.contains("did not take the request, as a version before 0.6 or a stopped one does not")
            && err.contains("in an agent's sandbox, a command may not signal processes that another command started"),
        "{err}"
    );

    signal(holder.id(), libc::SIGCONT);
    sandbox.release("hold");
    assert!(wait(&mut holder).success());
}

#[test]
fn a_signal_the_task_survives_does_not_swallow_a_cancel() {
    // SIGHUP reloads the task, as for a daemon; a cancel afterwards must
    // still send its SIGTERM.
    let probe = Sandbox::new(4, "");
    let script = format!(
        "trap 'echo hup >> {log}' HUP; trap 'echo term >> {log}; exit 3' TERM; {}",
        probe.hold_script("hold"),
        log = q(&probe.path("signals"))
    );
    std::fs::write(
        probe.repo.join(".loadmaster.toml"),
        sh_task("hold", 1, &script),
    )
    .unwrap();
    let sandbox = probe;
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    signal(holder.id(), libc::SIGHUP);
    wait_until("the task to get SIGHUP", || {
        sandbox.read("signals").contains("hup")
    });

    let output = sandbox.run(&["cancel", "1"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(wait(&mut holder).code(), Some(3));
    assert_eq!(sandbox.read("signals"), "hup\nterm\n");
}

#[test]
fn a_second_cancel_does_not_signal_a_job_already_stopping() {
    // A member of the task's group logs every SIGTERM it gets; the task's
    // clean-up takes 4 seconds, longer than a cancel waits for its request
    // to be taken.
    let probe = Sandbox::new(4, "");
    let (log, done) = (q(&probe.path("signals")), q(&probe.path("done")));
    let script = format!(
        "sh -c 'trap \"echo term >> {log}\" TERM; while [ ! -e {done} ]; do sleep 0.05; done' & \
         c=$!; \
         trap 'i=0; while [ $i -lt 80 ]; do sleep 0.05; i=$((i+1)); done; touch {done}; wait $c; exit 3' TERM; \
         {}",
        probe.hold_script("hold")
    );
    std::fs::write(
        probe.repo.join(".loadmaster.toml"),
        sh_task("hold", 1, &script),
    )
    .unwrap();
    let sandbox = probe;
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let mut first = sandbox.spawn(&["cancel", "1"]);
    wait_until("the clean-up to start", || {
        sandbox.read("signals").contains("term")
    });

    // A stopped loadmaster takes no request: a second cancel must not fall
    // back to signalling it, since the job is stopping already.
    signal(holder.id(), libc::SIGSTOP);
    let mut second = sandbox.spawn(&["cancel", "1"]);
    std::thread::sleep(std::time::Duration::from_millis(2_500));
    signal(holder.id(), libc::SIGCONT);

    assert!(wait(&mut first).success());
    assert!(wait(&mut second).success());
    assert_eq!(wait(&mut holder).code(), Some(3));
    assert_eq!(sandbox.read("signals"), "term\n", "one SIGTERM");
}
