//! Resources other than CPU: a job waiting for one holds back only that
//! resource, per-checkout limits keep two runs in one checkout apart, and the
//! machine config can change a capacity. Also the scenarios of a real
//! migration: an e2e runtime limit, one e2e run per worktree, and a long
//! series of checks taking turns with the checks other agents run.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use common::*;
use loadmaster::queue::JobState;

/// `e2e-hold` keeps a database until released; `e2e` and `lint` log to
/// `log` and exit.
fn sandbox(db: &str, log: &Path) -> Sandbox {
    let sandbox = Sandbox::new(8, "");
    let tasks = format!("[resources.db]\n{db}\n\n")
        + &sh_task("e2e-hold", 2, &sandbox.hold_script("hold"))
        + &sh_task("e2e", 2, &format!("echo \"e2e $PWD\" >> {}", q(log)))
        + &sh_task("lint", 1, &format!("echo lint >> {}", q(log)));
    // Both e2e tasks use a database.
    let tasks = tasks.replace(
        "[tasks.e2e-hold]\nweight = 2\n",
        "[tasks.e2e-hold]\nweight = 2\nuses = { db = 1 }\n",
    );
    let tasks = tasks.replace(
        "[tasks.e2e]\nweight = 2\n",
        "[tasks.e2e]\nweight = 2\nuses = { db = 1 }\n",
    );
    std::fs::write(sandbox.repo.join(".loadmaster.toml"), &tasks).unwrap();
    // A second worktree of the same project.
    let other = sandbox.path("repo2");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join(".loadmaster.toml"), &tasks).unwrap();
    sandbox
}

fn other_checkout(sandbox: &Sandbox) -> PathBuf {
    sandbox.path("repo2")
}

/// Starts `loadmaster` from `dir`, with stderr in the sandbox file `err`.
fn spawn_in(sandbox: &Sandbox, dir: &Path, args: &[&str], err: &str) -> Child {
    sandbox
        .cmd(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(sandbox.path(err)).unwrap())
        .spawn()
        .unwrap()
}

fn run_in(sandbox: &Sandbox, dir: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = sandbox.cmd(args);
    cmd.current_dir(dir);
    output(cmd)
}

fn hold_database(sandbox: &Sandbox) -> Child {
    let holder = sandbox.spawn(&["run", "e2e-hold"]);
    wait_until("the database to be taken", || sandbox.started("hold"));
    holder
}

fn wait_for_waiting(sandbox: &Sandbox, task: &str) {
    wait_until(&format!("`{task}` to wait"), || {
        sandbox
            .job(task)
            .is_some_and(|j| j.state == JobState::Waiting)
    });
}

#[test]
fn a_lint_passes_an_e2e_run_waiting_for_a_database() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = sandbox("capacity = 1", log.path());
    let mut holder = hold_database(&sandbox);
    let mut e2e = spawn_in(&sandbox, &sandbox.repo, &["run", "e2e"], "e2e.err");
    wait_for_waiting(&sandbox, "e2e");
    wait_until("the reason on stderr", || {
        sandbox.read("e2e.err").contains(
            "loadmaster: #2 `e2e` is waiting (weight 2, next in line) for `db` \
             (1 of 1 in use by #1 `e2e-hold`); running: #1 `e2e-hold` for ",
        )
    });

    // The CPU is mostly free, and lint needs no database.
    let lint = sandbox.run(&["run", "lint"]);
    assert!(lint.status.success(), "{}", stderr(&lint));
    assert_eq!(sandbox.job("e2e").unwrap().state, JobState::Waiting);

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    assert!(wait(&mut e2e).success(), "{}", sandbox.read("e2e.err"));
    let log = std::fs::read_to_string(log.path()).unwrap();
    assert!(log.starts_with("lint\ne2e "), "{log}");
}

#[test]
fn one_e2e_run_per_checkout() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = sandbox("capacity = 2\nper_checkout = 1", log.path());
    let mut holder = hold_database(&sandbox);

    // Another worktree gets the second database at once.
    let other = run_in(&sandbox, &other_checkout(&sandbox), &["run", "e2e"]);
    assert!(other.status.success(), "{}", stderr(&other));
    // This worktree waits, though a database is free machine-wide.
    let mut same = spawn_in(&sandbox, &sandbox.repo, &["run", "e2e"], "same.err");
    wait_for_waiting(&sandbox, "e2e");
    wait_until("the reason on stderr", || {
        sandbox
            .read("same.err")
            .contains("for `db` in its checkout (1 of 1 in use by #1 `e2e-hold`)")
    });
    let status = stdout(&sandbox.run(&["status"]));
    assert!(status.contains("`db`: 1 of 2 in use\n"), "{status}");
    assert!(status.contains("#3 waiting `e2e` "), "{status}");
    assert!(
        status.contains(
            ": next in line; waits for `db` in its checkout (1 of 1 in use by #1 `e2e-hold`)\n"
        ),
        "{status}"
    );

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    assert!(wait(&mut same).success(), "{}", sandbox.read("same.err"));
    let log = std::fs::read_to_string(log.path()).unwrap();
    let dirs: Vec<&str> = log.lines().collect();
    assert_eq!(dirs.len(), 2, "{log}");
    assert!(dirs[0].ends_with("/repo2"), "{log}");
}

#[test]
fn the_machine_config_overrides_a_resource_capacity() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = sandbox("capacity = 1", log.path());
    std::fs::write(
        &sandbox.machine_config,
        format!("capacity = 8\n{STATIC_CPU}[resources]\ndb = 2\n"),
    )
    .unwrap();
    let mut holder = hold_database(&sandbox);
    let second = run_in(&sandbox, &other_checkout(&sandbox), &["run", "e2e"]);
    assert!(second.status.success(), "{}", stderr(&second));
    sandbox.release("hold");
    assert!(wait(&mut holder).success());
}

#[test]
fn an_amount_above_the_capacity_takes_all_of_it() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = sandbox("capacity = 1", log.path());
    let config = sandbox.repo.join(".loadmaster.toml");
    let tasks = std::fs::read_to_string(&config).unwrap().replace(
        "[tasks.e2e]\nweight = 2\nuses = { db = 1 }",
        "[tasks.e2e]\nweight = 2\nuses = { db = 3 }",
    );
    std::fs::write(&config, tasks).unwrap();
    let output = sandbox.run(&["run", "e2e"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output)
            .contains("`e2e` uses 3 of `db`, above its capacity of 1; it takes all of it"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn status_shows_what_jobs_use() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = sandbox("capacity = 1", log.path());
    let mut holder = hold_database(&sandbox);
    let status = stdout(&sandbox.run(&["status"]));
    assert!(status.contains("`db`: 1 of 1 in use\n"), "{status}");
    assert!(status.contains(", using `db`, for "), "{status}");
    sandbox.release("hold");
    assert!(wait(&mut holder).success());
}

#[test]
fn held_can_require_a_resource() {
    let sandbox = Sandbox::new(8, "");
    let check = format!("{} held --uses db; echo \"held=$?\"", q(Path::new(BIN)));
    let tasks = "[resources.db]\ncapacity = 1\n\n".to_owned()
        + &sh_task("e2e", 1, &check).replace("weight = 1\n", "weight = 1\nuses = { db = 1 }\n")
        + &sh_task("lint", 1, &check);
    std::fs::write(sandbox.repo.join(".loadmaster.toml"), tasks).unwrap();

    let e2e = sandbox.run(&["run", "e2e"]);
    assert_eq!(stdout(&e2e), "held=0\n", "{}", stderr(&e2e));
    let lint = sandbox.run(&["run", "lint"]);
    assert_eq!(stdout(&lint), "held=1\n");
    assert!(
        stderr(&lint).contains("(`lint`) does not use `db`"),
        "{}",
        stderr(&lint)
    );
}

#[test]
fn a_series_takes_turns_with_checks_that_arrive_meanwhile() {
    // A long "verify everything" series, run one check at a time, and a short
    // e2e run that an iterating agent starts while the series runs: the e2e
    // run goes between the steps instead of waiting for the whole series.
    let log = tempfile::NamedTempFile::new().unwrap();
    let sandbox = Sandbox::new(4, "");
    let tasks = sh_task(
        "step1",
        4,
        &format!(
            "echo step1 >> {}; {}",
            q(log.path()),
            sandbox.hold_script("step1")
        ),
    ) + &sh_task("step2", 4, &format!("echo step2 >> {}", q(log.path())))
        + &sh_task("e2e", 2, &format!("echo e2e >> {}", q(log.path())));
    std::fs::write(sandbox.repo.join(".loadmaster.toml"), tasks).unwrap();

    let bin = q(Path::new(BIN));
    let mut series = Command::new("sh")
        .args(["-c", &format!("set -e; {bin} run step1; {bin} run step2")])
        .current_dir(&sandbox.repo)
        .env("LOADMASTER_CONFIG", &sandbox.machine_config)
        .env("LOADMASTER_STATE_DIR", &sandbox.state)
        .env_remove("LOADMASTER_HELD")
        .env("LOADMASTER_MAX_WAIT", "")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the first step to start", || sandbox.started("step1"));
    let mut e2e = sandbox.spawn(&["run", "e2e"]);
    wait_for_waiting(&sandbox, "e2e");

    sandbox.release("step1");
    assert!(wait(&mut series).success());
    assert!(wait(&mut e2e).success());
    let order = std::fs::read_to_string(log.path()).unwrap();
    assert_eq!(order, "step1\ne2e\nstep2\n");
}

#[test]
fn a_check_starts_beside_one_waiting_for_what_a_long_run_holds() {
    // A UI capture holds the one browser, and a unit run waits for it: the
    // unit run can only start once the capture ends, which frees its 5 cores,
    // so it sets none aside meanwhile, and the lint behind it starts at once
    // in the 3 left of 8 (clew, 0.9.0: the lint used to wait for the capture).
    let probe = Sandbox::new(8, "");
    let tasks = "[resources.browser]\ncapacity = 1\n\n".to_owned()
        + &sh_task("ui", 5, &probe.hold_script("ui"))
        + &sh_task("unit", 2, "true")
        + &sh_task("lint", 3, "true");
    let tasks = tasks
        .replace(
            "[tasks.ui]\nweight = 5\n",
            "[tasks.ui]\nweight = 5\nuses = { browser = 1 }\n",
        )
        .replace(
            "[tasks.unit]\nweight = 2\n",
            "[tasks.unit]\nweight = 2\nuses = { browser = 1 }\n",
        );
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = probe;
    let mut ui = sandbox.spawn(&["run", "ui"]);
    wait_until("the capture to start", || sandbox.started("ui"));
    let mut unit = sandbox.spawn(&["run", "unit"]);
    wait_for_waiting(&sandbox, "unit");

    let status = stdout(&sandbox.run(&["status"]));
    assert!(
        status.contains("#2 waiting `unit` ")
            && status.contains("waits for `browser` (1 of 1 in use by #1 `ui`)\n"),
        "{status}"
    );
    let lint = sandbox.run(&["run", "lint", "--max-wait", "0"]);
    assert!(lint.status.success(), "{}", stderr(&lint));
    assert_eq!(sandbox.job("unit").unwrap().state, JobState::Waiting);

    sandbox.release("ui");
    assert!(wait(&mut ui).success());
    assert!(wait(&mut unit).success());
}
