//! Starting a task and supervising it.

use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::process::{Descendants, ProcessGroup, ProcessId, Runs};

/// Set in a running task's environment to the job ID that holds the slot.
/// A nested `loadmaster run` sees it and runs directly instead of queueing.
pub const HELD_ENV: &str = "LOADMASTER_HELD";
/// Set in a running task's environment to the task's weight, so the task can
/// size its own parallelism to match (e.g. `--maxWorkers=$LOADMASTER_WEIGHT`).
pub const WEIGHT_ENV: &str = "LOADMASTER_WEIGHT";

/// Signals loadmaster intercepts: they abort a wait, and are forwarded to the
/// task's process group while it runs.
pub const INTERCEPTED_SIGNALS: [i32; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// The signals a task may name as its `stop_signal`: ones it can handle.
/// SIGKILL is `loadmaster cancel --kill`.
pub const STOP_SIGNALS: [(&str, i32); 6] = [
    ("SIGTERM", libc::SIGTERM),
    ("SIGINT", libc::SIGINT),
    ("SIGHUP", libc::SIGHUP),
    ("SIGQUIT", libc::SIGQUIT),
    ("SIGUSR1", libc::SIGUSR1),
    ("SIGUSR2", libc::SIGUSR2),
];

/// "SIGTERM", "SIGKILL", or "signal 7", for messages.
pub fn signal_name(signal: i32) -> String {
    match STOP_SIGNALS.iter().find(|&&(_, s)| s == signal) {
        Some((name, _)) => (*name).to_owned(),
        None if signal == libc::SIGKILL => "SIGKILL".to_owned(),
        None => format!("signal {signal}"),
    }
}

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

/// Gives the task `stop`'s default action, whatever this process inherited:
/// a stop signal it inherited as ignored (SIGINT, in a job a script started
/// in the background) would make every request to stop the task do nothing,
/// and a shell cannot even trap a signal it starts with ignored.
pub fn deliver_stop_signal(command: &mut Command, stop: i32) {
    // SAFETY: the hook runs in the forked child and only calls signal(),
    // which is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            libc::signal(stop, libc::SIG_DFL);
            Ok(())
        })
    };
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
    // One gated spawn at a time per process. A child forked by a concurrent
    // spawn would inherit this spawn's gate (and vice versa) until it execs;
    // two children both waiting for their gate to close would then each keep
    // the other's gate open forever. By the time this returns, every gate
    // descriptor is closed again.
    static SPAWN_LOCK: Mutex<()> = Mutex::new(());
    let _serialized = SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

/// How often a running task's loadmaster looks for a request from
/// `loadmaster cancel`, in polls of the task.
const CANCEL_CHECK_EVERY: u32 = 10;
/// How often the processes a task started are looked for, in polls: about
/// once a second, and first after a tenth of one.
const LOOK_EVERY: u32 = 50;
const FIRST_LOOK: u32 = 5;

/// Sends a task the signals meant for it: those intercepted from outside
/// and those a cancel asks for. A SIGTERM, from outside or from a cancel,
/// asks loadmaster to stop the task, and reaches it as the task's stop
/// signal (which tools differ on: Playwright cleans up only on SIGINT), and
/// once, whichever way it came: a second one would cut short the clean-up
/// the first started (as in a handler registered with `process.once`).
/// Other signals pass as they came, each time; SIGKILL is there to force it.
#[derive(Debug)]
pub struct Forwarder {
    stop: i32,
    stop_sent: bool,
    stopped: bool,
}

/// Whether a signal went to a task's process group, and whether its leader,
/// the task's own command, still ran then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    No,
    /// To what it left: the leader had exited.
    ToMembers,
    ToLeader,
}

impl Forwarder {
    /// For a task whose stop signal is `stop`.
    pub fn new(stop: i32) -> Forwarder {
        Forwarder {
            stop,
            stop_sent: false,
            stopped: false,
        }
    }

    /// Forwards intercepted signals that arrived since the last call to the
    /// task's process group, if it still has running members. The check goes
    /// by the leader's start time, so a group ID the OS has since reused is
    /// never hit.
    pub fn forward_pending(&mut self, group: &ProcessGroup, signals: &Signals) {
        let pending = signals.take();
        if pending.is_empty() || !group.is_alive() {
            return;
        }
        // The leader has been waited for: what is left gets it.
        self.forward(pending, |signal| {
            if signal_group(group, signal) {
                Sent::ToMembers
            } else {
                Sent::No
            }
        });
    }

    /// Sends what `received` asks for with `kill`, which says where the
    /// signal went. The stop signal as it came goes first: a SIGTERM that
    /// came with it then asks for nothing more.
    fn forward(&mut self, mut received: Vec<i32>, mut kill: impl FnMut(i32) -> Sent) {
        received.sort_by_key(|&signal| signal != self.stop);
        for signal in received {
            self.send_with(signal, &mut kill);
        }
    }

    /// Whether the task has had its stop signal: then a request to stop
    /// sends none again.
    pub fn stop_sent(&self) -> bool {
        self.stop_sent
    }

    /// Whether the task's own command had its stop signal while it ran: one
    /// sent to what is left once it exited does not make a failure a stop.
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    fn send_with(&mut self, received: i32, kill: impl FnOnce(i32) -> Sent) {
        let request = received == libc::SIGTERM;
        if request && self.stop_sent {
            return;
        }
        let signal = if request { self.stop } else { received };
        // Only a stop signal that was sent counts: one the OS refused (a task
        // that changed its user, say) may be tried again. One that came as
        // itself counts too (Ctrl-C, for a stop signal of SIGINT), and still
        // passes each time.
        let sent = kill(signal);
        if sent != Sent::No && signal == self.stop {
            self.stop_sent = true;
            self.stopped |= sent == Sent::ToLeader;
        }
    }
}

/// Sends `signal` to `group`, and SIGCONT, which lets a stopped task (e.g.
/// one that touched the terminal) act on it. Whether `signal` was sent.
fn signal_group(group: &ProcessGroup, signal: i32) -> bool {
    let sent = group.signal(signal).is_ok();
    let _ = group.signal(libc::SIGCONT);
    sent
}

/// [`signal_group`], if the group still has running members, after a look
/// at the task's processes: one it started since the last look, in a session
/// of its own, is found only through its parent, which the signal may end
/// (as Playwright exits on SIGTERM, leaving its `webServer`). While its
/// leader has not been waited for (`unwaited`), the group is the task's for
/// certain (see [`ProcessGroup::runs_unwaited`]), and whether the leader
/// still runs is told right before the signal; once it has, it is checked
/// after the look, which takes a while, so a group ID the OS has reused
/// meanwhile is never hit.
fn signal_looked(
    group: &ProcessGroup,
    descendants: &mut Descendants,
    signal: i32,
    unwaited: bool,
) -> Sent {
    descendants.look(group);
    let to = if !unwaited {
        if group.is_alive() {
            Sent::ToMembers
        } else {
            Sent::No
        }
    } else {
        match group.runs_unwaited() {
            Runs::Leader => Sent::ToLeader,
            Runs::Members => Sent::ToMembers,
            Runs::Nothing => Sent::No,
        }
    };
    if to != Sent::No && signal_group(group, signal) {
        to
    } else {
        Sent::No
    }
}

/// Waits for the task's leader to exit, forwarding intercepted signals to the
/// whole process group in the meantime, and the signal `canceled` asks for
/// (SIGTERM to stop it, SIGKILL to kill it), which it is asked every few
/// polls; it is told whether the task has had its stop signal already, and
/// the forwarder sends none twice. Meanwhile it looks for the processes the
/// task starts (see [`Descendants`]), and again before each signal it
/// sends (see [`signal_looked`]). Returns the leader's exit status, and
/// whether it had its stop signal while it ran (see [`Forwarder::stopped`]).
pub fn wait(
    task: &mut Task,
    signals: &Signals,
    forwarder: &mut Forwarder,
    descendants: &mut Descendants,
    mut canceled: impl FnMut(bool) -> Option<i32>,
) -> io::Result<(ExitStatus, bool)> {
    let mut polls = 0u32;
    loop {
        let exited = task.child.try_wait()?;
        // Forward even if the leader just exited: others in its group may
        // still be running.
        let pending = signals.take();
        if !pending.is_empty() {
            forwarder.forward(pending, |signal| {
                signal_looked(&task.group, descendants, signal, exited.is_none())
            });
        }
        if let Some(status) = exited {
            return Ok((status, forwarder.stopped()));
        }
        polls = polls.wrapping_add(1);
        if polls % LOOK_EVERY == FIRST_LOOK {
            descendants.look(&task.group);
        }
        if polls.is_multiple_of(CANCEL_CHECK_EVERY)
            && let Some(signal) = canceled(forwarder.stop_sent())
        {
            // Not waited for yet: it had not exited when this poll began.
            forwarder.send_with(signal, |signal| {
                signal_looked(&task.group, descendants, signal, true)
            });
        }
        thread::sleep(CHILD_POLL_INTERVAL);
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

    /// What `forwarder` sends for each batch of `received` signals.
    fn sent(forwarder: &mut Forwarder, batches: &[&[i32]]) -> Vec<i32> {
        let mut sent = Vec::new();
        for batch in batches {
            forwarder.forward(batch.to_vec(), |signal| {
                sent.push(signal);
                Sent::ToLeader
            });
        }
        sent
    }

    #[test]
    fn a_stop_request_is_sent_as_the_stop_signal_once() {
        use libc::{SIGHUP, SIGINT, SIGTERM};
        // The default: SIGTERM once, SIGINT and SIGHUP each time.
        let mut default = Forwarder::new(SIGTERM);
        let batches: &[&[i32]] = &[&[SIGTERM], &[SIGINT, SIGTERM], &[SIGHUP]];
        assert_eq!(sent(&mut default, batches), [SIGTERM, SIGINT, SIGHUP]);
        // Stopped with SIGINT: a Ctrl-C passes each time and counts as the
        // stop, a SIGTERM becomes it.
        let mut int = Forwarder::new(SIGINT);
        assert_eq!(sent(&mut int, &[&[SIGTERM], &[SIGTERM]]), [SIGINT]);
        let mut int = Forwarder::new(SIGINT);
        let batches: &[&[i32]] = &[&[SIGINT], &[SIGINT, SIGTERM]];
        assert_eq!(sent(&mut int, batches), [SIGINT, SIGINT]);
        // Stopped with SIGHUP, which comes with a SIGTERM in one batch (in
        // the order they are taken): one SIGHUP.
        let mut hup = Forwarder::new(SIGHUP);
        assert_eq!(sent(&mut hup, &[&[SIGTERM, SIGHUP]]), [SIGHUP]);
        assert!(hup.stop_sent());
    }

    #[test]
    fn only_a_stop_that_reached_the_running_command_makes_it_stopped() {
        // What it left gets the stop, once; the command had exited.
        let mut forwarder = Forwarder::new(libc::SIGTERM);
        forwarder.forward(vec![libc::SIGTERM], |_| Sent::ToMembers);
        assert!(forwarder.stop_sent() && !forwarder.stopped());
        let mut forwarder = Forwarder::new(libc::SIGTERM);
        forwarder.forward(vec![libc::SIGTERM], |_| Sent::ToLeader);
        assert!(forwarder.stop_sent() && forwarder.stopped());
        // Another signal is no stop, wherever it went.
        let mut forwarder = Forwarder::new(libc::SIGTERM);
        forwarder.forward(vec![libc::SIGINT], |_| Sent::ToLeader);
        assert!(!forwarder.stop_sent() && !forwarder.stopped());
    }

    #[test]
    fn a_signal_after_the_command_exited_goes_to_what_it_left() {
        use std::os::unix::process::CommandExt;
        // The leader exits at once, leaving `sleep` in its group; it is not
        // waited for yet.
        let mut leader = Command::new("sh")
            .args(["-c", "sleep 30 >/dev/null & echo $!"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let group = ProcessGroup {
            pgid: leader.id(),
            leader_start_time: ProcessId::of(leader.id()).unwrap().map(|p| p.start_time),
        };
        let output = std::mem::take(&mut leader.stdout);
        let sleeper: u32 = std::io::read_to_string(output.unwrap())
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while ProcessId::of(leader.id()).unwrap().is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "the leader never exited"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let mut descendants = Descendants::default();
        assert_eq!(
            signal_looked(&group, &mut descendants, libc::SIGTERM, true),
            Sent::ToMembers
        );
        while ProcessId::of(sleeper).unwrap().is_some() {
            assert!(std::time::Instant::now() < deadline, "`sleep` got nothing");
            thread::sleep(Duration::from_millis(10));
        }
        leader.wait().unwrap();
    }

    #[test]
    fn a_stop_signal_that_was_refused_may_be_sent_again() {
        let mut forwarder = Forwarder::new(libc::SIGINT);
        forwarder.forward(vec![libc::SIGTERM], |_| Sent::No);
        assert!(!forwarder.stop_sent());
        assert_eq!(sent(&mut forwarder, &[&[libc::SIGTERM]]), [libc::SIGINT]);
    }

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
    fn a_signal_reaches_a_task_whose_leader_is_not_waited_for_whatever_its_start_time() {
        use std::os::unix::process::CommandExt;
        let mut leader = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        // Its start time misread: checked by it, the task gets nothing.
        let start_time = ProcessId::of(leader.id()).unwrap().unwrap().start_time;
        let group = ProcessGroup {
            pgid: leader.id(),
            leader_start_time: Some(start_time + 1),
        };
        let mut descendants = Descendants::default();
        let mut looked =
            |unwaited| signal_looked(&group, &mut descendants, libc::SIGTERM, unwaited);
        assert_eq!(looked(false), Sent::No);
        assert!(leader.try_wait().unwrap().is_none());
        assert_eq!(looked(true), Sent::ToLeader);
        assert_eq!(leader.wait().unwrap().signal(), Some(libc::SIGTERM));
    }

    /// A task whose leader's start time was misread, waited for with
    /// `received` signals pending (no handler is installed) and `canceled`.
    fn wait_misread(received: &[i32], canceled: Option<i32>) -> (ExitStatus, bool) {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        // Whatever this test process inherited, as a task gets its stop
        // signal.
        for &signal in received.iter().chain(&canceled) {
            deliver_stop_signal(&mut cmd, signal);
        }
        let mut task = spawn(&mut cmd, |_| Ok::<(), ()>(())).unwrap();
        task.group.leader_start_time = task.group.leader_start_time.map(|t| t + 1);
        let signals = Signals {
            pending: INTERCEPTED_SIGNALS.map(|signal| {
                (
                    signal,
                    Arc::new(AtomicBool::new(received.contains(&signal))),
                )
            }),
        };
        let mut forwarder = Forwarder::new(libc::SIGTERM);
        wait(
            &mut task,
            &signals,
            &mut forwarder,
            &mut Descendants::default(),
            |_| canceled,
        )
        .unwrap()
    }

    #[test]
    fn a_stop_that_came_after_the_command_exited_does_not_make_it_stopped() {
        // It fails, leaving `sleep` in its group, as the stop comes.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 >/dev/null & exit 1"]);
        let mut task = spawn(&mut cmd, |_| Ok::<(), ()>(())).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while ProcessId::of(task.child.id()).unwrap().is_some() {
            assert!(std::time::Instant::now() < deadline, "it never exited");
            thread::sleep(Duration::from_millis(10));
        }
        let signals = Signals {
            pending: INTERCEPTED_SIGNALS
                .map(|signal| (signal, Arc::new(AtomicBool::new(signal == libc::SIGTERM)))),
        };
        let mut forwarder = Forwarder::new(libc::SIGTERM);
        let (status, stopped) = wait(
            &mut task,
            &signals,
            &mut forwarder,
            &mut Descendants::default(),
            |_| None,
        )
        .unwrap();
        assert_eq!((status.code(), stopped), (Some(1), false));
        // What it left had the stop, so a request to stop sends none again.
        assert!(forwarder.stop_sent());
    }

    #[test]
    fn signals_reach_a_running_task_whatever_its_leaders_start_time() {
        // A SIGINT is not its stop signal; a cancel's request to stop
        // reached it while it ran.
        let (status, stopped) = wait_misread(&[libc::SIGINT], None);
        assert_eq!((status.signal(), stopped), (Some(libc::SIGINT), false));
        let (status, stopped) = wait_misread(&[], Some(libc::SIGTERM));
        assert_eq!((status.signal(), stopped), (Some(libc::SIGTERM), true));
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

    /// Gated spawns from several threads at once, half of them failing to
    /// register, must neither deadlock nor run a task they shouldn't.
    #[test]
    fn concurrent_spawns_do_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let workers: Vec<_> = (0..8)
            .map(|t| {
                let dir = dir.path().to_owned();
                thread::spawn(move || {
                    for i in 0..5 {
                        let marker = dir.join(format!("ran-{t}-{i}"));
                        let mut cmd = Command::new("touch");
                        cmd.arg(&marker);
                        let fail = (t + i) % 2 == 0;
                        let result = spawn(&mut cmd, move |_| if fail { Err(()) } else { Ok(()) });
                        match result {
                            Ok(mut task) => {
                                assert!(!fail);
                                assert!(task.child.wait().unwrap().success());
                                assert!(marker.exists());
                            }
                            Err(_) => assert!(fail && !marker.exists()),
                        }
                    }
                })
            })
            .collect();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        for worker in workers {
            while !worker.is_finished() {
                assert!(std::time::Instant::now() < deadline, "spawns deadlocked");
                thread::sleep(Duration::from_millis(10));
            }
            worker.join().unwrap();
        }
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
