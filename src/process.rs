//! Process identity and liveness.
//!
//! A PID alone is not a stable identity: once a process exits, the OS may hand
//! its PID to an unrelated process. Every PID stored in the queue is therefore
//! paired with the process start time, and a process only counts as alive when
//! both match.

use std::io;

/// A process, identified by PID plus start time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessId {
    pub pid: u32,
    /// OS-specific start time token (µs since epoch on macOS, clock ticks since
    /// boot on Linux). Only ever compared for equality.
    pub start_time: u64,
}

impl ProcessId {
    /// The calling process.
    pub fn current() -> io::Result<ProcessId> {
        Self::of(std::process::id())?
            .ok_or_else(|| io::Error::other("cannot read this process's start time"))
    }

    /// Looks up a running process by PID. `Ok(None)` means there is no such
    /// process (zombies count as gone).
    pub fn of(pid: u32) -> io::Result<Option<ProcessId>> {
        Ok(start_time(pid)?.map(|start_time| ProcessId { pid, start_time }))
    }

    /// Whether this exact process is still running.
    ///
    /// Errs on the side of "alive" when the OS cannot tell, so a live job's
    /// slot is never handed to someone else.
    pub fn is_alive(&self) -> bool {
        match start_time(self.pid) {
            Ok(Some(t)) => t == self.start_time,
            Ok(None) => false,
            Err(_) => true,
        }
    }
}

/// A child process group, identified by its leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessGroup {
    pub pgid: u32,
    /// Start time of the leader, if it could be read before the leader exited.
    pub leader_start_time: Option<u64>,
}

impl ProcessGroup {
    /// Whether any process in this group is still running (zombies don't count).
    pub fn is_alive(&self) -> bool {
        match (start_time(self.pgid), self.leader_start_time) {
            // The leader itself is still running.
            (Ok(Some(t)), Some(expected)) if t == expected => true,
            // The PID now belongs to a different process. The OS does not reuse
            // a PID while a process group with that ID exists, so ours is gone.
            (Ok(Some(_)), Some(_)) => false,
            // The leader is gone (or unknown); other members may remain.
            (Ok(_), _) => has_running_member(self.pgid).unwrap_or(true),
            (Err(_), _) => true,
        }
    }

    /// Sends `signal` to every process in the group.
    pub fn signal(&self, signal: i32) -> io::Result<()> {
        // SAFETY: killpg has no memory-safety preconditions.
        if unsafe { libc::killpg(self.pgid as libc::pid_t, signal) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// Whether a running (non-zombie) process with process group ID `pgid` exists.
fn has_running_member(pgid: u32) -> io::Result<bool> {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return Ok(false);
    };
    if pgid <= 1 {
        return Ok(false);
    }
    // Members can fork and exit while we look: one found dead may have started
    // another after it was listed. "No running member" is only trusted once a
    // complete listing of zombies repeats exactly: every member of the earlier
    // listing was then already dead when the later one was taken, so anything
    // they started would have shown up in it.
    let mut previous: Option<Vec<u32>> = None;
    for _ in 0..10 {
        // ESRCH: no process at all, not even a zombie, is in the group.
        // SAFETY: signal 0 only checks for existence.
        if unsafe { libc::killpg(pgid, 0) } != 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return Ok(false);
        }
        let listing = list_group(pgid)?;
        for &pid in &listing.members {
            if start_time(pid)?.is_some() {
                return Ok(true);
            }
        }
        // An empty listing while the group exists means a member is hidden
        // from us or was missed: no evidence either way.
        let settled = listing.complete && !listing.members.is_empty();
        if settled && previous.as_ref() == Some(&listing.members) {
            return Ok(false);
        }
        previous = settled.then_some(listing.members);
    }
    Ok(true)
}

/// The members of a process group, zombies included.
struct Listing {
    /// Sorted PIDs.
    members: Vec<u32>,
    /// False if some process could not be inspected and so might be a member.
    complete: bool,
}

#[cfg(target_os = "macos")]
fn list_group(pgid: libc::pid_t) -> io::Result<Listing> {
    let mut pids: Vec<libc::pid_t> = vec![0; 64];
    loop {
        let size = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice()))
            .map_err(io::Error::other)?;
        // libproc reports failure as 0 with errno set, so clear errno first.
        // SAFETY: __error returns this thread's errno location; the buffer is
        // valid for writes of `size` bytes.
        let n = unsafe {
            *libc::__error() = 0;
            libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), size)
        };
        let err = io::Error::last_os_error();
        if n < 0 || (n == 0 && err.raw_os_error() != Some(0)) {
            return Err(err);
        }
        // Unlike proc_listpids, this returns a number of PIDs, not bytes.
        let count = n as usize;
        if count < pids.len() {
            pids.truncate(count);
            break;
        }
        // The buffer may have been too small; retry with a bigger one.
        pids.resize(pids.len() * 2, 0);
    }
    let mut members: Vec<u32> = pids
        .into_iter()
        .filter(|&pid| pid > 0)
        .map(|pid| pid as u32)
        .collect();
    members.sort_unstable();
    // The kernel copies the member list in one go.
    Ok(Listing {
        members,
        complete: true,
    })
}

#[cfg(target_os = "linux")]
fn list_group(pgid: libc::pid_t) -> io::Result<Listing> {
    let mut listing = Listing {
        members: Vec::new(),
        // With hidepid, some processes are not listed at all.
        complete: proc_shows_all_pids(),
    };
    for entry in std::fs::read_dir("/proc")? {
        let Some(pid) = entry?.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        match read_stat(pid) {
            Ok(Some(stat)) if stat.pgrp == pgid as u32 => listing.members.push(pid),
            Ok(Some(_)) => {}
            // It vanished mid-scan or we may not read it: it could have been
            // a member, possibly one that started another.
            Ok(None) | Err(_) => listing.complete = false,
        }
    }
    listing.members.sort_unstable();
    Ok(listing)
}

/// Whether `/proc` lists every process, i.e. is not mounted with `hidepid`.
#[cfg(target_os = "linux")]
fn proc_shows_all_pids() -> bool {
    static ALL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALL.get_or_init(|| {
        let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return false;
        };
        // Fields: ID parent major:minor root mount-point options ... - type source super-options
        mountinfo.lines().all(|line| {
            let mut halves = line.splitn(2, " - ");
            let (Some(mount), Some(fs)) = (halves.next(), halves.next()) else {
                return true;
            };
            let is_proc =
                mount.split(' ').nth(4) == Some("/proc") && fs.split(' ').next() == Some("proc");
            !is_proc
                || fs
                    .split(' ')
                    .nth(2)
                    .unwrap_or("")
                    .split(',')
                    .filter_map(|option| option.strip_prefix("hidepid="))
                    .all(|value| matches!(value, "0" | "off"))
        })
    })
}

/// Start time of `pid`, or `None` if no such (non-zombie) process exists.
///
/// `proc_pidinfo` is fast (well under a microsecond) but fails with EPERM for
/// other users' processes, and a dead job's PID may since have gone to one.
/// Those fall back to `sysctl(KERN_PROC_PID)`, which works for any process but
/// is about 40 times slower.
#[cfg(target_os = "macos")]
fn start_time(pid: u32) -> io::Result<Option<u64>> {
    const SZOMB: u32 = 5;
    let Ok(pid) = libc::c_int::try_from(pid) else {
        return Ok(None);
    };
    if pid <= 0 {
        return Ok(None);
    }
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is valid for writes of `size` bytes.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if n == size {
        // SAFETY: proc_pidinfo filled the whole struct.
        let info = unsafe { info.assume_init() };
        if info.pbi_status == SZOMB {
            return Ok(None);
        }
        return Ok(Some(
            info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        ));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => Ok(None),
        Some(libc::EPERM) => start_time_any_user(pid),
        _ => Err(err),
    }
}

/// [`start_time`] for a process of any user, via `sysctl(KERN_PROC_PID)`.
#[cfg(target_os = "macos")]
fn start_time_any_user(pid: libc::c_int) -> io::Result<Option<u64>> {
    use std::ffi::{c_char, c_int, c_void};

    /// The leading fields of `struct kinfo_proc` (its `kp_proc` member, a
    /// `struct extern_proc`). This layout has been stable for decades; `ps`
    /// reads the same fields.
    #[repr(C)]
    #[allow(dead_code)] // The unread fields only fix the layout.
    struct KinfoProcHead {
        p_starttime: libc::timeval,
        p_vmspace: *mut c_void,
        p_sigacts: *mut c_void,
        p_flag: c_int,
        p_stat: c_char,
    }
    const SZOMB: c_char = 5;

    // sizeof(struct kinfo_proc) is 648; leave room in case it ever grows.
    let mut buf = [0u8; 2048];
    let mut len = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    // SAFETY: `buf` is valid for writes of `len` bytes.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ESRCH) => Ok(None),
            _ => Err(err),
        };
    }
    if len == 0 {
        return Ok(None);
    }
    if len < std::mem::size_of::<KinfoProcHead>() {
        return Err(io::Error::other("short kinfo_proc from sysctl"));
    }
    // SAFETY: the buffer holds at least a `KinfoProcHead`; the read is unaligned-safe.
    let head: KinfoProcHead = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    if head.p_stat == SZOMB {
        return Ok(None);
    }
    let start = head.p_starttime;
    Ok(Some(start.tv_sec as u64 * 1_000_000 + start.tv_usec as u64))
}

/// Start time of `pid`, or `None` if no such (non-zombie) process exists.
#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> io::Result<Option<u64>> {
    Ok(read_stat(pid)?
        .filter(|stat| stat.running)
        .map(|stat| stat.start_time))
}

#[cfg(target_os = "linux")]
struct Stat {
    /// False for a process that has fully exited (a zombie).
    running: bool,
    pgrp: u32,
    start_time: u64,
}

/// The parts of `/proc/<pid>/stat` we need, or `None` if there is no such
/// process.
#[cfg(target_os = "linux")]
fn read_stat(pid: u32) -> io::Result<Option<Stat>> {
    if pid == 0 {
        return Ok(None);
    }
    let stat = match std::fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let malformed = || io::Error::other(format!("unexpected /proc/{pid}/stat format"));
    // Field 2 (comm) may contain spaces, parentheses and non-UTF-8 bytes, so
    // parse only what follows the last ')'.
    let close = stat
        .iter()
        .rposition(|&b| b == b')')
        .ok_or_else(malformed)?;
    let rest = std::str::from_utf8(&stat[close + 1..]).map_err(|_| malformed())?;
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    // fields[i] is field i + 3: state (3), pgrp (5), num_threads (20),
    // starttime (22).
    let field = |i: usize| fields.get(i).copied().ok_or_else(malformed);
    let running = match field(0)? {
        "X" | "x" => false,
        // A thread-group leader that called pthread_exit shows as a zombie
        // while its other threads keep the process running. Field 20 is
        // num_threads.
        "Z" => field(17)?.parse::<u32>().map_err(|_| malformed())? > 1,
        _ => true,
    };
    Ok(Some(Stat {
        running,
        pgrp: field(2)?.parse().map_err(|_| malformed())?,
        start_time: field(19)?.parse().map_err(|_| malformed())?,
    }))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("loadmaster supports macOS and Linux only");

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn current_process_is_alive() {
        let me = ProcessId::current().unwrap();
        assert_eq!(me.pid, std::process::id());
        assert!(me.is_alive());
    }

    /// Checks the hand-written `kinfo_proc` layout against `proc_pidinfo`.
    #[cfg(target_os = "macos")]
    #[test]
    fn both_macos_start_time_sources_agree() {
        let pid = std::process::id();
        let fast = start_time(pid).unwrap().unwrap();
        let any_user = start_time_any_user(pid as libc::c_int).unwrap().unwrap();
        assert_eq!(fast, any_user);
    }

    #[test]
    fn same_pid_with_different_start_time_is_not_alive() {
        let me = ProcessId::current().unwrap();
        let recycled = ProcessId {
            start_time: me.start_time + 1,
            ..me
        };
        assert!(!recycled.is_alive());
    }

    #[test]
    fn pid_reused_by_another_users_process_is_not_alive() {
        // PID 1 (launchd / init) belongs to root. A dead job whose PID it now
        // holds must count as dead, not as "can't tell".
        let init = ProcessId::of(1).unwrap().expect("PID 1 is readable");
        assert!(init.is_alive());
        let dead_job = ProcessId {
            start_time: init.start_time + 1,
            ..init
        };
        assert!(!dead_job.is_alive());
        let dead_group = ProcessGroup {
            pgid: 1,
            leader_start_time: Some(init.start_time + 1),
        };
        assert!(!dead_group.is_alive());
    }

    /// A thread-group leader that called pthread_exit shows as a zombie on
    /// Linux while its other threads keep running.
    #[cfg(target_os = "linux")]
    #[test]
    fn process_whose_main_thread_exited_is_alive() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("t.c");
        let bin = dir.path().join("t");
        std::fs::write(
            &src,
            "#include <pthread.h>\n#include <unistd.h>\n\
             static void *work(void *a) { (void)a; sleep(30); return 0; }\n\
             int main(void) { pthread_t t; pthread_create(&t, 0, work, 0); pthread_exit(0); }\n",
        )
        .unwrap();
        let compiled = Command::new("cc")
            .arg("-pthread")
            .arg("-o")
            .arg(&bin)
            .arg(&src)
            .status();
        if !compiled.is_ok_and(|s| s.success()) {
            eprintln!("skipping: no C compiler");
            return;
        }
        let mut child = Command::new(&bin).spawn().unwrap();
        let id = ProcessId::of(child.id()).unwrap().unwrap();
        let stat = format!("/proc/{}/stat", child.id());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !std::fs::read_to_string(&stat).unwrap().contains(") Z ") {
            assert!(
                std::time::Instant::now() < deadline,
                "main thread never exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(id.is_alive());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!id.is_alive());
    }

    #[test]
    fn exited_process_is_not_alive() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let id = ProcessId::of(child.id()).unwrap().unwrap();
        assert!(id.is_alive());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!id.is_alive());
        assert_eq!(ProcessId::of(child.id()).unwrap(), None);
    }

    #[test]
    fn zombie_is_not_alive() {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        // Wait for the child to exit without reaping it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ProcessId::of(pid).unwrap().is_some() {
            assert!(std::time::Instant::now() < deadline, "child never exited");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.wait().unwrap();
    }

    #[test]
    fn process_group_outlives_its_leader() {
        use std::os::unix::process::CommandExt;
        // The leader exits right away, leaving `sleep` behind in the group.
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
        leader.wait().unwrap();
        assert!(group.is_alive(), "group should live while `sleep` runs");

        group.signal(libc::SIGKILL).unwrap();
        // The orphaned `sleep` may linger briefly as a zombie until reaped.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while group.is_alive() {
            assert!(std::time::Instant::now() < deadline, "group never died");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(ProcessId::of(sleeper).unwrap(), None);
    }
}
