//! Starting a task and supervising it.

use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::process::{ProcessGroup, ProcessId};

/// Set in a running task's environment to the job ID that holds the slot.
/// A nested `loadmaster run` sees it and runs directly instead of queueing.
pub const HELD_ENV: &str = "LOADMASTER_HELD";
/// Set in a running task's environment to the task's weight, so the task can
/// size its own parallelism to match (e.g. `--maxWorkers=$LOADMASTER_WEIGHT`).
pub const WEIGHT_ENV: &str = "LOADMASTER_WEIGHT";

/// Signals loadmaster intercepts: they abort a wait, and are forwarded to the
/// task's process group while it runs.
pub const INTERCEPTED_SIGNALS: [i32; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// `cmd` followed by `extra_args`, run in `dir`. No shell is involved, so
/// arguments are passed through exactly as given.
pub fn command(cmd: &[String], extra_args: &[String], dir: &Path) -> Command {
    let (program, args) = cmd.split_first().expect("cmd is validated to be non-empty");
    let mut command = Command::new(program);
    command.args(args).args(extra_args).current_dir(dir);
    command
}

/// Records which intercepted signals have arrived.
pub struct Signals {
    pending: [(i32, Arc<AtomicBool>); INTERCEPTED_SIGNALS.len()],
}

impl Signals {
    /// Starts intercepting [`INTERCEPTED_SIGNALS`], except those the caller
    /// set to be ignored (as `nohup` does for SIGHUP): those stay ignored, for
    /// the task too. Also restores SIGCHLD's default action, without which
    /// exited children could not be waited for.
    pub fn install() -> io::Result<Signals> {
        // SAFETY: restoring a default disposition has no preconditions.
        unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) };
        let pending = INTERCEPTED_SIGNALS.map(|signal| (signal, Arc::new(AtomicBool::new(false))));
        for (signal, flag) in &pending {
            if !is_ignored(*signal)? {
                signal_hook::flag::register(*signal, Arc::clone(flag))?;
            }
        }
        Ok(Signals { pending })
    }

    /// The signals received since the previous call, each listed once.
    pub fn take(&self) -> Vec<i32> {
        self.pending
            .iter()
            .filter(|(_, flag)| flag.swap(false, Ordering::SeqCst))
            .map(|(signal, _)| *signal)
            .collect()
    }
}

fn is_ignored(signal: i32) -> io::Result<bool> {
    let mut action = MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: with a null new action, sigaction only reports the current one.
    if unsafe { libc::sigaction(signal, std::ptr::null(), action.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: sigaction filled it in.
    Ok(unsafe { action.assume_init() }.sa_sigaction == libc::SIG_IGN)
}

/// A started task.
pub struct Task {
    pub child: Child,
    pub group: ProcessGroup,
}

/// Why a task did not start.
#[derive(Debug)]
pub enum SpawnError<E> {
    /// The command could not be started.
    Spawn(io::Error),
    /// Recording the task failed, so it was not run.
    Register(E),
}

/// Starts `command` as the leader of a new process group, so the whole task
/// can be signalled and tracked as a unit.
///
/// The child waits just before `exec` until `register` has recorded its
/// process group, so the task never runs untracked. If `register` fails, or
/// this process dies before it succeeds, the waiting child sees its gate close
/// and exits without running the task.
pub fn spawn<E: Send + 'static>(
    command: &mut Command,
    register: impl FnOnce(ProcessGroup) -> Result<(), E> + Send + 'static,
) -> Result<Task, SpawnError<E>> {
    let (hello_read, hello_write) = cloexec_pipe().map_err(SpawnError::Spawn)?; // child -> us: its PID
    let (gate_read, gate_write) = cloexec_pipe().map_err(SpawnError::Spawn)?; // us -> child: go ahead
    let fds = [&hello_read, &hello_write, &gate_read, &gate_write].map(|fd| fd.as_raw_fd());

    command.process_group(0);
    // A background process group that touches the terminal gets stopped by
    // SIGTTIN/SIGTTOU, which would hang the task. Tasks are batch jobs, so
    // give them no terminal input instead.
    // SAFETY: isatty has no memory-safety preconditions.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        command.stdin(Stdio::null());
    }
    // SAFETY: the hook runs in the forked child and only makes
    // async-signal-safe calls.
    unsafe { command.pre_exec(move || wait_at_gate(fds)) };

    // `Command::spawn` only returns once the child has exec'd, i.e. after the
    // gate opened, so the gate is kept from another thread.
    let gatekeeper = thread::Builder::new()
        .name("loadmaster-gate".into())
        .spawn(move || {
            let mut pid = [0u8; 4];
            File::from(hello_read).read_exact(&mut pid).ok()?;
            let pgid = u32::from_ne_bytes(pid);
            let group = ProcessGroup {
                pgid,
                leader_start_time: ProcessId::of(pgid).ok().flatten().map(|p| p.start_time),
            };
            if let Err(e) = register(group) {
                // Returning drops `gate_write` unopened: the child gives up.
                return Some(Err(e));
            }
            // If this fails the child is already gone; spawn() reports why.
            let _ = File::from(gate_write).write_all(&[1]);
            Some(Ok(group))
        })
        .map_err(SpawnError::Spawn)?;
    let spawned = command.spawn();
    // Close our copies of the child's ends, so the gatekeeper sees EOF if the
    // child fails before reaching the gate.
    drop((hello_write, gate_read));
    let registered = gatekeeper.join().ok().flatten();
    if let Some(Err(e)) = registered {
        return Err(SpawnError::Register(e));
    }
    let child = spawned.map_err(SpawnError::Spawn)?;
    let group = match registered {
        Some(Ok(group)) => group,
        _ => ProcessGroup {
            pgid: child.id(),
            leader_start_time: None,
        },
    };
    Ok(Task { child, group })
}

/// Runs in the forked child just before `exec`: reports the child's PID, then
/// waits for the go-ahead. Only async-signal-safe calls are allowed here.
fn wait_at_gate([hello_read, hello_write, gate_read, gate_write]: [RawFd; 4]) -> io::Result<()> {
    // SAFETY: plain syscalls on descriptors this process owns.
    unsafe {
        // Without closing our copy of the gate's write end, the gate could
        // never report EOF when the parent dies.
        libc::close(hello_read);
        libc::close(gate_write);
        let pid = libc::getpid().to_ne_bytes();
        let written = libc::write(hello_write, pid.as_ptr().cast(), pid.len());
        libc::close(hello_write);
        if written != pid.len() as isize {
            return Err(io::Error::from_raw_os_error(libc::EPIPE));
        }
        let mut go = [0u8; 1];
        loop {
            match libc::read(gate_read, go.as_mut_ptr().cast(), 1) {
                1 => return Ok(()),
                // The parent is gone: never run the task untracked.
                0 => return Err(io::Error::from_raw_os_error(libc::ECANCELED)),
                _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }
}

fn cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for both descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe() just handed us these descriptors.
    let pipe = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in fds {
        // SAFETY: fcntl on a descriptor we own.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(pipe)
}

/// Waits for the task's leader to exit, forwarding intercepted signals to the
/// whole process group in the meantime.
pub fn wait(task: &mut Task, signals: &Signals) -> io::Result<ExitStatus> {
    loop {
        let exited = task.child.try_wait()?;
        // Forward even if the leader just exited: others in its group may
        // still be running.
        forward_pending(&task.group, signals);
        if let Some(status) = exited {
            return Ok(status);
        }
        thread::sleep(CHILD_POLL_INTERVAL);
    }
}

/// Forwards intercepted signals that arrived since the last call to the
/// task's process group, if it still has running members. The check goes by
/// the leader's start time, so a group ID the OS has since reused is never hit.
pub fn forward_pending(group: &ProcessGroup, signals: &Signals) {
    let pending = signals.take();
    if pending.is_empty() || !group.is_alive() {
        return;
    }
    for signal in pending {
        // SIGCONT lets a stopped task (e.g. one that touched the terminal)
        // act on the signal.
        let _ = group.signal(signal);
        let _ = group.signal(libc::SIGCONT);
    }
}

/// The exit code that mirrors `status`: the child's own code, or 128+n if it
/// was killed by signal n.
pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_args_are_appended_verbatim() {
        let cmd = vec!["printf".to_owned(), "[%s]".to_owned()];
        let extra = vec!["a b".to_owned(), "$HOME".to_owned(), "'q'".to_owned()];
        let output = command(&cmd, &extra, Path::new("/")).output().unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "[a b][$HOME]['q']"
        );
    }

    #[test]
    fn exit_code_mirrors_child() {
        let status = Command::new("sh").args(["-c", "exit 7"]).status().unwrap();
        assert_eq!(exit_code(status), 7);
        let status = Command::new("sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .unwrap();
        assert_eq!(exit_code(status), 128 + libc::SIGTERM);
    }

    #[test]
    fn task_waits_at_the_gate_until_registered() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let mut cmd = Command::new("touch");
        cmd.arg(&marker);
        let ran_early = Arc::new(AtomicBool::new(true));
        let (flag, path) = (Arc::clone(&ran_early), marker.clone());
        let mut task = spawn(&mut cmd, move |group| {
            // Simulates a slow queue update.
            thread::sleep(Duration::from_millis(200));
            flag.store(path.exists() || !group.is_alive(), Ordering::SeqCst);
            Ok::<(), ()>(())
        })
        .unwrap();
        assert!(task.child.wait().unwrap().success());
        assert!(
            !ran_early.load(Ordering::SeqCst),
            "the task ran before it was registered"
        );
        assert!(marker.exists());
        assert_eq!(task.group.pgid, task.child.id());
        assert!(task.group.leader_start_time.is_some());
    }

    #[test]
    fn task_never_runs_if_registering_fails() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let mut cmd = Command::new("touch");
        cmd.arg(&marker);
        let result = spawn(&mut cmd, |_| Err("database is gone"));
        assert!(matches!(
            result,
            Err(SpawnError::Register("database is gone"))
        ));
        thread::sleep(Duration::from_millis(100));
        assert!(!marker.exists());
    }

    #[test]
    fn task_never_runs_if_the_gate_closes() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let mut cmd = Command::new("touch");
        cmd.arg(&marker);
        // The gate's write end is dropped without opening it, as when
        // loadmaster dies while registering.
        let result = spawn(&mut cmd, |_| -> Result<(), ()> {
            panic!("simulated death while registering")
        });
        assert!(matches!(result, Err(SpawnError::Spawn(_))));
        thread::sleep(Duration::from_millis(100));
        assert!(!marker.exists());
    }
}
