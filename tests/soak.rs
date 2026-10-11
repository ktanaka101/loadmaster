//! Soak test: several simulated agents drive loadmaster the way Claude Code's
//! Bash tool does, for a while, with timeouts, kills and crashes.
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
const AGENTS: usize = 5;
/// Delay between SIGTERM and SIGKILL (a few seconds in Claude Code; shorter here).
const KILL_GRACE: Duration = Duration::from_secs(1);

/// (name, weight, work). `slow` takes a second to shut down after SIGTERM.
const TASKS: [(&str, u32, &str); 4] = [
    ("lint", 1, "sleep 0.3"),
    ("test", 4, "sleep 1"),
    ("e2e", 8, "sleep 2"),
    ("slow", 2, "trap 'sleep 1; exit 143' TERM; sleep 3"),
];

#[derive(Default)]
struct Stats {
    runs: AtomicUsize,
    ok: AtomicUsize,
    gave_up: AtomicUsize,
    timed_out: AtomicUsize,
    crashed: AtomicUsize,
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
    // Every task, right after it starts, sums the weights of tasks whose
    // process is alive (cleaning up markers of dead ones) and records any
    // excess. A task's marker outlives a SIGKILL, so liveness decides.
    let tasks: String = TASKS
        .iter()
        .map(|(name, weight, work)| {
            let script = format!(
                "m={markers}/$$.{weight}; : > \"$m\"; total=0; \
                 for f in {markers}/*; do [ -e \"$f\" ] || continue; \
                   p=${{f##*/}}; p=${{p%.*}}; \
                   if kill -0 \"$p\" 2>/dev/null; then total=$((total + ${{f##*.}})); \
                   else rm -f \"$f\"; fi; \
                 done; \
                 if [ \"$total\" -gt {CAPACITY} ]; then echo \"$total\" >> {violations}; fi; \
                 {work}; rm -f \"$m\"",
                markers = q(&markers),
                violations = q(&violations),
            );
            sh_task(name, *weight, &script)
        })
        .collect();
    std::fs::write(probe.repo.join(".loadmaster.toml"), tasks).unwrap();
    let sandbox = Arc::new(probe);
    let stats = Arc::new(Stats::default());
    let until = Instant::now() + Duration::from_secs(secs);

    let agents: Vec<_> = (0..AGENTS)
        .map(|i| {
            let (sandbox, stats) = (Arc::clone(&sandbox), Arc::clone(&stats));
            thread::spawn(move || agent(&sandbox, &stats, i as u64 + 1, until))
        })
        .collect();
    for agent in agents {
        agent.join().unwrap();
    }

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

    println!(
        "soak: {secs}s, {} runs: {} ok, {} gave up (resumed), {} timed out, {} crashed; \
         {} history lines",
        stats.runs.load(Ordering::SeqCst),
        stats.ok.load(Ordering::SeqCst),
        stats.gave_up.load(Ordering::SeqCst),
        stats.timed_out.load(Ordering::SeqCst),
        stats.crashed.load(Ordering::SeqCst),
        history.lines().count(),
    );
    assert!(excess.is_empty(), "capacity exceeded: {excess}");
    assert!(
        leftover.is_empty(),
        "task processes left running: {leftover:?}"
    );
    assert!(stats.ok.load(Ordering::SeqCst) > 0);
    // Every run that got in line is logged once, by itself or, if it was
    // killed, by whoever reaped its job. Only a crash can come before getting
    // in line.
    let (lines, runs) = (history.lines().count(), stats.runs.load(Ordering::SeqCst));
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
        let (task, ..) = TASKS[rng.below(TASKS.len() as u64) as usize];
        let timeout = Duration::from_millis(1500 + rng.below(6500));
        let crash_after = (rng.below(10) == 0).then(|| Duration::from_millis(rng.below(2000)));
        loop {
            stats.runs.fetch_add(1, Ordering::SeqCst);
            let mut child = sandbox
                .cmd(&["run", "--max-wait", "2", task])
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
            }
            break;
        }
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
