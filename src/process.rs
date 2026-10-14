//! Process identity and liveness.
//!
//! A PID alone is not a stable identity: once a process exits, the OS may hand
//! its PID to an unrelated process. Every PID stored in the queue is therefore
//! paired with the process start time, and a process only counts as alive when
//! both match.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::time::Duration;

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

    /// Whether this exact process is stopped (Ctrl-Z, SIGSTOP), so that it
    /// acts on nothing until it is continued. False when the OS cannot tell.
    pub fn is_stopped(&self) -> bool {
        inspect(self.pid)
            .ok()
            .flatten()
            .is_some_and(|seen| seen.start_time == self.start_time && seen.stopped)
    }

    /// Sends `signal` to this exact process. `Ok(false)` if it is gone, so a
    /// PID the OS has since given to another process is never hit (but for
    /// a reuse within the instant between the check and the signal).
    pub fn signal(&self, signal: i32) -> io::Result<bool> {
        if start_time(self.pid)? != Some(self.start_time) {
            return Ok(false);
        }
        // SAFETY: kill has no memory-safety preconditions.
        if unsafe { libc::kill(self.pid as libc::pid_t, signal) } == 0 {
            return Ok(true);
        }
        match io::Error::last_os_error() {
            e if e.raw_os_error() == Some(libc::ESRCH) => Ok(false),
            e => Err(e),
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

    /// Whether the group's leader, the task's own command, is known to have
    /// exited. False if its start time was never read.
    pub fn leader_exited(&self) -> bool {
        self.leader_start_time.is_some_and(|start_time| {
            !ProcessId {
                pid: self.pgid,
                start_time,
            }
            .is_alive()
        })
    }

    /// The group's running members, with their command names, as far as
    /// they can be seen: what holds a slot once the task's command exited.
    pub fn members(&self) -> Vec<Descendant> {
        let Ok(pgid) = libc::pid_t::try_from(self.pgid) else {
            return Vec::new();
        };
        let Ok(listing) = list_group(pgid) else {
            return Vec::new();
        };
        listing
            .members
            .into_iter()
            .filter_map(|pid| {
                let o = observe(pid).filter(|o| o.pgid == self.pgid)?;
                Some(Descendant {
                    pid,
                    start_time: o.start_time,
                    name: o.name,
                })
            })
            .collect()
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

/// What is known of a running process.
struct Seen {
    start_time: u64,
    /// Stopped by a signal (Ctrl-Z, SIGSTOP), which SIGCONT ends.
    stopped: bool,
}

/// Start time of `pid`, or `None` if no such (non-zombie) process exists.
fn start_time(pid: u32) -> io::Result<Option<u64>> {
    Ok(inspect(pid)?.map(|seen| seen.start_time))
}

/// What is known of `pid`, or `None` if no such (non-zombie) process exists.
///
/// `proc_pidinfo` is fast (well under a microsecond) but fails with EPERM for
/// other users' processes, and a dead job's PID may since have gone to one.
/// Those fall back to `sysctl(KERN_PROC_PID)`, which works for any process but
/// is about 40 times slower.
#[cfg(target_os = "macos")]
fn inspect(pid: u32) -> io::Result<Option<Seen>> {
    const SSTOP: u32 = 4;
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
        return Ok(Some(Seen {
            start_time: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
            stopped: info.pbi_status == SSTOP,
        }));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => Ok(None),
        Some(libc::EPERM) => inspect_any_user(pid),
        _ => Err(err),
    }
}

/// [`inspect`] for a process of any user, via `sysctl(KERN_PROC_PID)`.
#[cfg(target_os = "macos")]
fn inspect_any_user(pid: libc::c_int) -> io::Result<Option<Seen>> {
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
    const SSTOP: c_char = 4;
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
    Ok(Some(Seen {
        start_time: start.tv_sec as u64 * 1_000_000 + start.tv_usec as u64,
        stopped: head.p_stat == SSTOP,
    }))
}

/// What is known of `pid`, or `None` if no such (non-zombie) process exists.
#[cfg(target_os = "linux")]
fn inspect(pid: u32) -> io::Result<Option<Seen>> {
    Ok(read_stat(pid)?
        .filter(|stat| stat.running)
        .map(|stat| Seen {
            start_time: stat.start_time,
            stopped: stat.stopped,
        }))
}

#[cfg(target_os = "linux")]
struct Stat {
    /// False for a process that has fully exited (a zombie).
    running: bool,
    /// Stopped by a signal (T). A debugger's stop (t) is not: SIGCONT
    /// does not end it.
    stopped: bool,
    ppid: u32,
    pgrp: u32,
    start_time: u64,
    /// The command name (comm), lossy.
    name: String,
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
    let open = stat
        .iter()
        .position(|&b| b == b'(')
        .filter(|&open| open < close);
    let name = open.map_or_else(String::new, |open| {
        String::from_utf8_lossy(&stat[open + 1..close]).into_owned()
    });
    let rest = std::str::from_utf8(&stat[close + 1..]).map_err(|_| malformed())?;
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    // fields[i] is field i + 3: state (3), ppid (4), pgrp (5), num_threads
    // (20), starttime (22).
    let field = |i: usize| fields.get(i).copied().ok_or_else(malformed);
    let state = field(0)?;
    let running = match state {
        "X" | "x" => false,
        // A thread-group leader that called pthread_exit shows as a zombie
        // while its other threads keep the process running. Field 20 is
        // num_threads.
        "Z" => field(17)?.parse::<u32>().map_err(|_| malformed())? > 1,
        _ => true,
    };
    Ok(Some(Stat {
        running,
        stopped: state == "T",
        ppid: field(1)?.parse().map_err(|_| malformed())?,
        pgrp: field(2)?.parse().map_err(|_| malformed())?,
        start_time: field(19)?.parse().map_err(|_| malformed())?,
        name,
    }))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("loadmaster supports macOS and Linux only");

/// The CPUs online in the whole system, which is what the load average is
/// measured against (`available_parallelism` counts only those this process
/// may use, which a container can limit).
pub fn online_cpus() -> Option<usize> {
    // SAFETY: sysconf has no memory-safety preconditions.
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    usize::try_from(n).ok().filter(|&n| n > 0)
}

/// The system's 1-minute load average, if the OS reports one.
pub fn load_average() -> Option<f64> {
    let mut load = [0.0f64; 1];
    // SAFETY: the buffer holds the one element requested.
    let n = unsafe { libc::getloadavg(load.as_mut_ptr(), 1) };
    (n == 1 && load[0].is_finite()).then_some(load[0])
}

/// The whole machine's CPU time so far, in clock ticks summed over its CPUs:
/// what was busy, and all of it. It counts every process, inside the queue
/// or not, and only CPU time (not waits for disks, as the load average does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTicks {
    pub busy: u64,
    pub total: u64,
}

impl CpuTicks {
    /// The share of the machine's CPU time that was busy from `earlier` to
    /// this reading, or `None` if no tick passed or a count went back (a
    /// counter wrapped, or Linux's iowait, which can, did).
    pub fn busy_since(&self, earlier: CpuTicks) -> Option<f64> {
        let busy = self.busy.checked_sub(earlier.busy)?;
        let idle = (self.total - self.busy).checked_sub(earlier.total - earlier.busy)?;
        let total = busy + idle;
        (total > 0).then(|| busy as f64 / total as f64)
    }
}

/// The machine's [`CpuTicks`] now, summed over its CPUs, if the OS reports
/// them. Each CPU's counters, unlike the machine's (`host_statistics`), are
/// kept up to date: the machine's are added up about once a second, too
/// rarely to measure a quarter of a second by. They are 32 bits and count
/// 100 times a second, so one wraps after 497 days of uptime: the share
/// across that is not measured.
#[cfg(target_os = "macos")]
pub fn cpu_ticks() -> Option<CpuTicks> {
    use std::sync::OnceLock;
    // Each call of mach_host_self adds a reference to the port; one is enough.
    static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
    // libc deprecates its Mach bindings for the mach2 crate; a few calls are
    // not worth a dependency.
    // SAFETY: mach_host_self has no preconditions.
    #[allow(deprecated)]
    let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
    let mut cpus: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut count: libc::mach_msg_type_number_t = 0;
    // SAFETY: the out-pointers are valid for the call, which allocates the
    // array in this task.
    let r = unsafe {
        libc::host_processor_info(
            host,
            libc::PROCESSOR_CPU_LOAD_INFO,
            &mut cpus,
            &mut info,
            &mut count,
        )
    };
    if r != libc::KERN_SUCCESS || info.is_null() {
        return None;
    }
    // SAFETY: the call returned `count` integers at `info`.
    let values = unsafe { std::slice::from_raw_parts(info.cast::<u32>(), count as usize) };
    let (mut busy, mut total) = (0u64, 0u64);
    let (per_cpu, _) = values.as_chunks::<{ libc::CPU_STATE_MAX as usize }>();
    for cpu in per_cpu {
        let tick = |state: libc::c_int| u64::from(cpu[state as usize]);
        let used =
            tick(libc::CPU_STATE_USER) + tick(libc::CPU_STATE_SYSTEM) + tick(libc::CPU_STATE_NICE);
        busy += used;
        total += used + tick(libc::CPU_STATE_IDLE);
    }
    // SAFETY: the array was allocated for this task by the call, with this
    // size, and is not used after.
    #[allow(deprecated)]
    unsafe {
        libc::vm_deallocate(
            libc::mach_task_self(),
            info as libc::vm_address_t,
            count as usize * std::mem::size_of::<libc::integer_t>(),
        );
    }
    Some(CpuTicks { busy, total })
}

/// The machine's [`CpuTicks`] now, from the first line of `/proc/stat`. Time
/// waiting for disks (iowait) counts as idle; time stolen by a hypervisor
/// counts as busy, since this machine could not use it.
#[cfg(target_os = "linux")]
pub fn cpu_ticks() -> Option<CpuTicks> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?.strip_prefix("cpu ")?;
    // user nice system idle iowait irq softirq steal (guest time is in user).
    let fields: Vec<u64> = line
        .split_ascii_whitespace()
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let total: u64 = fields.iter().sum();
    let idle = fields.get(3)? + fields.get(4).copied().unwrap_or(0);
    Some(CpuTicks {
        busy: total - idle,
        total,
    })
}

/// CPU time (user plus system) used by this process's children that have
/// exited and been waited for, and by their own waited-for descendants.
pub fn children_cpu_time() -> Option<Duration> {
    // SAFETY: an all-zero `rusage` is a valid value to be overwritten.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: the pointer is valid for the call.
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) } != 0 {
        return None;
    }
    let time = |t: libc::timeval| {
        Duration::from_secs(u64::try_from(t.tv_sec).ok()?)
            .checked_add(Duration::from_micros(u64::try_from(t.tv_usec).ok()?))
    };
    time(usage.ru_utime)?.checked_add(time(usage.ru_stime)?)
}

/// A process a task started, as seen while it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descendant {
    pub pid: u32,
    pub start_time: u64,
    /// Its command name, as the OS reports it.
    pub name: String,
}

/// The processes a task started that still run, found by walking the process
/// tree from its leader, and from those found before, every so often while
/// it runs. One that starts a session of its own leaves the task's process
/// group, which is all that holds the task's slot and gets its signals; once
/// its parent exits it belongs to PID 1, and only having seen it before tells
/// it from any other. One whose parent exits between two looks is missed:
/// the OS keeps no record of where a process came from (macOS does not even
/// show another process's environment, which would carry LOADMASTER_HELD).
#[derive(Debug, Default)]
pub struct Descendants {
    seen: BTreeMap<u32, Descendant>,
}

impl Descendants {
    /// Walks the tree under the leader of `group` and under every process
    /// seen before that still runs, and forgets those that have exited.
    /// Each process is checked by its start time, and each child by its
    /// parent, so a PID the OS hands out again meanwhile is not taken for
    /// one of them.
    pub fn look(&mut self, group: &ProcessGroup) {
        self.seen
            .retain(|_, seen| observe(seen.pid).is_some_and(|o| o.start_time == seen.start_time));
        let mut queue: Vec<(u32, Option<u64>)> =
            std::iter::once((group.pgid, group.leader_start_time))
                .chain(
                    self.seen
                        .values()
                        .map(|seen| (seen.pid, Some(seen.start_time))),
                )
                .collect();
        let mut visited = BTreeSet::new();
        while let Some((pid, start)) = queue.pop() {
            if !visited.insert(pid) {
                continue;
            }
            let found: Vec<(u32, Observed)> = children(pid)
                .into_iter()
                .filter_map(|child| Some((child, observe(child).filter(|o| o.ppid == pid)?)))
                .collect();
            // Still the process whose children those are, after they were
            // looked at.
            if !observe(pid).is_some_and(|o| start.is_none_or(|t| o.start_time == t)) {
                continue;
            }
            for (child, o) in found {
                queue.push((child, Some(o.start_time)));
                self.seen.insert(
                    child,
                    Descendant {
                        pid: child,
                        start_time: o.start_time,
                        name: o.name,
                    },
                );
            }
        }
    }

    /// Those seen that still run outside process group `group`.
    pub fn outside_group(&self, group: u32) -> Vec<Descendant> {
        self.seen
            .values()
            .filter(|seen| {
                observe(seen.pid)
                    .is_some_and(|o| o.start_time == seen.start_time && o.pgid != group)
            })
            .cloned()
            .collect()
    }
}

/// What one look at a running process tells.
struct Observed {
    start_time: u64,
    ppid: u32,
    pgid: u32,
    name: String,
}

/// `pid`, if it runs (not a zombie) and may be looked at.
#[cfg(target_os = "macos")]
fn observe(pid: u32) -> Option<Observed> {
    let info = bsd_info(pid)?;
    // pbi_name is the longer name, when the process has one.
    let name = if info.pbi_name[0] != 0 {
        &info.pbi_name[..]
    } else {
        &info.pbi_comm[..]
    };
    let name: Vec<u8> = name
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    Some(Observed {
        start_time: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        ppid: info.pbi_ppid,
        pgid: info.pbi_pgid,
        name: String::from_utf8_lossy(&name).into_owned(),
    })
}

/// The process's BSD info, unless it is gone, a zombie, or another user's.
#[cfg(target_os = "macos")]
fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    const SZOMB: u32 = 5;
    let pid = libc::c_int::try_from(pid).ok().filter(|&pid| pid > 0)?;
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
    // SAFETY: proc_pidinfo filled the whole struct.
    (n == size)
        .then(|| unsafe { info.assume_init() })
        .filter(|info| info.pbi_status != SZOMB)
}

/// The children of `pid`; none if it is gone.
#[cfg(target_os = "macos")]
fn children(pid: u32) -> Vec<u32> {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return Vec::new();
    };
    let mut pids: Vec<libc::pid_t> = vec![0; 16];
    loop {
        let Ok(size) = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice())) else {
            return Vec::new();
        };
        // SAFETY: the buffer is valid for writes of `size` bytes.
        let n = unsafe { libc::proc_listchildpids(pid, pids.as_mut_ptr().cast(), size) };
        // A number of PIDs, as with proc_listpgrppids.
        let Ok(count) = usize::try_from(n) else {
            return Vec::new();
        };
        if count < pids.len() {
            pids.truncate(count);
            break;
        }
        pids.resize(pids.len() * 2, 0);
    }
    pids.into_iter()
        .filter(|&pid| pid > 0)
        .map(|pid| pid as u32)
        .collect()
}

/// `pid`, if it runs (not a zombie) and may be looked at.
#[cfg(target_os = "linux")]
fn observe(pid: u32) -> Option<Observed> {
    let stat = read_stat(pid).ok()??;
    stat.running.then_some(Observed {
        start_time: stat.start_time,
        ppid: stat.ppid,
        pgid: stat.pgrp,
        name: stat.name,
    })
}

/// The children of `pid`, from each of its threads' `children` file; none
/// if it is gone.
#[cfg(target_os = "linux")]
fn children(pid: u32) -> Vec<u32> {
    let Ok(threads) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for thread in threads.flatten() {
        if let Ok(list) = std::fs::read_to_string(thread.path().join("children")) {
            pids.extend(
                list.split_ascii_whitespace()
                    .filter_map(|p| p.parse::<u32>().ok()),
            );
        }
    }
    pids
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn busy_cpu_time_shows_in_the_machine_ticks() {
        let cores = online_cpus().unwrap();
        // A container's quota can leave this process fewer.
        let usable = std::thread::available_parallelism().unwrap().get();
        let before = cpu_ticks().expect("the OS reports CPU ticks");
        // Every core it may use busy for a while: the share busy is up.
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            for _ in 0..usable {
                scope.spawn(|| {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(500));
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let after = cpu_ticks().unwrap();
        // Seen, at least: a quota can keep the threads to a share of a core
        // (tests/outside.rs checks a machine kept busy).
        let busy = after.busy_since(before).unwrap();
        assert!(busy > 0.0, "busy share {busy}");
        assert!(busy <= 1.0);
        assert_eq!(after.busy_since(after), None, "no tick passed");
        // A quarter of a second is enough to measure: the counters move
        // all the time, not once a second.
        for _ in 0..4 {
            let from = cpu_ticks().unwrap();
            std::thread::sleep(Duration::from_millis(250));
            let to = cpu_ticks().unwrap();
            let ticks = to.total - from.total;
            assert!(ticks >= u64::from(cores as u32) * 10, "{ticks} ticks");
        }
    }

    #[test]
    fn a_count_that_went_back_measures_nothing() {
        let earlier = CpuTicks {
            busy: 100,
            total: 1_000,
        };
        let later = |busy, idle| CpuTicks {
            busy,
            total: busy + idle,
        };
        assert_eq!(later(130, 910).busy_since(earlier), Some(0.75));
        // A CPU's idle count wrapped, so the sum dropped.
        assert_eq!(later(130, 4).busy_since(earlier), None);
        assert_eq!(later(90, 950).busy_since(earlier), None);
    }

    #[test]
    fn load_average_is_reported() {
        assert!(load_average().is_some_and(|load| load >= 0.0));
    }

    #[test]
    fn cpu_time_of_children_is_counted_once_waited_for() {
        let before = children_cpu_time().unwrap();
        // A fixed amount of work in grandchildren: well above the 50 ms
        // asked for even on a fast machine (100 MB took 40 ms in a Linux
        // container on an M-series Mac, and once fell short with other tests
        // running).
        let status = Command::new("sh")
            .args(["-c", "sh -c 'head -c 400000000 /dev/zero | cksum'"])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let used = children_cpu_time().unwrap() - before;
        assert!(used >= Duration::from_millis(50), "{used:?}");
    }

    #[test]
    fn current_process_is_alive() {
        let me = ProcessId::current().unwrap();
        assert_eq!(me.pid, std::process::id());
        assert!(me.is_alive());
    }

    /// Checks the hand-written `kinfo_proc` layout against `proc_pidinfo`.
    #[cfg(target_os = "macos")]
    #[test]
    fn both_macos_sources_agree() {
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let pid = child.id();
        for stopped in [false, true] {
            if stopped {
                // SAFETY: kill has no memory-safety preconditions.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGSTOP) };
                std::thread::sleep(Duration::from_millis(100));
            }
            let fast = inspect(pid).unwrap().unwrap();
            let any_user = inspect_any_user(pid as libc::c_int).unwrap().unwrap();
            assert_eq!(
                (fast.start_time, fast.stopped),
                (any_user.start_time, any_user.stopped)
            );
            assert_eq!(fast.stopped, stopped);
        }
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_stopped_process_is_stopped_until_continued() {
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let process = ProcessId::of(child.id()).unwrap().unwrap();
        assert!(!process.is_stopped());
        let wait_for = |stopped: bool| {
            for _ in 0..100 {
                if process.is_stopped() == stopped {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("never became stopped={stopped}");
        };
        assert!(process.signal(libc::SIGSTOP).unwrap());
        wait_for(true);
        assert!(process.is_alive(), "stopped is not gone");
        assert!(process.signal(libc::SIGCONT).unwrap());
        wait_for(false);
        child.kill().unwrap();
        child.wait().unwrap();
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
