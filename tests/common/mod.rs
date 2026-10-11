//! Test harness: an isolated machine config, state directory and repository
//! per test, plus helpers to drive real `loadmaster` processes.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use loadmaster::queue::{Job, Queue, STATE_DB_FILE, Snapshot};

pub const BIN: &str = env!("CARGO_BIN_EXE_loadmaster");
pub const TIMEOUT: Duration = Duration::from_secs(60);

pub struct Sandbox {
    pub root: tempfile::TempDir,
    pub repo: PathBuf,
    pub state: PathBuf,
    pub machine_config: PathBuf,
}

impl Sandbox {
    /// `tasks` is the body of `.loadmaster.toml`; see [`task`] and [`sh_task`].
    pub fn new(capacity: u32, tasks: &str) -> Sandbox {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let state = root.path().join("state");
        let machine_config = root.path().join("machine.toml");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(&machine_config, format!("capacity = {capacity}\n")).unwrap();
        fs::write(repo.join(".loadmaster.toml"), tasks).unwrap();
        Sandbox {
            root,
            repo,
            state,
            machine_config,
        }
    }

    /// A path inside the sandbox (outside the repo).
    pub fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// A `loadmaster` command isolated to this sandbox, run from the repo.
    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .current_dir(&self.repo)
            .env("LOADMASTER_CONFIG", &self.machine_config)
            .env("LOADMASTER_STATE_DIR", &self.state)
            .env_remove("LOADMASTER_HELD")
            .env_remove("LOADMASTER_MAX_WAIT")
            .stdin(Stdio::null());
        cmd
    }

    /// Starts `loadmaster` in the background, discarding its output.
    pub fn spawn(&self, args: &[&str]) -> Child {
        self.cmd(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Runs `loadmaster` to completion and captures its output.
    pub fn run(&self, args: &[&str]) -> Output {
        output(self.cmd(args))
    }

    pub fn snapshot(&self) -> Snapshot {
        Queue::open(&self.state.join(STATE_DB_FILE))
            .unwrap()
            .snapshot()
            .unwrap()
    }

    /// The job for `task`, if it is in the queue.
    pub fn job(&self, task: &str) -> Option<Job> {
        self.snapshot().jobs.into_iter().find(|j| j.task == task)
    }

    /// Lets a task started with [`hold_script`] finish.
    pub fn release(&self, name: &str) {
        fs::write(self.path(&format!("{name}.release")), "").unwrap();
    }

    /// A task script that records its PID, then runs until [`Sandbox::release`]
    /// (or 30 seconds, so a failed test cannot leave it running forever).
    pub fn hold_script(&self, name: &str) -> String {
        format!(
            "echo started > {ready}; i=0; \
             while [ ! -e {release} ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done",
            ready = q(&self.path(&format!("{name}.started"))),
            release = q(&self.path(&format!("{name}.release"))),
        )
    }

    pub fn started(&self, name: &str) -> bool {
        self.path(&format!("{name}.started")).exists()
    }

    pub fn read(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap_or_default()
    }
}

impl Drop for Sandbox {
    /// Kills anything a failed test left behind.
    fn drop(&mut self) {
        let Ok(queue) = Queue::open(&self.state.join(STATE_DB_FILE)) else {
            return;
        };
        let Ok(snapshot) = queue.snapshot() else {
            return;
        };
        // Snapshots only keep live jobs, but check again right before
        // killing, so a recycled PID is never hit.
        for job in snapshot.jobs {
            if let Some(owner) = job.owner.filter(|o| o.is_alive()) {
                signal(owner.pid, libc::SIGKILL);
            }
            if let Some(child) = job.child.filter(|c| c.is_alive()) {
                let _ = child.signal(libc::SIGKILL);
            }
        }
    }
}

/// A `[tasks.<name>]` table.
pub fn task(name: &str, weight: u32, cmd: &[&str]) -> String {
    let cmd = toml::Value::Array(
        cmd.iter()
            .map(|s| toml::Value::String((*s).to_owned()))
            .collect(),
    );
    format!("[tasks.{name}]\nweight = {weight}\ncmd = {cmd}\n\n")
}

/// A task running `script` with `sh -c`. Extra arguments become `$1`, `$2`, ...
/// (the trailing "sh" fills `$0`).
pub fn sh_task(name: &str, weight: u32, script: &str) -> String {
    task(name, weight, &["sh", "-c", script, "sh"])
}

/// Single-quotes a path for the shell.
pub fn q(path: &Path) -> String {
    format!("'{}'", path.display())
}

pub fn output(mut cmd: Command) -> Output {
    // Files rather than pipes: a pipe would block us if a task leaves a
    // process behind that still holds it.
    let dir = tempfile::tempdir().unwrap();
    let (out, err) = (dir.path().join("out"), dir.path().join("err"));
    let mut child = cmd
        .stdout(fs::File::create(&out).unwrap())
        .stderr(fs::File::create(&err).unwrap())
        .spawn()
        .unwrap();
    let status = wait(&mut child);
    Output {
        status,
        stdout: fs::read(out).unwrap(),
        stderr: fs::read(err).unwrap(),
    }
}

pub fn wait(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("process {} did not exit within {TIMEOUT:?}", child.id());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn signal(pid: u32, signal: i32) {
    // SAFETY: kill has no memory-safety preconditions.
    unsafe { libc::kill(pid as libc::pid_t, signal) };
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
