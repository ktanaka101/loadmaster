//! Soak test: several simulated agents drive loadmaster the way Claude Code's
//! Bash tool does, for a while, with timeouts, kills and crashes, in two
//! checkouts and with two resources besides CPU, while jobs are canceled at
//! random.
//!
//! Ignored by default. Run it with:
//!
//! ```text
//! LOADMASTER_SOAK_SECS=600 cargo test --release --test soak -- --ignored --nocapture
//! ```

mod common;

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use common::*;

const CAPACITY: u32 = 8;
/// `db`: at most 2 at once, 1 per checkout. `mem`: at most 6.
const RESOURCES: &str = "[resources.db]\ncapacity = 2\nper_checkout = 1\n\n\
                         [resources.mem]\ncapacity = 6\n\n";
const CHECKOUTS: [&str; 2] = ["repo", "repo2"];
const AGENTS: usize = 5;
/// Delay between SIGTERM and SIGKILL (a few seconds in Claude Code; shorter here).
const KILL_GRACE: Duration = Duration::from_secs(1);

/// (name, weight, db, mem, work). `slow` takes a second to shut down after
/// SIGTERM.
const TASKS: [(&str, u32, u32, u32, &str); 5] = [
    ("lint", 1, 0, 0, "sleep 0.3"),
    ("test", 4, 0, 3, "sleep 1"),
    ("e2e", 2, 1, 2, "sleep 2"),
    ("big", 8, 0, 0, "sleep 1"),
    ("slow", 2, 1, 4, "trap 'sleep 1; exit 143' TERM; sleep 3"),
];

#[derive(Default)]
struct Stats {
    runs: AtomicUsize,
    ok: AtomicUsize,
    ok_by_task: [AtomicUsize; TASKS.len()],
    gave_up: AtomicUsize,
    timed_out: AtomicUsize,
    crashed: AtomicUsize,
    canceled: AtomicUsize,
}

#[test]
#[ignore = "takes minutes; run explicitly"]
fn agents_under_timeouts_and_crashes() {
    let secs: u64 = std::env::var("LOADMASTER_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let probe = Sandbox::new(CAPACITY, "");
    let markers = probe.path("markers");
    let violations = probe.path("violations");
    std::fs::create_dir_all(&markers).unwrap();
    // Every task, right after it starts, sums what tasks whose process is
    // alive use (cleaning up markers of dead ones) and records any excess:
    // CPU, each resource, and `db` in its own checkout. A task's marker
    // outlives a SIGKILL, so liveness decides.
    let tasks: String = TASKS
        .iter()
        .map(|(name, weight, db, mem, work)| {
            let script = format!(
                "ck=${{PWD##*/}}; m={markers}/$$.{weight}.{db}.{mem}.$ck; : > \"$m\"; \
                 cpu=0; db=0; mem=0; dbck=0; \
                 for f in {markers}/*; do [ -e \"$f\" ] || continue; \
                   set -- $(echo \"${{f##*/}}\" | tr . ' '); \
                   if kill -0 \"$1\" 2>/dev/null; then \
                     cpu=$((cpu + $2)); db=$((db + $3)); mem=$((mem + $4)); \
                     if [ \"$5\" = \"$ck\" ]; then dbck=$((dbck + $3)); fi; \
                   else rm -f \"$f\"; fi; \
                 done; \
                 if [ $cpu -gt {CAPACITY} ] || [ $db -gt 2 ] || [ $mem -gt 6 ] || [ $dbck -gt 1 ]; \
                 then echo \"cpu=$cpu db=$db mem=$mem db-in-$ck=$dbck\" >> {violations}; fi; \
                 {work}; rm -f \"$m\"",
                markers = q(&markers),
                violations = q(&violations),
            );
            let uses: Vec<String> = [("db", db), ("mem", mem)]
                .iter()
                .filter(|(_, amount)| **amount > 0)
                .map(|(resource, amount)| format!("{resource} = {amount}"))
                .collect();
            sh_task(name, *weight, &script).replacen(
                "cmd = ",
                &format!("uses = {{ {} }}\ncmd = ", uses.join(", ")),
                1,
            )
        })
        .collect();
    for checkout in CHECKOUTS {
        let dir = probe.root.path().join(checkout);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".loadmaster.toml"), RESOURCES.to_owned() + &tasks).unwrap();
    }
    let sandbox = Arc::new(probe);
    let stats = Arc::new(Stats::default());
    let until = Instant::now() + Duration::from_secs(secs);

    let agents: Vec<_> = (0..AGENTS)
        .map(|i| {
            let (sandbox, stats) = (Arc::clone(&sandbox), Arc::clone(&stats));
            thread::spawn(move || agent(&sandbox, &stats, i as u64 + 1, until))
        })
        .collect();
    let canceler = {
        let (sandbox, stats) = (Arc::clone(&sandbox), Arc::clone(&stats));
        thread::spawn(move || canceler(&sandbox, &stats, until))
    };
    for agent in agents {
        agent.join().unwrap();
    }
    canceler.join().unwrap();

    // Orphaned tasks (their loadmaster was killed) finish on their own; then
    // nothing may be left holding capacity.
    wait_until("the queue to drain", || sandbox.snapshot().jobs.is_empty());
    let leftover: Vec<_> = std::fs::read_dir(&markers)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|name| {
            let pid: u32 = name.split('.').next().unwrap().parse().unwrap();
            loadmaster::process::ProcessId::of(pid).unwrap().is_some()
        })
        .collect();
    let excess = std::fs::read_to_string(&violations).unwrap_or_default();
    let history = std::fs::read_to_string(sandbox.state.join("history.jsonl")).unwrap();

    let ok_by_task: Vec<String> = TASKS
        .iter()
        .zip(&stats.ok_by_task)
        .map(|((name, ..), ok)| format!("{name} {}", ok.load(Ordering::SeqCst)))
        .collect();
    println!(
        "soak: {secs}s, {} runs: {} ok ({}), {} gave up (resumed), {} timed out, {} crashed, \
         {} canceled; {} history lines",
        stats.runs.load(Ordering::SeqCst),
        stats.ok.load(Ordering::SeqCst),
        ok_by_task.join(", "),
        stats.gave_up.load(Ordering::SeqCst),
        stats.timed_out.load(Ordering::SeqCst),
        stats.crashed.load(Ordering::SeqCst),
        stats.canceled.load(Ordering::SeqCst),
        history.lines().count(),
    );
    assert!(excess.is_empty(), "capacity exceeded: {excess}");
    assert!(
        leftover.is_empty(),
        "task processes left running: {leftover:?}"
    );
    // No task is starved: each, the whole-machine one included, gets through.
    for ((name, ..), ok) in TASKS.iter().zip(&stats.ok_by_task) {
        assert!(ok.load(Ordering::SeqCst) > 0, "`{name}` never succeeded");
    }
    // Every run that got in line is logged once, by itself or, if it was
    // killed, by whoever reaped its job. Only a crash can come before getting
    // in line. `cancel` adds records of its own.
    let lines = history
        .lines()
        .filter(|line| !line.contains(r#""outcome":"canceled""#))
        .count();
    let runs = stats.runs.load(Ordering::SeqCst);
    let crashed = stats.crashed.load(Ordering::SeqCst);
    assert!(
        lines <= runs && lines + crashed >= runs,
        "{lines} history lines for {runs} runs ({crashed} crashed)"
    );
}

/// One simulated agent: picks tasks and runs them like Claude Code's Bash
/// tool, re-running on exit code 75 to resume its place in line.
fn agent(sandbox: &Sandbox, stats: &Stats, seed: u64, until: Instant) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    while Instant::now() < until {
        let index = rng.below(TASKS.len() as u64) as usize;
        let (task, ..) = TASKS[index];
        let checkout = sandbox
            .root
            .path()
            .join(CHECKOUTS[rng.below(CHECKOUTS.len() as u64) as usize]);
        let timeout = Duration::from_millis(1500 + rng.below(6500));
        let crash_after = (rng.below(10) == 0).then(|| Duration::from_millis(rng.below(2000)));
        loop {
            stats.runs.fetch_add(1, Ordering::SeqCst);
            let mut child = sandbox
                .cmd(&["run", "--max-wait", "2", task])
                .current_dir(&checkout)
                // Like Claude Code: each command in its own process group.
                .process_group(0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let status = supervise(&mut child, timeout, crash_after, stats);
            if status.code() == Some(75) {
                stats.gave_up.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            if status.success() {
                stats.ok.fetch_add(1, Ordering::SeqCst);
                stats.ok_by_task[index].fetch_add(1, Ordering::SeqCst);
            }
            break;
        }
    }
}

/// Cancels jobs now and then, as agents stopping their own checks do: those
/// of a checkout, or one job, sometimes with SIGKILL. Every cancel must
/// return once its jobs are gone.
fn canceler(sandbox: &Sandbox, stats: &Stats, until: Instant) {
    let mut rng = Rng(0xC0FF_EE00_DEAD_BEEF);
    while Instant::now() < until {
        thread::sleep(Duration::from_millis(500 + rng.below(2000)));
        let checkout = sandbox
            .root
            .path()
            .join(CHECKOUTS[rng.below(CHECKOUTS.len() as u64) as usize]);
        let jobs = sandbox.snapshot().jobs;
        let mut args: Vec<String> = match rng.below(3) {
            0 => vec!["--here".into()],
            _ if jobs.is_empty() => continue,
            _ => vec![jobs[rng.below(jobs.len() as u64) as usize].id.to_string()],
        };
        if rng.below(4) == 0 {
            args.insert(0, "--kill".into());
        }
        let mut cmd = sandbox.cmd(&["cancel"]);
        cmd.args(&args).current_dir(&checkout);
        let output = output(cmd);
        assert!(
            output.status.success(),
            "cancel {args:?}: {}",
            stderr(&output)
        );
        let canceled = stdout(&output).matches("canceled #").count();
        stats.canceled.fetch_add(canceled, Ordering::SeqCst);
    }
}

/// Waits for `child`. After `crash_after`, SIGKILLs loadmaster alone (a crash);
/// after `timeout`, SIGTERMs its process group and SIGKILLs it after a grace
/// period, as Claude Code does on a Bash timeout.
fn supervise(
    child: &mut Child,
    timeout: Duration,
    crash_after: Option<Duration>,
    stats: &Stats,
) -> ExitStatus {
    let started = Instant::now();
    let pgid = child.id() as libc::pid_t;
    let mut crashed = false;
    let mut terminated_at = None;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        let elapsed = started.elapsed();
        if !crashed && crash_after.is_some_and(|after| elapsed >= after) {
            crashed = true;
            stats.crashed.fetch_add(1, Ordering::SeqCst);
            signal(child.id(), libc::SIGKILL);
        }
        match terminated_at {
            None if elapsed >= timeout => {
                stats.timed_out.fetch_add(1, Ordering::SeqCst);
                // SAFETY: killpg has no memory-safety preconditions.
                unsafe { libc::killpg(pgid, libc::SIGTERM) };
                terminated_at = Some(Instant::now());
            }
            Some(at) if at.elapsed() >= KILL_GRACE => {
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
                let status = child.wait().unwrap();
                assert!(status.signal().is_some() || status.code().is_some());
                return status;
            }
            _ => {}
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// xorshift64*: deterministic per agent, no dependency needed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }
}
