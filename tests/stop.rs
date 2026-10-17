//! A task's stop signal (`stop_signal`), and processes a task leaves running
//! outside its process group.

mod common;

use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::*;
use loadmaster::process::ProcessId;

/// `task` (from [`sh_task`]) stopped with `signal`.
fn stopped_with(task: String, signal: &str) -> String {
    task.replacen(
        "\ncmd = ",
        &format!("\nstop_signal = \"{signal}\"\ncmd = "),
        1,
    )
}

/// A sandbox whose `hold` task logs each SIGINT and SIGTERM it gets and
/// carries on until released, and whose stop signal is SIGINT.
fn logging_sandbox() -> Sandbox {
    let probe = Sandbox::new(4, "");
    let log = q(&probe.path("signals"));
    let script = format!(
        "echo $$ > {}; trap 'echo int >> {log}' INT; trap 'echo term >> {log}' TERM; {}",
        q(&probe.path("hold.pid")),
        probe.hold_script("hold")
    );
    std::fs::write(
        probe.repo.join(".loadmaster.toml"),
        stopped_with(sh_task("hold", 1, &script), "SIGINT"),
    )
    .unwrap();
    probe
}

/// Starts `loadmaster run hold` in a [`logging_sandbox`], keeping what it
/// says for [`wait_for_signals`], once the task has started.
fn run_hold(sandbox: &Sandbox) -> Child {
    let mut cmd = sandbox.cmd(&["run", "hold"]);
    cmd.stdout(Stdio::null())
        .stderr(std::fs::File::create(sandbox.path("hold.err")).unwrap());
    let runner = cmd.spawn().unwrap();
    wait_until("the task to start", || sandbox.started("hold"));
    runner
}

/// Waits until the task of a [`logging_sandbox`] has logged `n` signals or
/// more (more fails the test's own check of the log, not a wait), else says
/// what it logged, whether a process with its PID still runs (a zombie does
/// not), whether `loadmaster` does, and what that said.
fn wait_for_signals(sandbox: &Sandbox, runner: &mut Child, n: usize) {
    let deadline = Instant::now() + TIMEOUT;
    while sandbox.read("signals").lines().count() < n {
        if Instant::now() >= deadline {
            let task: Option<u32> = sandbox.read("hold.pid").trim().parse().ok();
            let task_runs = task.is_some_and(|pid| matches!(ProcessId::of(pid), Ok(Some(_))));
            panic!(
                "the task logged {:?}, not {n} signals; a process with its PID ({task:?}) \
                 {}; loadmaster {:?}, and said: {:?}",
                sandbox.read("signals"),
                if task_runs { "runs" } else { "does not run" },
                runner.try_wait(),
                sandbox.read("hold.err"),
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_sigterm_reaches_the_task_as_its_stop_signal_once() {
    let sandbox = logging_sandbox();
    let mut runner = run_hold(&sandbox);
    signal(runner.id(), libc::SIGTERM);
    wait_for_signals(&sandbox, &mut runner, 1);
    signal(runner.id(), libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(300));
    sandbox.release("hold");
    assert!(wait(&mut runner).success());
    assert_eq!(sandbox.read("signals"), "int\n");
    let history = sandbox.read("state/history.jsonl");
    assert!(history.contains("\"stopped\":true"), "{history}");
}

#[test]
fn the_stop_signal_reaches_a_task_whose_loadmaster_inherited_it_ignored() {
    // As a script's background job does SIGINT: the task must not inherit
    // that, or nothing would stop it.
    let sandbox = logging_sandbox();
    let mut cmd = sandbox.cmd(&["run", "hold"]);
    cmd.stdout(Stdio::null())
        .stderr(std::fs::File::create(sandbox.path("hold.err")).unwrap());
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut runner = cmd.spawn().unwrap();
    wait_until("the task to start", || sandbox.started("hold"));
    signal(runner.id(), libc::SIGTERM);
    wait_for_signals(&sandbox, &mut runner, 1);
    sandbox.release("hold");
    assert!(wait(&mut runner).success());
    assert_eq!(sandbox.read("signals"), "int\n");
}

#[test]
fn ctrl_c_passes_each_time_and_counts_as_the_stop() {
    let sandbox = logging_sandbox();
    let mut runner = run_hold(&sandbox);
    for n in 1..=2 {
        signal(runner.id(), libc::SIGINT);
        wait_for_signals(&sandbox, &mut runner, n);
    }
    // It has had its stop signal: a SIGTERM asks for nothing more.
    signal(runner.id(), libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(300));
    sandbox.release("hold");
    assert!(wait(&mut runner).success());
    assert_eq!(sandbox.read("signals"), "int\nint\n");
}

#[test]
fn a_cancel_sends_the_stop_signal_and_says_which() {
    let probe = Sandbox::new(4, "");
    let log = q(&probe.path("signals"));
    let script = format!(
        "trap 'echo int >> {log}; exit 3' INT; trap 'echo term >> {log}; exit 4' TERM; {}",
        probe.hold_script("hold")
    );
    std::fs::write(
        probe.repo.join(".loadmaster.toml"),
        stopped_with(sh_task("hold", 1, &script), "SIGINT"),
    )
    .unwrap();
    let sandbox = probe;
    let mut cmd = sandbox.cmd(&["run", "hold"]);
    cmd.stdout(Stdio::null())
        .stderr(std::fs::File::create(sandbox.path("hold.err")).unwrap());
    let mut runner = cmd.spawn().unwrap();
    wait_until("the task to start", || sandbox.started("hold"));

    let output = sandbox.run(&["cancel", "1"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(wait(&mut runner).code(), Some(3));
    assert_eq!(sandbox.read("signals"), "int\n");
    assert!(
        sandbox
            .read("hold.err")
            .contains("canceled with `loadmaster cancel`; sending it SIGINT"),
        "{}",
        sandbox.read("hold.err")
    );
}

#[test]
fn a_cancel_stops_a_task_whose_loadmaster_is_gone_with_its_stop_signal() {
    let sandbox = logging_sandbox();
    let mut runner = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    signal(runner.id(), libc::SIGKILL);
    wait(&mut runner);
    wait_until("the job to be orphaned", || {
        sandbox.job("hold").is_some_and(|j| j.is_orphaned())
    });

    let mut cancel = sandbox.spawn(&["cancel", "1"]);
    wait_until("the stop signal", || {
        sandbox.read("signals").contains("int")
    });
    sandbox.release("hold");
    assert!(wait(&mut cancel).success());
    assert_eq!(sandbox.read("signals"), "int\n");
}

/// A task that starts a process in a session of its own, which writes its
/// PID to `pid` and lives `secs` seconds, and exits a while later.
fn leaving_sandbox(secs: f64) -> Sandbox {
    let probe = Sandbox::new(4, "");
    let pid = q(&probe.path("pid"));
    let script = format!(
        "perl -MPOSIX -e 'POSIX::setsid(); open(my $f, \">\", $ARGV[0]) or die; print $f $$; \
         close $f; select(undef, undef, undef, $ARGV[1])' {pid} {secs} & sleep 1.5"
    );
    std::fs::write(
        probe.repo.join(".loadmaster.toml"),
        sh_task("serve", 1, &script),
    )
    .unwrap();
    probe
}

#[test]
fn processes_left_running_outside_the_group_are_named() {
    let sandbox = leaving_sandbox(30.0);
    let output = sandbox.run(&["run", "serve"]);
    let pid: u32 = sandbox.read("pid").trim().parse().unwrap();
    signal(pid, libc::SIGKILL);
    assert!(output.status.success(), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(
        err.contains(
            "loadmaster: warning: `serve` exited, but processes it started outside its process \
             group are still running ("
        ) && err.contains(&format!("({pid} `")),
        "{err}"
    );
    assert!(sandbox.snapshot().jobs.is_empty(), "no slot is held for it");
}

#[test]
fn a_process_that_ends_with_the_task_is_not_named() {
    // It outlives the task by a moment.
    let sandbox = leaving_sandbox(1.7);
    let output = sandbox.run(&["run", "serve"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stderr(&output).contains("warning"), "{}", stderr(&output));
}

/// Stops, with `stop` (given the sandbox and the PID of `loadmaster`), a
/// task that starts a process in a session of its own after the first look
/// at its processes and before the next, and dies with what it waits for:
/// once it is gone, nothing leads to that process. Its loadmaster still
/// names it, having looked just before the stop.
fn stopped_just_after_leaving(stop: impl FnOnce(&Sandbox, u32)) {
    let probe = Sandbox::new(4, "");
    let pid = q(&probe.path("pid"));
    let script = format!(
        "sleep 0.3; perl -MPOSIX -e 'POSIX::setsid(); open(my $f, \">\", $ARGV[0]) or die; \
         print $f $$; close $f; sleep 30' {pid} & wait"
    );
    std::fs::write(
        probe.repo.join(".loadmaster.toml"),
        sh_task("serve", 1, &script),
    )
    .unwrap();
    let sandbox = probe;
    let mut cmd = sandbox.cmd(&["run", "serve"]);
    cmd.stdout(Stdio::null())
        .stderr(std::fs::File::create(sandbox.path("serve.err")).unwrap());
    let mut runner = cmd.spawn().unwrap();
    wait_until("the process to start", || !sandbox.read("pid").is_empty());
    let pid: u32 = sandbox.read("pid").trim().parse().unwrap();
    stop(&sandbox, runner.id());
    wait(&mut runner);
    signal(pid, libc::SIGKILL);
    let err = sandbox.read("serve.err");
    assert!(
        err.contains("outside its process group are still running (")
            && err.contains(&format!("({pid} `")),
        "{err}"
    );
}

#[test]
fn a_process_started_just_before_a_cancel_is_named() {
    stopped_just_after_leaving(|sandbox, _| {
        let output = sandbox.run(&["cancel", "1"]);
        assert!(output.status.success(), "{}", stderr(&output));
    });
}

#[test]
fn a_process_started_just_before_a_sigterm_is_named() {
    stopped_just_after_leaving(|_, loadmaster| signal(loadmaster, libc::SIGTERM));
}
