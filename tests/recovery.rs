//! Acceptance criteria 3, 8 and 9: slots and places held by killed processes
//! are reclaimed, but not while the task's process group is still running.
//! Also covers SIGTERM/SIGINT handling.

mod common;

use std::os::unix::process::ExitStatusExt;

use common::*;
use loadmaster::queue::JobState;

fn sandbox() -> Sandbox {
    // Build the config in two steps: hold_script needs the sandbox's paths.
    let probe = Sandbox::new(4, "");
    let tasks = sh_task("hold", 4, &probe.hold_script("hold"))
        + &sh_task("quick", 4, "echo quick-ran")
        + &sh_task(
            "small",
            1,
            &format!("echo ran > {}", q(&probe.path("small.ran"))),
        );
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    probe
}

#[test]
fn killing_loadmaster_keeps_the_slot_while_the_task_runs() {
    let sandbox = sandbox();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    wait_until("the child group to be recorded", || {
        sandbox.job("hold").is_some_and(|j| j.child.is_some())
    });

    signal(holder.id(), libc::SIGKILL);
    wait(&mut holder);

    // Criterion 8: the task is still running, so its slot stays taken.
    let snapshot = sandbox.snapshot();
    assert!(snapshot.reaped.is_empty());
    assert_eq!(snapshot.used(), 4);
    assert!(snapshot.jobs[0].is_orphaned());
    let status = sandbox.run(&["status"]);
    assert!(
        stdout(&status).contains("4/4 in use"),
        "{}",
        stdout(&status)
    );
    assert!(stdout(&status).contains("orphaned"), "{}", stdout(&status));

    // Criterion 3: once the task's process group exits, the next state update
    // reclaims the slot.
    let child = snapshot.jobs[0].child.unwrap();
    sandbox.release("hold");
    wait_until("the orphaned task to exit", || !child.is_alive());
    let status = sandbox.run(&["status"]);
    assert!(
        stdout(&status).contains("0/4 in use"),
        "{}",
        stdout(&status)
    );
    assert!(
        stdout(&status).contains("reclaimed 1 job"),
        "{}",
        stdout(&status)
    );
    let quick = sandbox.run(&["run", "quick"]);
    assert!(quick.status.success());
    assert_eq!(stdout(&quick), "quick-ran\n");
}

#[test]
fn killing_loadmaster_and_its_task_frees_the_slot_on_the_next_update() {
    let sandbox = sandbox();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the child group to be recorded", || {
        sandbox.job("hold").is_some_and(|j| j.child.is_some())
    });
    let child = sandbox.job("hold").unwrap().child.unwrap();

    signal(holder.id(), libc::SIGKILL);
    child.signal(libc::SIGKILL).unwrap();
    wait(&mut holder);
    wait_until("the task to die", || !child.is_alive());

    // `clean` is the first state update after the kill.
    let clean = sandbox.run(&["clean"]);
    assert!(
        stdout(&clean).contains("reclaimed 1 job(s)"),
        "{}",
        stdout(&clean)
    );
    assert!(stdout(&sandbox.run(&["clean"])).contains("nothing to reclaim"));
    assert!(sandbox.run(&["run", "quick"]).status.success());
}

#[test]
fn killed_waiting_job_leaves_the_line() {
    let sandbox = sandbox();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    let mut waiter = sandbox.spawn(&["run", "small"]);
    wait_until("the small job to wait", || {
        sandbox
            .job("small")
            .is_some_and(|j| j.state == JobState::Waiting)
    });

    signal(waiter.id(), libc::SIGKILL);
    wait(&mut waiter);

    // Criterion 9: the next state update removes it.
    let snapshot = sandbox.snapshot();
    assert_eq!(snapshot.reaped.len(), 1);
    assert_eq!(snapshot.reaped[0].task, "small");
    assert!(snapshot.jobs.iter().all(|j| j.task != "small"));

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    assert!(!sandbox.path("small.ran").exists());
}

#[test]
fn sigterm_or_sigint_while_waiting_leaves_the_line() {
    for (sig, code) in [(libc::SIGTERM, 143), (libc::SIGINT, 130)] {
        let sandbox = sandbox();
        let mut holder = sandbox.spawn(&["run", "hold"]);
        wait_until("the task to start", || sandbox.started("hold"));
        let mut waiter = sandbox.spawn(&["run", "small"]);
        wait_until("the small job to wait", || sandbox.job("small").is_some());

        signal(waiter.id(), sig);
        let status = wait(&mut waiter);
        assert_eq!(status.code(), Some(code), "signal {sig}");

        // It removed itself; nothing was left for the reaper.
        let snapshot = sandbox.snapshot();
        assert!(snapshot.reaped.is_empty());
        assert!(snapshot.jobs.iter().all(|j| j.task != "small"));

        sandbox.release("hold");
        assert!(wait(&mut holder).success());
        assert!(!sandbox.path("small.ran").exists(), "the task must not run");
    }
}

#[test]
fn sigterm_while_running_is_forwarded_to_the_task_group() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let log = q(log.path());
    // `sh` runs the trap only after its foreground command ends, so a quick
    // exit proves the signal reached that command too, i.e. the whole group.
    // "ready" comes from the command itself, a shell without the trap that
    // then becomes `sleep`: a signal sent between `sh` forking a command
    // and the command's exec would hit the trap handler the fork inherited,
    // and be lost with it on exec.
    let script = format!(
        "trap 'echo got-term >> {log}; exit 42' TERM; \
         sh -c 'echo ready >> \"$1\"; exec sleep 30' sh {log}"
    );
    let sandbox = Sandbox::new(4, &sh_task("serve", 4, &script));
    let mut runner = sandbox.spawn(&["run", "serve"]);
    let read_log = || std::fs::read_to_string(log.trim_matches('\'')).unwrap_or_default();
    wait_until("the task to start", || read_log().contains("ready"));

    let signalled = std::time::Instant::now();
    signal(runner.id(), libc::SIGTERM);
    let status = wait(&mut runner);
    assert!(
        signalled.elapsed() < std::time::Duration::from_secs(10),
        "the signal did not reach the whole group"
    );

    // loadmaster waited for the task and returned the task's own exit code.
    assert_eq!(status.code(), Some(42));
    assert!(read_log().contains("got-term"));
    let snapshot = sandbox.snapshot();
    assert!(snapshot.jobs.is_empty());
    assert!(
        snapshot.reaped.is_empty(),
        "the slot was released, not reaped"
    );
}

#[test]
fn signal_killed_task_maps_to_128_plus_n() {
    let sandbox = Sandbox::new(4, &sh_task("die", 1, "kill -KILL $$"));
    let output = sandbox.run(&["run", "die"]);
    assert_eq!(output.status.code(), Some(128 + libc::SIGKILL));
    assert_eq!(
        output.status.signal(),
        None,
        "loadmaster itself exits normally"
    );
}

#[test]
fn slot_stays_taken_while_processes_the_task_left_behind_run() {
    let probe = Sandbox::new(4, "");
    // The main process exits at once, leaving a background job in its group.
    let script = format!("({}) & echo leader-done", probe.hold_script("left"));
    let tasks = sh_task("bg", 4, &script) + &sh_task("quick", 4, "true");
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = probe;

    let output = sandbox.run(&["run", "bg"]);
    assert!(output.status.success());
    assert!(
        stderr(&output).contains("processes it started are still running"),
        "{}",
        stderr(&output)
    );
    wait_until("the background job to start", || sandbox.started("left"));
    let snapshot = sandbox.snapshot();
    assert_eq!(snapshot.used(), 4);
    assert!(snapshot.jobs[0].is_orphaned());
    let child = snapshot.jobs[0].child.unwrap();

    sandbox.release("left");
    wait_until("the background job to exit", || !child.is_alive());
    assert_eq!(sandbox.snapshot().used(), 0);
    assert!(sandbox.run(&["run", "quick"]).status.success());
}

#[test]
fn ignored_signals_stay_ignored() {
    let probe = Sandbox::new(4, "");
    let tasks = sh_task("hold", 4, &probe.hold_script("hold"));
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = probe;
    // Like `nohup loadmaster run hold`.
    let mut cmd = sandbox.cmd(&["run", "hold"]);
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut runner = cmd.spawn().unwrap();
    wait_until("the task to start", || sandbox.started("hold"));
    wait_until("the child group to be recorded", || {
        sandbox.job("hold").is_some_and(|j| j.child.is_some())
    });

    signal(runner.id(), libc::SIGHUP);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(runner.try_wait().unwrap().is_none(), "loadmaster died");
    assert!(sandbox.job("hold").unwrap().child.unwrap().is_alive());

    sandbox.release("hold");
    assert!(wait(&mut runner).success());
}

#[test]
fn sigterm_reaches_the_task_once() {
    // The task logs each SIGTERM and carries on until released.
    let probe = Sandbox::new(4, "");
    let script = format!(
        "trap 'echo term >> {}' TERM; {}",
        q(&probe.path("signals")),
        probe.hold_script("hold")
    );
    std::fs::write(
        probe.repo.join(".loadmaster.toml"),
        sh_task("hold", 1, &script),
    )
    .unwrap();
    let sandbox = probe;
    let mut runner = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));
    signal(runner.id(), libc::SIGTERM);
    wait_until("the first SIGTERM", || {
        sandbox.read("signals").contains("term")
    });
    signal(runner.id(), libc::SIGTERM);
    std::thread::sleep(std::time::Duration::from_millis(300));
    sandbox.release("hold");
    assert!(wait(&mut runner).success());
    assert_eq!(sandbox.read("signals"), "term\n");
}
