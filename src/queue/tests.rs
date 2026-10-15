use super::*;
use std::collections::BTreeMap;

struct Fixture {
    _dir: tempfile::TempDir,
    queue: Queue,
    me: ProcessId,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let queue = Queue::open(&dir.path().join(QUEUE_DB_FILE)).unwrap();
    Fixture {
        _dir: dir,
        queue,
        me: ProcessId::current().unwrap(),
    }
}

fn request<'a>(task: &'a str, weight: u32, args: &'a [String]) -> JobRequest<'a> {
    JobRequest {
        task,
        weight,
        dir: Path::new("/repo"),
        args,
        command: task,
        capacity: 8,
        measure_only: false,
        uses: &[],
        checkout: Path::new("/repo"),
        adapt_cpu: false,
        stop_signal: libc::SIGTERM,
    }
}

/// What the rule says about a job, as `status` words it.
fn said(admission: Admission) -> String {
    match admission {
        Admission::Admitted => "admitted".to_owned(),
        Admission::Waiting(wait) => {
            format!("{} ahead; waits for {}", wait.ahead, wait.waits_for())
        }
    }
}

fn history(f: &Fixture) -> Vec<serde_json::Value> {
    std::fs::read_to_string(f._dir.path().join(history::HISTORY_FILE))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// An owner that is guaranteed dead: our PID with the wrong start time.
fn dead_owner(me: ProcessId) -> ProcessId {
    ProcessId {
        start_time: me.start_time + 1,
        ..me
    }
}

#[test]
fn admits_jobs_up_to_capacity() {
    let f = fixture();
    let a = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    let b = f.queue.enqueue(&request("lint", 4, &[]), f.me).unwrap();
    let c = f.queue.enqueue(&request("lint", 1, &[]), f.me).unwrap();
    assert_eq!(a.try_admit().unwrap(), Admission::Admitted);
    assert_eq!(b.try_admit().unwrap(), Admission::Admitted);
    assert_eq!(
        said(c.try_admit().unwrap()),
        "0 ahead; waits for 1 core (8 of 8 in use)"
    );
    drop(a);
    assert_eq!(c.try_admit().unwrap(), Admission::Admitted);
    assert_eq!(f.queue.snapshot().unwrap().used(), 5);
}

#[test]
fn small_jobs_do_not_overtake_a_head_they_would_delay() {
    let f = fixture();
    let running = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    assert_eq!(running.try_admit().unwrap(), Admission::Admitted);
    let big = f.queue.enqueue(&request("e2e", 8, &[]), f.me).unwrap();
    let small = f.queue.enqueue(&request("lint", 1, &[]), f.me).unwrap();
    // The small job would fit now, but the big one ahead of it needs the
    // whole machine when it starts.
    assert_eq!(
        said(small.try_admit().unwrap()),
        "1 ahead; waits for 1 core (8 of 8 could be in use when #2 `e2e` starts)"
    );
    assert_eq!(
        said(big.try_admit().unwrap()),
        "0 ahead; waits for all 8 cores (4 in use)"
    );
    drop(running);
    assert_eq!(big.try_admit().unwrap(), Admission::Admitted);
    assert_eq!(
        said(small.try_admit().unwrap()),
        "0 ahead; waits for 1 core (8 of 8 in use)"
    );
}

#[test]
fn dropping_a_waiting_ticket_leaves_the_line() {
    let f = fixture();
    let running = f.queue.enqueue(&request("e2e", 8, &[]), f.me).unwrap();
    running.try_admit().unwrap();
    let head = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    let next = f.queue.enqueue(&request("lint", 1, &[]), f.me).unwrap();
    drop(head);
    drop(running);
    assert_eq!(next.try_admit().unwrap(), Admission::Admitted);
    assert_eq!(f.queue.snapshot().unwrap().jobs.len(), 1);
}

#[test]
fn reaps_jobs_whose_owner_died() {
    let f = fixture();
    let dead = f
        .queue
        .enqueue(&request("e2e", 8, &[]), dead_owner(f.me))
        .unwrap();
    let alive = f.queue.enqueue(&request("lint", 1, &[]), f.me).unwrap();
    // The dead head would block everyone; reaping removes it.
    assert_eq!(alive.try_admit().unwrap(), Admission::Admitted);
    std::mem::forget(dead);
    let snapshot = f.queue.snapshot().unwrap();
    assert_eq!(snapshot.jobs.len(), 1);
}

#[test]
fn running_job_survives_its_owner_while_child_group_lives() {
    let f = fixture();
    let ticket = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    ticket.try_admit().unwrap();
    let task = LiveGroup::start();
    f.queue.record_child(ticket.id(), task.group).unwrap();
    f.queue
        .conn
        .execute(
            "UPDATE jobs SET owner_start = owner_start + 1 WHERE id = ?1",
            [ticket.id()],
        )
        .unwrap();
    let snapshot = f.queue.snapshot().unwrap();
    assert!(snapshot.reaped.is_empty());
    assert!(snapshot.jobs[0].is_orphaned());
    std::mem::forget(ticket);
}

/// A real task process group, killed on drop.
struct LiveGroup {
    child: std::process::Child,
    group: ProcessGroup,
}

impl LiveGroup {
    fn start() -> LiveGroup {
        use std::os::unix::process::CommandExt;
        let child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = ProcessGroup {
            pgid: child.id(),
            leader_start_time: ProcessId::of(child.id()).unwrap().map(|p| p.start_time),
        };
        LiveGroup { child, group }
    }
}

impl Drop for LiveGroup {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn detached_slot_is_held_until_the_group_exits() {
    let f = fixture();
    let live = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    live.try_admit().unwrap();
    let task = LiveGroup::start();
    f.queue.record_child(live.id(), task.group).unwrap();
    live.hand_over(|| false).unwrap();

    let gone = f.queue.enqueue(&request("lint", 2, &[]), f.me).unwrap();
    gone.try_admit().unwrap();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    child.wait().unwrap();
    let dead_group = ProcessGroup {
        pgid: child.id(),
        leader_start_time: None,
    };
    f.queue.record_child(gone.id(), dead_group).unwrap();
    gone.hand_over(|| false).unwrap();

    let snapshot = f.queue.snapshot().unwrap();
    assert_eq!(snapshot.reaped.len(), 1);
    assert_eq!(snapshot.reaped[0].task, "lint");
    assert_eq!(snapshot.used(), 4);
    assert!(snapshot.jobs[0].is_orphaned());
}

#[test]
fn parked_job_keeps_its_place_and_can_be_resumed() {
    let f = fixture();
    let args = vec!["--shard=1".to_owned()];
    let small = |args| JobRequest {
        capacity: 4,
        ..request("test", 4, args)
    };
    let running = f.queue.enqueue(&request("e2e", 8, &[]), f.me).unwrap();
    running.try_admit().unwrap();
    let first = f.queue.enqueue(&small(&args), f.me).unwrap();
    let first_id = first.id();
    first.park().unwrap();
    let later = f.queue.enqueue(&small(&[]), f.me).unwrap();
    assert!(!later.resumed(), "different args must not resume");

    let resumed = f.queue.enqueue(&small(&args), f.me).unwrap();
    assert!(resumed.resumed());
    assert_eq!(resumed.id(), first_id);
    drop(running);
    assert_eq!(resumed.try_admit().unwrap(), Admission::Admitted);
    assert_eq!(
        said(later.try_admit().unwrap()),
        "0 ahead; waits for all 4 cores (4 in use)"
    );
}

#[test]
fn a_parked_job_is_resumed_only_with_the_same_demands() {
    // Jobs behind may have started next to the parked job's set-aside; a
    // re-run that needs more could then be delayed by them.
    let f = fixture();
    let parked = f.queue.enqueue(&request("e2e", 2, &[]), f.me).unwrap();
    let parked_id = parked.id();
    parked.park().unwrap();
    let bigger = f.queue.enqueue(&request("e2e", 4, &[]), f.me).unwrap();
    assert!(!bigger.resumed());
    assert_ne!(bigger.id(), parked_id);
    let uses = [db(1)];
    let with_db = f
        .queue
        .enqueue(
            &JobRequest {
                uses: &uses,
                ..request("e2e", 2, &[])
            },
            f.me,
        )
        .unwrap();
    assert!(!with_db.resumed());
    let same = f.queue.enqueue(&request("e2e", 2, &[]), f.me).unwrap();
    assert!(same.resumed());
    assert_eq!(same.id(), parked_id);
}

#[test]
fn parked_head_blocks_until_it_expires() {
    let f = fixture();
    let parked = f.queue.enqueue(&request("e2e", 8, &[]), f.me).unwrap();
    let parked_id = parked.id();
    parked.park().unwrap();
    let next = f.queue.enqueue(&request("lint", 1, &[]), f.me).unwrap();
    assert_eq!(
        said(next.try_admit().unwrap()),
        "1 ahead; waits for 1 core (8 of 8 could be in use when #1 `e2e` starts)"
    );
    let expired = unix_ms(SystemTime::now() - PARK_TTL - Duration::from_secs(1));
    f.queue
        .conn
        .execute(
            "UPDATE jobs SET parked_at = ?2 WHERE id = ?1",
            params![parked_id, expired],
        )
        .unwrap();
    assert_eq!(next.try_admit().unwrap(), Admission::Admitted);
}

#[test]
fn live_holder_check() {
    let f = fixture();
    let ticket = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    assert_eq!(f.queue.holder(ticket.id()).unwrap(), None, "waiting");
    ticket.try_admit().unwrap();
    let holder = f.queue.holder(ticket.id()).unwrap().unwrap();
    assert_eq!(
        (holder.id, holder.dir.as_path()),
        (ticket.id(), Path::new("/repo"))
    );
    assert_eq!(f.queue.holder(ticket.id() + 100).unwrap(), None);
    let id = ticket.id();
    drop(ticket);
    assert_eq!(f.queue.holder(id).unwrap(), None);
}

#[test]
fn ids_are_never_reused() {
    let f = fixture();
    let first = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    let first_id = first.id();
    drop(first);
    let second = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    assert!(second.id() > first_id);
}

#[test]
fn resume_key_compares_directories_byte_for_byte() {
    use std::ffi::OsStr;
    let f = fixture();
    // Both are "/repo/\u{FFFD}" when converted lossily.
    let a = Path::new(OsStr::from_bytes(b"/repo/\xff"));
    let b = Path::new(OsStr::from_bytes(b"/repo/\xfe"));
    let parked = f
        .queue
        .enqueue(
            &JobRequest {
                dir: a,
                ..request("test", 4, &[])
            },
            f.me,
        )
        .unwrap();
    parked.park().unwrap();
    let other = f
        .queue
        .enqueue(
            &JobRequest {
                dir: b,
                ..request("test", 4, &[])
            },
            f.me,
        )
        .unwrap();
    assert!(!other.resumed());
    let same = f
        .queue
        .enqueue(
            &JobRequest {
                dir: a,
                ..request("test", 4, &[])
            },
            f.me,
        )
        .unwrap();
    assert!(same.resumed());
    assert_eq!(f.queue.snapshot().unwrap().jobs[0].dir, a);
}

#[test]
fn reaping_a_killed_owner_logs_the_job_as_lost() {
    let f = fixture();
    // Killed while waiting.
    let waiting = f
        .queue
        .enqueue(&request("lint", 1, &[]), dead_owner(f.me))
        .unwrap();
    std::mem::forget(waiting);
    // Killed while running; its task has exited too.
    let running = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    running.try_admit().unwrap();
    let gone = ProcessGroup {
        pgid: f.me.pid,
        leader_start_time: Some(f.me.start_time + 1),
    };
    f.queue.record_child(running.id(), gone).unwrap();
    f.queue
        .conn
        .execute(
            "UPDATE jobs SET owner_start = owner_start + 1, started_at = enqueued_at + 250
             WHERE id = ?1",
            [running.id()],
        )
        .unwrap();
    std::mem::forget(running);
    // Killed once admitted, but before its task was started: the task never
    // ran, since it is let through only after its group is recorded.
    // It waited, as checks that never started do, until it ended.
    let admitted = f.queue.enqueue(&request("build", 1, &[]), f.me).unwrap();
    admitted.try_admit().unwrap();
    f.queue
        .conn
        .execute(
            "UPDATE jobs SET owner_start = owner_start + 1, enqueued_at = enqueued_at - 5000,
                 started_at = enqueued_at - 4750
             WHERE id = ?1",
            [admitted.id()],
        )
        .unwrap();
    std::mem::forget(admitted);
    // Gave up and expired: already logged as gave_up by its owner.
    let parked = f.queue.enqueue(&request("e2e", 2, &[]), f.me).unwrap();
    let parked_id = parked.id();
    parked.park().unwrap();
    let expired = unix_ms(SystemTime::now() - PARK_TTL - Duration::from_secs(1));
    f.queue
        .conn
        .execute(
            "UPDATE jobs SET parked_at = ?2 WHERE id = ?1",
            params![parked_id, expired],
        )
        .unwrap();

    // Every write reaps, so the first two went when the later jobs got in line.
    f.queue.reap().unwrap();
    assert!(f.queue.snapshot().unwrap().jobs.is_empty());
    let records = history(&f);
    assert_eq!(records.len(), 3, "{records:?}");
    for ((record, task), started) in records
        .iter()
        .zip(["lint", "test", "build"])
        .zip([false, true, false])
    {
        assert_eq!(record["task"], task);
        assert_eq!(record["outcome"], "lost");
        assert_eq!(record["capacity"], 8);
        assert_eq!(record["exit_code"], serde_json::Value::Null);
        assert_eq!(record["started"], started, "{task}");
    }
    assert_eq!(records[1]["wait_ms"], 250);
    assert!(
        records[2]["wait_ms"].as_u64().unwrap() >= 5000,
        "{records:?}"
    );
    // Reaped once, logged once.
    f.queue.reap().unwrap();
    assert_eq!(history(&f).len(), 3);
}

#[test]
fn a_detached_job_is_not_logged_when_reaped() {
    let f = fixture();
    let ticket = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    ticket.try_admit().unwrap();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    child.wait().unwrap();
    let gone = ProcessGroup {
        pgid: child.id(),
        leader_start_time: None,
    };
    f.queue.record_child(ticket.id(), gone).unwrap();
    ticket.hand_over(|| false).unwrap();
    assert_eq!(f.queue.reap().unwrap().len(), 1);
    assert!(history(&f).is_empty());
}

#[test]
fn admit_now_ignores_the_line_and_the_capacity() {
    let f = fixture();
    let big = f.queue.enqueue(&request("e2e", 8, &[]), f.me).unwrap();
    big.try_admit().unwrap();
    let parked = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    parked.park().unwrap();
    let late = f.queue.enqueue(&request("lint", 4, &[]), f.me).unwrap();
    late.admit_now().unwrap();
    assert_eq!(f.queue.snapshot().unwrap().used(), 12);
}

#[test]
fn processes_opening_a_new_database_at_once_make_its_schema_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(QUEUE_DB_FILE);
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                barrier.wait();
                Queue::open(&path).unwrap();
            });
        }
    });
    let queue = Queue::open(&path).unwrap();
    let version: i64 = queue
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1);
    assert!(queue.snapshot().unwrap().jobs.is_empty());
}

/// Whether a stop has been asked for job `id` that nobody has done yet.
fn cancel_pending(queue: &Queue, id: JobId) -> bool {
    queue
        .conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM cancel_requests
                            WHERE job = ?1 AND kill > max(owner_done, signal_done))",
            [id],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn a_cancel_request_is_taken_once_and_goes_with_its_job() {
    let f = fixture();
    let ticket = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    assert_eq!(ticket.cancel_requested().unwrap(), None);
    assert!(matches!(
        f.queue.stop(ticket.id(), false).unwrap(),
        Canceled::Requested(_)
    ));
    assert!(cancel_pending(&f.queue, ticket.id()));
    assert_eq!(ticket.cancel_requested().unwrap(), Some(false));
    assert_eq!(ticket.cancel_requested().unwrap(), None, "taken once");

    // Asking again for no more than was done does nothing: no second SIGTERM.
    f.queue.stop(ticket.id(), false).unwrap();
    assert!(!cancel_pending(&f.queue, ticket.id()));
    assert_eq!(ticket.cancel_requested().unwrap(), None);
    // Asking for SIGKILL is new; nothing goes above it.
    f.queue.stop(ticket.id(), true).unwrap();
    assert_eq!(ticket.cancel_requested().unwrap(), Some(true));
    f.queue.stop(ticket.id(), false).unwrap();
    assert_eq!(ticket.cancel_requested().unwrap(), None);

    let id = ticket.id();
    drop(ticket);
    assert!(!cancel_pending(&f.queue, id), "gone with its job");
}

#[test]
fn a_sigterm_recorded_at_handover_is_not_sent_again() {
    // The task left processes behind after its loadmaster passed a SIGTERM
    // on; a cancel, which signals them itself, must not send another.
    let f = fixture();
    let ticket = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    assert_eq!(ticket.try_admit().unwrap(), Admission::Admitted);
    let left_behind = LiveGroup::start();
    f.queue
        .record_child(ticket.id(), left_behind.group)
        .unwrap();
    let id = ticket.id();
    ticket.hand_over(|| true).unwrap();
    assert!(matches!(
        f.queue.stop(id, false).unwrap(),
        Canceled::Requested(_)
    ));
    assert!(!cancel_pending(&f.queue, id));
    assert!(left_behind.group.is_alive(), "not signalled");
}

#[test]
fn records_uses_and_checkout() {
    let f = fixture();
    let uses = [db(1)];
    let request = JobRequest {
        uses: &uses,
        checkout: Path::new("/repo-a"),
        ..request("e2e", 2, &[])
    };
    let ticket = f.queue.enqueue(&request, f.me).unwrap();
    let job = f.queue.snapshot().unwrap().jobs.remove(0);
    assert_eq!(job.id, ticket.id());
    assert_eq!(job.uses, uses);
    assert_eq!(job.checkout, Path::new("/repo-a"));
}

// ---- The admission rule with resources, on jobs built in memory. ----

/// `db`: at most 2 machine-wide and 1 per checkout. `mem`: at most 6.
fn db(amount: u32) -> Use {
    Use {
        name: "db".into(),
        amount,
        capacity: Some(2),
        per_checkout: Some(1),
    }
}

fn capped_db(amount: u32) -> Use {
    Use {
        per_checkout: None,
        ..db(amount)
    }
}

/// `db` as declared by a repository that gives it no capacity.
fn uncapped_db(amount: u32) -> Use {
    Use {
        capacity: None,
        ..db(amount)
    }
}

fn mem(amount: u32) -> Use {
    Use {
        name: "mem".into(),
        amount,
        capacity: Some(6),
        per_checkout: None,
    }
}

fn job(id: JobId, state: JobState, weight: u32, uses: Vec<Use>, checkout: &str) -> Job {
    Job {
        id,
        task: format!("job{id}"),
        weight,
        state,
        dir: PathBuf::from(checkout),
        args: vec![],
        command: String::new(),
        owner: None,
        child: None,
        enqueued_at: UNIX_EPOCH,
        parked_at: None,
        started_at: None,
        capacity: 8,
        measure_only: false,
        uses,
        checkout: PathBuf::from(checkout),
        adapt_cpu: true,
        stop_signal: libc::SIGTERM,
        granted: (state == JobState::Running).then_some(weight),
    }
}

use JobState::{Running, Waiting};

fn admitted(jobs: &[Job], id: JobId) -> bool {
    decide(jobs, id, None).unwrap() == Admission::Admitted
}

fn browser(amount: u32) -> Use {
    res("browser", amount, 1)
}

fn res(name: &str, amount: u32, capacity: u32) -> Use {
    Use {
        name: name.into(),
        amount,
        capacity: Some(capacity),
        per_checkout: None,
    }
}

/// `job` with a CPU capacity of its own, adapting to CPU outside or not.
fn sized(job: Job, capacity: u32, adapt_cpu: bool) -> Job {
    Job {
        capacity,
        adapt_cpu,
        ..job
    }
}

/// Jobs with tasks named.
fn named(mut jobs: Vec<Job>, names: &[&str]) -> Vec<Job> {
    for (job, name) in jobs.iter_mut().zip(names) {
        job.task = (*name).to_owned();
    }
    jobs
}

#[test]
fn a_job_short_of_a_resource_waits_for_it_alone() {
    let jobs = [
        job(1, Running, 2, vec![db(1)], "/a"),
        job(2, Running, 2, vec![db(1)], "/b"),
        job(3, Waiting, 2, vec![db(1)], "/c"),
        job(4, Waiting, 1, vec![], "/a"),
        job(5, Waiting, 2, vec![db(1)], "/d"),
    ];
    assert_eq!(
        said(decide(&jobs, 3, None).unwrap()),
        "0 ahead; waits for `db` (2 of 2 in use by #1 `job1` and #2 `job2`)"
    );
    assert!(
        admitted(&jobs, 4),
        "a lint needs no db, and fits beside job 3's start"
    );
    assert!(!admitted(&jobs, 5), "both dbs are in use");
}

#[test]
fn a_check_starts_beside_a_run_waiting_for_what_a_long_run_holds() {
    // A long UI capture holds the one browser; a unit run waits for it. The
    // unit run can only start once the capture ends, so a lint fits beside
    // it then, and beside the capture now (clew, 0.9.0).
    let jobs = named(
        vec![
            job(1, Running, 5, vec![browser(1)], "/a"),
            job(2, Waiting, 2, vec![browser(1)], "/b"),
            job(3, Waiting, 3, vec![], "/c"),
        ],
        &["ui", "unit", "lint"],
    );
    assert_eq!(
        said(decide(&jobs, 2, None).unwrap()),
        "0 ahead; waits for `browser` (1 of 1 in use by #1 `ui`)"
    );
    assert!(admitted(&jobs, 3));
}

#[test]
fn runs_waiting_in_turn_for_one_resource_keep_no_cores_from_others() {
    // A capture (8 of 12 cores) holds the browser; three 4-core units wait
    // for it in turn, and a 4-core lint behind them starts (clew, 0.11).
    let b = || vec![browser(1)];
    let mut jobs: Vec<Job> = vec![sized(job(1, Running, 8, b(), "/a"), 12, true)];
    for id in 2..=4 {
        jobs.push(sized(job(id, Waiting, 4, b(), "/b"), 12, true));
    }
    jobs.push(sized(job(5, Waiting, 4, vec![], "/c"), 12, true));
    assert!(admitted(&jobs, 5));
    // A second capture waiting behind the first holds nothing back either.
    let captures = [
        sized(job(1, Running, 8, b(), "/a"), 12, true),
        sized(job(2, Waiting, 8, b(), "/b"), 12, true),
        sized(job(3, Waiting, 4, vec![], "/c"), 12, true),
    ];
    assert!(admitted(&captures, 3));
}

#[test]
fn a_small_job_starts_ahead_of_a_big_one_only_where_it_cannot_delay_it() {
    // Job 2 needs one of the running jobs to end; beside it then, one of
    // them may still run: 3 + 4 + 2 > 8, so job 3 (2 cores) waits ...
    let jobs = [
        job(1, Running, 3, vec![], "/a"),
        job(2, Running, 3, vec![], "/a"),
        job(3, Waiting, 4, vec![], "/a"),
        job(4, Waiting, 2, vec![], "/a"),
    ];
    assert_eq!(
        said(decide(&jobs, 4, None).unwrap()),
        "1 ahead; waits for 2 cores (7 of 8 could be in use when #3 `job3` starts, beside \
         #1 `job1`)"
    );
    // ... but 1 core fits.
    let jobs = [
        jobs[0].clone(),
        jobs[1].clone(),
        jobs[2].clone(),
        job(4, Waiting, 1, vec![], "/a"),
    ];
    assert!(admitted(&jobs, 4));
    // A 6-core job running excludes job 3 until it ends: 4 + 1 fits.
    let jobs = [
        job(1, Running, 6, vec![], "/a"),
        job(2, Waiting, 4, vec![], "/a"),
        job(3, Waiting, 1, vec![], "/b"),
    ];
    assert!(admitted(&jobs, 3));
}

#[test]
fn a_job_waits_where_a_start_ahead_could_run_beside_a_running_one() {
    // B waits for the browser C holds; when C ends, B starts beside A.
    let jobs = [
        sized(job(1, Running, 2, vec![browser(1)], "/a"), 12, false),
        sized(job(2, Running, 5, vec![], "/a"), 12, false),
        sized(job(3, Waiting, 6, vec![browser(1)], "/a"), 12, false),
        sized(job(4, Waiting, 5, vec![], "/a"), 12, false),
    ];
    assert_eq!(
        said(decide(&jobs, 4, None).unwrap()),
        "1 ahead; waits for 5 cores (11 of 12 could be in use when #3 `job3` starts, beside \
         #2 `job2`)"
    );
}

#[test]
fn a_job_that_may_start_now_may_start_later_with_more() {
    // Job 1 may start now with 4 of its 8 cores (CPU outside), but if it does
    // not, it may start later with 8: job 2 waits.
    let outside = Some(Outside {
        busy: 4.0,
        cores: 8,
    });
    let jobs = [
        sized(job(1, Waiting, 8, vec![], "/a"), 8, true),
        sized(job(2, Waiting, 4, vec![], "/a"), 8, false),
    ];
    assert_eq!(decide(&jobs, 1, outside).unwrap(), Admission::Admitted);
    assert_ne!(decide(&jobs, 2, outside).unwrap(), Admission::Admitted);
}

#[test]
fn a_job_that_may_start_now_counts_as_running_for_the_jobs_behind() {
    // Under a lowered limit of 1 core, job 1 starts first whatever order the
    // two poll in.
    let outside = Some(Outside {
        busy: 1.0,
        cores: 2,
    });
    let jobs = [
        sized(job(1, Waiting, 1, vec![], "/a"), 2, true),
        sized(job(2, Waiting, 1, vec![], "/a"), 2, true),
    ];
    assert_eq!(decide(&jobs, 1, outside).unwrap(), Admission::Admitted);
    assert_ne!(decide(&jobs, 2, outside).unwrap(), Admission::Admitted);
}

#[test]
fn a_job_that_adapts_may_start_beside_a_start_with_fewer_cores() {
    // When job 1 ends, job 3 may start with 2 cores before job 2 starts,
    // which does not see it: job 4 must leave room for both.
    let jobs = [
        sized(job(1, Running, 1, vec![browser(1)], "/a"), 5, false),
        sized(job(2, Waiting, 3, vec![browser(1)], "/a"), 5, false),
        sized(job(3, Waiting, 3, vec![], "/a"), 5, true),
        sized(job(4, Waiting, 1, vec![], "/a"), 5, false),
    ];
    assert!(!admitted(&jobs, 4));
}

#[test]
fn beside_a_job_that_starts_with_fewer_cores_runs_nothing_ahead_of_it() {
    // Job 3 adapts: with fewer cores it starts only with no core in use
    // ahead of it, so jobs 1 and 2 are not beside it then.
    let jobs = [
        sized(job(1, Running, 5, vec![], "/a"), 12, true),
        sized(job(2, Running, 5, vec![], "/a"), 12, true),
        sized(job(3, Waiting, 4, vec![], "/a"), 12, true),
        sized(job(4, Waiting, 2, vec![], "/a"), 12, true),
    ];
    assert!(admitted(&jobs, 4));
}

#[test]
fn limits_that_disagree_bind_by_the_smallest() {
    // Jobs 1 and 2 record r = 10 and may run together; job 3 records 7.
    let r = |capacity| res("r", 4, capacity);
    let jobs = named(
        vec![
            sized(job(1, Waiting, 1, vec![r(10)], "/a"), 12, false),
            sized(job(2, Waiting, 1, vec![r(10)], "/a"), 12, false),
            sized(job(3, Waiting, 1, vec![res("r", 3, 7)], "/a"), 12, false),
        ],
        &["a", "z", "k"],
    );
    assert_eq!(
        said(decide(&jobs, 3, None).unwrap()),
        "2 ahead; waits for 3 of `r` (8 of 7 in use by #1 `a` and #2 `z`)"
    );
    // A job ahead with a smaller capacity binds only its own start: 8 + 4
    // now, and 4 + 4 of its 8 when it starts beside job 3.
    let jobs = [
        sized(job(1, Running, 8, vec![browser(1)], "/a"), 12, false),
        sized(job(2, Waiting, 4, vec![browser(1)], "/a"), 8, false),
        sized(job(3, Waiting, 4, vec![], "/a"), 12, false),
    ];
    assert!(admitted(&jobs, 3));
    let jobs = [
        sized(job(1, Running, 7, vec![browser(1)], "/a"), 12, false),
        sized(job(2, Waiting, 4, vec![browser(1)], "/a"), 8, false),
        sized(job(3, Waiting, 5, vec![], "/a"), 12, false),
    ];
    assert_eq!(
        said(decide(&jobs, 3, None).unwrap()),
        "1 ahead; waits for 5 cores (4 of 8 could be in use when #2 `job2` starts; the limit \
         is the one #2 `job2` got in line with)"
    );
}

#[test]
fn a_use_without_a_limit_of_its_own_still_counts() {
    // Without a per-checkout limit, job 2 still uses the checkout's db.
    let jobs = [
        job(1, Running, 1, vec![db(1)], "/a"),
        job(2, Waiting, 1, vec![capped_db(1)], "/a"),
    ];
    assert_eq!(
        said(decide(&jobs, 2, None).unwrap()),
        "0 ahead; waits for `db` in its checkout (1 of 1 in use by #1 `job1`; the limit is \
         the one #1 `job1` got in line with)"
    );
    // Without a machine-wide one, job 2 still uses a gpu, which job 3 waits
    // for when job 2 starts.
    let gpu = |capacity| Use {
        name: "gpu".into(),
        amount: 1,
        capacity,
        per_checkout: None,
    };
    let jobs = [
        job(1, Running, 1, vec![db(1)], "/a"),
        job(2, Waiting, 1, vec![db(1), gpu(None)], "/a"),
        job(3, Waiting, 1, vec![gpu(Some(1))], "/b"),
    ];
    assert_eq!(
        said(decide(&jobs, 3, None).unwrap()),
        "1 ahead; waits for `gpu` (1 of 1 could be in use when #2 `job2` starts)"
    );
}

#[test]
fn measure_only_jobs_count_where_they_run_and_may_end() {
    // Job 1 started without the rule: it counts now, but beside job 3's start
    // it may have ended, and then job 2 runs beside job 3.
    let mut measured = sized(job(1, Running, 1, vec![res("r", 1, 2)], "/a"), 12, false);
    measured.measure_only = true;
    let jobs = [
        measured,
        sized(job(2, Running, 5, vec![res("r", 1, 2)], "/a"), 12, false),
        sized(job(3, Waiting, 5, vec![res("r", 1, 2)], "/a"), 12, false),
        sized(job(4, Waiting, 6, vec![], "/a"), 12, false),
    ];
    assert!(!admitted(&jobs, 4));
    // One behind a job counts for it too.
    let mut measured = sized(job(2, Running, 4, vec![browser(1)], "/a"), 8, false);
    measured.measure_only = true;
    let jobs = [
        sized(job(1, Waiting, 4, vec![browser(1)], "/a"), 8, false),
        measured,
        sized(job(3, Waiting, 4, vec![], "/a"), 8, false),
    ];
    assert!(!admitted(&jobs, 1));
    assert!(admitted(&jobs, 3));
}

#[test]
fn a_parked_job_keeps_its_protection() {
    let jobs = [
        sized(job(1, Running, 4, vec![], "/a"), 4, false),
        sized(job(2, JobState::Parked, 4, vec![], "/a"), 4, false),
        sized(job(3, Waiting, 4, vec![], "/a"), 4, false),
    ];
    assert!(!admitted(&jobs, 3));
}

#[test]
fn large_capacities_cost_no_more_than_small_ones() {
    let jobs = [
        sized(job(1, Running, 1, vec![], "/a"), 1_000_000, true),
        sized(job(2, Waiting, 1_000_000, vec![], "/a"), 1_000_000, true),
        sized(job(3, Waiting, 1, vec![], "/a"), 1_000_000, true),
    ];
    let started = std::time::Instant::now();
    assert!(!admitted(&jobs, 3));
    assert!(started.elapsed() < std::time::Duration::from_millis(100));
}

#[test]
fn per_checkout_limits_count_only_their_checkout() {
    let other_checkout = [
        job(1, Running, 2, vec![db(1)], "/a"),
        job(2, Waiting, 2, vec![db(1)], "/b"),
    ];
    assert!(admitted(&other_checkout, 2));
    let same_checkout = [
        job(1, Running, 2, vec![db(1)], "/a"),
        job(2, Waiting, 2, vec![db(1)], "/a"),
        job(3, Waiting, 2, vec![db(1)], "/b"),
    ];
    assert_eq!(
        said(decide(&same_checkout, 2, None).unwrap()),
        "0 ahead; waits for `db` in its checkout (1 of 1 in use by #1 `job1`)"
    );
    // Job 2 starts only once job 1 ends, which frees the db it uses: job 3,
    // of another checkout, takes the one free meanwhile.
    assert!(admitted(&same_checkout, 3));
}

#[test]
fn an_amount_above_a_limit_takes_all_of_it() {
    let jobs = [
        job(1, Waiting, 1, vec![mem(10)], "/a"),
        job(2, Waiting, 1, vec![mem(1)], "/b"),
    ];
    assert!(admitted(&jobs, 1), "10 counts as the whole 6");
    assert!(!admitted(&jobs, 2));
}

#[test]
fn a_job_without_a_capacity_still_counts_toward_others_limits() {
    // Repositories may disagree: one declares `db` with capacity 2, another
    // without one. The second one's runs still use a db.
    let jobs = [
        job(1, Running, 1, vec![uncapped_db(1)], "/a"),
        job(2, Running, 1, vec![capped_db(1)], "/b"),
        job(3, Waiting, 1, vec![capped_db(1)], "/c"),
        job(4, Waiting, 1, vec![uncapped_db(1)], "/d"),
    ];
    assert!(!admitted(&jobs, 3), "2 of 2 in use");
    assert!(!admitted(&jobs, 4), "job 2's limit of 2 binds it too");
}

#[test]
fn unlimited_resources_do_not_hold_anyone_back() {
    let free = |amount| Use {
        name: "free".into(),
        amount,
        capacity: None,
        per_checkout: None,
    };
    let jobs = [
        job(1, Running, 1, vec![free(100)], "/a"),
        job(2, Waiting, 1, vec![free(100)], "/a"),
    ];
    assert!(admitted(&jobs, 2));
}

#[test]
fn a_job_behind_respects_the_smallest_capacity_of_a_start_ahead() {
    // Job 1 got in line with a CPU capacity of 4 and may start now; job 4
    // says 8, but beside job 1 and job 3 it would take the machine past 4.
    let mut small = job(1, Waiting, 3, vec![db(1)], "/a");
    small.capacity = 4;
    let jobs = [
        small,
        job(3, Running, 1, vec![db(1)], "/a"),
        job(4, Waiting, 3, vec![], "/b"),
    ];
    assert!(!admitted(&jobs, 4));
}

#[test]
fn a_job_that_cannot_run_beside_a_start_by_its_own_limit_is_not_beside_it() {
    // Job 1 recorded a capacity of 4, so job 2 (2 cores) can only start once
    // it ended: job 3 fits beside both (Codex).
    let jobs = [
        sized(job(1, Running, 3, vec![], "/a"), 4, false),
        sized(job(2, Waiting, 2, vec![], "/a"), 10, false),
        sized(job(3, Waiting, 1, vec![], "/a"), 10, false),
    ];
    assert!(admitted(&jobs, 3));
}

#[test]
fn the_most_beside_a_start_is_over_the_sets_that_fit_on_every_dimension() {
    // Beside job 3's start (it waits for the browser) jobs 2 and 4 could
    // use 3 cores, but not together: they need 7 of 6 `mem`. So at most 6
    // of the 8 cores are in use then, and job 5 (2 cores) fits.
    let jobs = [
        sized(job(1, Running, 1, vec![browser(1)], "/a"), 8, false),
        sized(job(2, Running, 2, vec![mem(6)], "/a"), 8, false),
        sized(job(3, Waiting, 4, vec![browser(1)], "/a"), 8, false),
        sized(job(4, Waiting, 1, vec![mem(1)], "/a"), 8, false),
        sized(job(5, Waiting, 2, vec![], "/a"), 8, false),
    ];
    assert!(admitted(&jobs, 5));
    // With 3 cores it would not fit beside job 3 and job 2.
    let mut bigger = jobs.clone();
    bigger[4].weight = 3;
    assert!(!admitted(&bigger, 5));
}

/// A workload for the simulation: jobs in line order.
#[derive(Clone)]
struct Arrival {
    at: u32,
    duration: u32,
    weight: u32,
    uses: Vec<Use>,
    checkout: &'static str,
    /// When it leaves the line, if it has not started by then.
    leaves: Option<u32>,
    adapt: bool,
    capacity: u32,
    /// When it parks (if it waits then), and when it resumes; if never, its
    /// place expires 5 steps later.
    parks: Option<(u32, Option<u32>)>,
}

/// The "start" of a job that left the line, for comparing runs.
const LEFT: u32 = u32::MAX;

/// CPU busy outside the queue at each time step, of a machine with 14 cores
/// (the jobs' capacity is 6 or 8).
type Load<'a> = &'a dyn Fn(u32) -> Option<Outside>;

thread_local! {
    /// A seed for polling in a random order instead.
    static POLL_SEED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Runs `arrivals` through the admission rule in discrete time steps, letting
/// every waiting job poll (in line order, the reverse, or a random order)
/// until nothing changes, and returns when each job started (`LEFT` if it
/// left the line). Checks every limit, and the explanations, after each step.
fn simulate(arrivals: &[Arrival], reverse: bool, outside: Load) -> Vec<u32> {
    simulate_with(arrivals, reverse, outside, &mut |_| {})
}

/// [`simulate`], showing `look` the line after each step.
fn simulate_with(
    arrivals: &[Arrival],
    reverse: bool,
    outside: Load,
    look: &mut dyn FnMut(&[Job]),
) -> Vec<u32> {
    let mut jobs: Vec<Job> = Vec::new();
    let mut started = vec![None; arrivals.len()];
    for t in 0..10_000 {
        // Finish, leave, park, resume, expire, then arrive.
        jobs.retain(|j| {
            let i = j.id as usize - 1;
            started[i].is_none_or(|s: u32| s == LEFT || s + arrivals[i].duration > t)
        });
        jobs.retain(|j| {
            let i = j.id as usize - 1;
            let leaving = j.state == Waiting && arrivals[i].leaves == Some(t);
            if leaving {
                started[i] = Some(LEFT);
            }
            !leaving
        });
        for j in jobs.iter_mut() {
            match arrivals[j.id as usize - 1].parks {
                Some((at, _)) if at == t && j.state == Waiting => j.state = JobState::Parked,
                Some((_, Some(back))) if back == t && j.state == JobState::Parked => {
                    j.state = Waiting;
                }
                _ => {}
            }
        }
        jobs.retain(|j| {
            let i = j.id as usize - 1;
            let expired = j.state == JobState::Parked
                && matches!(arrivals[i].parks, Some((at, None)) if at + 5 == t);
            if expired {
                started[i] = Some(LEFT);
            }
            !expired
        });
        for (i, a) in arrivals.iter().enumerate().filter(|(_, a)| a.at == t) {
            jobs.push(sized(
                job(
                    i as JobId + 1,
                    Waiting,
                    a.weight.min(a.capacity),
                    a.uses.clone(),
                    a.checkout,
                ),
                a.capacity,
                a.adapt,
            ));
        }
        loop {
            let mut ids: Vec<JobId> = jobs
                .iter()
                .filter(|j| j.state == Waiting)
                .map(|j| j.id)
                .collect();
            if reverse {
                ids.reverse();
            }
            if let Some(seed) = POLL_SEED.get() {
                let mut order = Rng(seed ^ (u64::from(t) << 32 | ids.len() as u64) | 1);
                for i in (1..ids.len()).rev() {
                    ids.swap(i, order.below(i as u32 + 1) as usize);
                }
            }
            let admit = ids
                .into_iter()
                .find(|&id| decide(&jobs, id, outside(t)).unwrap() == Admission::Admitted);
            let Some(id) = admit else { break };
            let job = jobs.iter_mut().find(|j| j.id == id).unwrap();
            job.granted = Some(cpu_demand(job, outside(t)).0);
            job.state = Running;
            started[id as usize - 1] = Some(t);
        }
        assert_within_limits(&jobs);
        assert_explained(&jobs, outside(t));
        look(&jobs);
        if started.iter().all(Option::is_some) && jobs.is_empty() {
            return started.into_iter().map(Option::unwrap).collect();
        }
    }
    panic!("the workload did not finish: {started:?}");
}

/// The running jobs use no more on any dimension than the smallest limit any
/// of them records for it (CPU: the smallest capacity).
fn assert_within_limits(jobs: &[Job]) {
    let mut used: BTreeMap<Dim, u64> = BTreeMap::new();
    let mut limits: BTreeMap<Dim, u64> = BTreeMap::new();
    for job in jobs.iter().filter(|j| j.state == Running) {
        for (dim, amount, limit) in demands(job, None) {
            *used.entry(dim).or_default() += amount;
            let limit = if dim == Dim::Cpu {
                u64::from(job.capacity)
            } else {
                limit
            };
            let entry = limits.entry(dim).or_insert(u64::MAX);
            *entry = (*entry).min(limit);
        }
    }
    for (dim, used) in used {
        assert!(used <= limits[&dim], "{dim:?}: {used} > {}", limits[&dim]);
    }
}

/// Once no job can start, `explain` says so for every job, giving reasons,
/// and agrees with the rule as each job applies it.
fn assert_explained(jobs: &[Job], outside: Option<Outside>) {
    let explained = explain(jobs, outside);
    assert_eq!(
        explained.len(),
        jobs.iter().filter(|j| j.state != Running).count()
    );
    for (id, admission) in explained {
        let job = jobs.iter().find(|j| j.id == id).unwrap();
        assert_eq!(admission, decide(jobs, id, outside).unwrap());
        match admission {
            // A parked job does not poll.
            Admission::Admitted if job.state == JobState::Parked => {}
            Admission::Admitted => panic!("job {id} could start but did not"),
            Admission::Waiting(wait) => assert!(!wait.short.is_empty(), "job {id}"),
        }
    }
}

/// xorshift64*: deterministic, no dependency needed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u32) -> u32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % u64::from(n)) as u32
    }
}

fn random_workload(rng: &mut Rng, jobs: usize, resources: bool) -> Vec<Arrival> {
    let mut at = 0;
    (0..jobs)
        .map(|_| {
            at += rng.below(3);
            let mut uses = vec![];
            if resources {
                // Some record no machine-wide capacity, some no per-checkout
                // limit, for the same `db`.
                match rng.below(7) {
                    0 | 1 => uses.push(db(1)),
                    2 => uses.push(uncapped_db(1)),
                    3 => uses.push(capped_db(1)),
                    _ => {}
                }
            }
            if resources && rng.below(3) == 0 {
                uses.push(mem(1 + rng.below(6)));
            }
            // One of it, as for a browser that a long UI capture holds.
            if resources && rng.below(4) == 0 {
                uses.push(browser(1));
            }
            // Declared with capacity 1 by some repositories, 2 by others.
            if resources && rng.below(6) == 0 {
                uses.push(res("db1", 1, 1 + rng.below(2)));
            }
            Arrival {
                at,
                duration: 1 + rng.below(6),
                weight: [1, 1, 2, 4, 8, 9][rng.below(6) as usize],
                uses,
                checkout: ["/a", "/b", "/c"][rng.below(3) as usize],
                leaves: (rng.below(5) == 0).then(|| at + 1 + rng.below(8)),
                adapt: rng.below(3) != 0,
                capacity: if rng.below(4) == 0 { 6 } else { 8 },
                parks: (rng.below(6) == 0).then(|| {
                    let p = at + rng.below(4);
                    (p, (rng.below(3) != 0).then(|| p + 1 + rng.below(5)))
                }),
            }
        })
        .collect()
}

/// Each job starts at the same time whatever the order in which the jobs
/// poll, and whether or not the jobs behind it exist.
fn check_workload(round: usize, arrivals: &[Arrival], outside: Load) {
    let all = simulate(arrivals, false, outside);
    assert_eq!(
        all,
        simulate(arrivals, true, outside),
        "round {round}: poll order"
    );
    POLL_SEED.set(Some(round as u64 * 7919 + 1));
    let shuffled = simulate(arrivals, false, outside);
    POLL_SEED.set(None);
    assert_eq!(all, shuffled, "round {round}: random poll order");
    for k in 0..arrivals.len() {
        let alone = simulate(&arrivals[..=k], false, outside);
        assert_eq!(
            alone[k],
            all[k],
            "round {round}: job {} started at {} with the later jobs, at {} without",
            k + 1,
            all[k],
            alone[k]
        );
    }
}

#[test]
fn no_job_is_delayed_by_jobs_that_got_in_line_after_it() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for round in 0..300 {
        check_workload(round, &random_workload(&mut rng, 12, true), &|_| None);
    }
}

#[test]
fn cpu_busy_outside_delays_no_job_for_jobs_behind_it_and_stalls_none() {
    // The CPU outside the queue changes on its own, sometimes taking all
    // 14 cores: every job still starts (or `simulate` panics), and still
    // when it would without the jobs that got in line after it.
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for round in 0..300 {
        let arrivals = random_workload(&mut rng, 12, true);
        let steps: Vec<f64> = (0..64).map(|_| f64::from(rng.below(15))).collect();
        let outside = |t: u32| {
            Some(Outside {
                busy: steps[(t as usize / 3) % steps.len()],
                cores: 14,
            })
        };
        check_workload(round, &arrivals, &outside);
    }
}

#[test]
fn large_capacities_keep_the_promises() {
    // Rooms above what is tracked exactly are bounded instead.
    let mut rng = Rng(0xA5A5_5A5A_1234_4321);
    for round in 0..100 {
        let mut arrivals = random_workload(&mut rng, 10, true);
        for a in &mut arrivals {
            a.capacity *= 10;
            a.weight *= 10;
        }
        check_workload(round, &arrivals, &|_| None);
    }
}

#[test]
fn cpu_busy_outside_lowers_what_jobs_start_with() {
    let outside = Some(Outside {
        busy: 10.2,
        cores: 14,
    });
    // 4 cores are left; a weight of 8 counts as all of them.
    let big = job(1, Waiting, 8, vec![], "/a");
    assert_eq!(cpu_demand(&big, outside), (4, 4));
    assert_eq!(decide(&[big], 1, outside).unwrap(), Admission::Admitted);

    let jobs = [
        Job {
            granted: Some(4),
            ..job(1, Running, 8, vec![], "/a")
        },
        job(2, Waiting, 2, vec![], "/a"),
    ];
    assert_eq!(
        said(decide(&jobs, 2, outside).unwrap()),
        "0 ahead; waits for 2 cores (4 of 4 in use; 10 of 14 cores are busy beyond what \
         running jobs got)"
    );
    assert!(
        admitted(&jobs, 2),
        "without the outside load, 4 of 8 are free"
    );
}

#[test]
fn a_measure_only_job_behind_still_counts() {
    // It started without the rule, behind a job that waits: judged by the
    // jobs ahead alone, that job would start next to it, past every limit.
    let mut measured = job(2, Running, 1, vec![db(1)], "/b");
    measured.measure_only = true;
    let jobs = [job(1, Waiting, 8, vec![db(1)], "/a"), measured];
    assert!(!admitted(&jobs, 1));
}

#[test]
fn a_running_job_keeps_the_cores_it_started_with() {
    // It started with 8; the CPU outside has since taken 12 of 14 cores.
    let outside = Some(Outside {
        busy: 12.0,
        cores: 14,
    });
    let jobs = [
        job(1, Running, 8, vec![], "/a"),
        job(2, Waiting, 1, vec![], "/a"),
    ];
    assert!(!matches!(
        decide(&jobs, 2, outside).unwrap(),
        Admission::Admitted
    ));
    assert_eq!(cpu_demand(&jobs[0], outside), (8, 8));
}

#[test]
fn the_cpu_limit_is_what_is_free_outside_the_queue_within_the_capacity() {
    let at = |busy| Outside { busy, cores: 14 };
    assert_eq!(at(0.0).cpu_limit(12), 12, "the capacity bounds it");
    assert_eq!(
        at(2.4).cpu_limit(12),
        12,
        "within the 2 cores it leaves free"
    );
    assert_eq!(at(2.6).cpu_limit(12), 11);
    assert_eq!(at(14.0).cpu_limit(12), 1, "never below one core");
    assert_eq!(at(f64::NAN).cpu_limit(12), 12, "no number lowers nothing");
    // A capacity above the cores is meant: CPU outside takes from it alone.
    assert_eq!(at(0.0).cpu_limit(20), 20);
    assert_eq!(at(3.0).cpu_limit(20), 17);
}

#[test]
fn a_job_starts_with_what_the_shared_measurement_leaves() {
    let f = fixture();
    // Fresh, so every process uses it rather than measuring again.
    f.queue
        .conn
        .execute(
            "INSERT INTO cpu (id, at, cores, outside) VALUES (1, ?1, 14, 10.0)",
            [unix_ms(SystemTime::now())],
        )
        .unwrap();
    let request = JobRequest {
        adapt_cpu: true,
        ..request("e2e", 8, &[])
    };
    let ticket = f.queue.enqueue(&request, f.me).unwrap();
    assert_eq!(ticket.try_admit().unwrap(), Admission::Admitted);
    assert_eq!(ticket.granted().unwrap(), 4);
    assert_eq!(f.queue.snapshot().unwrap().used(), 4);
}

/// Stores a measurement of the CPU taken `ago` ms ago (negative: in the
/// future).
fn store_cpu(f: &Fixture, ago: i64) {
    f.queue
        .conn
        .execute(
            "INSERT INTO cpu (id, at, cores, outside) VALUES (1, ?1, 14, 3.0)
             ON CONFLICT (id) DO UPDATE SET at = ?1",
            [unix_ms(SystemTime::now()) - ago],
        )
        .unwrap();
}

#[test]
fn the_cpu_is_measured_again_once_its_measurement_is_old() {
    let f = fixture();
    let jobs = f.queue.snapshot().unwrap().jobs;
    let took = |ago| {
        store_cpu(&f, ago);
        let start = Instant::now();
        let outside = f.queue.outside(&jobs).unwrap().unwrap();
        (start.elapsed(), outside.busy)
    };
    let (fast, busy) = took(1_000);
    assert!(fast < CPU_SAMPLE);
    assert_eq!(busy, 3.0, "the stored one");
    assert!(took(5_000).0 >= CPU_SAMPLE);
}

#[test]
fn a_measurement_is_averaged_with_a_recent_one() {
    let secs = Duration::from_secs;
    assert_eq!(smoothed(Some(8.0), Some((2.0, secs(3)))), Some(5.0));
    assert_eq!(smoothed(Some(8.0), Some((2.0, secs(20)))), Some(8.0));
    assert_eq!(smoothed(Some(8.0), None), Some(8.0));
    // Nothing measured this time: the recent one stands in.
    assert_eq!(smoothed(None, Some((2.0, secs(3)))), Some(2.0));
    assert_eq!(smoothed(None, Some((2.0, secs(20)))), None);
}

#[test]
fn a_measurement_taken_earlier_does_not_replace_a_later_one() {
    let f = fixture();
    // Stored by another process, later than this one's (its clock is ahead).
    store_cpu(&f, -60_000);
    let jobs = f.queue.snapshot().unwrap().jobs;
    f.queue.outside(&jobs).unwrap();
    let kept = read_cpu(&f.queue.conn).unwrap().unwrap();
    assert!(kept.at > SystemTime::now(), "the later one stands");
}

#[test]
fn running_jobs_of_other_layouts_are_found_by_the_columns_every_layout_keeps() {
    let dir = tempfile::tempdir().unwrap();
    let me = ProcessId::current().unwrap();
    let gone = ProcessId {
        start_time: me.start_time + 1,
        ..me
    };
    // Another layout, with only the columns every layout keeps and one more.
    let other = Connection::open(dir.path().join("queue-v2.db")).unwrap();
    other
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE jobs (id INTEGER PRIMARY KEY, task TEXT, state TEXT,
                 owner_pid INTEGER, owner_start INTEGER, child_pgid INTEGER,
                 child_start INTEGER, extra TEXT)",
        )
        .unwrap();
    let insert = |task: &str, state: &str, owner: Option<ProcessId>, child: Option<ProcessId>| {
        other
            .execute(
                "INSERT INTO jobs (task, state, owner_pid, owner_start, child_pgid, child_start)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    task,
                    state,
                    owner.map(|o| o.pid),
                    owner.map(|o| o.start_time as i64),
                    child.map(|c| c.pid),
                    child.map(|c| c.start_time as i64),
                ],
            )
            .unwrap();
    };
    insert("e2e", "running", Some(me), None);
    insert("lost", "running", Some(gone), None);
    insert("orphaned", "running", None, Some(me));
    insert("waits", "waiting", Some(me), None);
    insert("e2e", "running", Some(me), None);
    drop(other);
    // Not another layout's queue, or not readable: nothing is said of them.
    std::fs::write(dir.path().join("queue-v9.db"), "not a database").unwrap();
    std::fs::write(dir.path().join("queue-vx.db"), "").unwrap();
    std::fs::write(dir.path().join("history.jsonl"), "").unwrap();
    // This layout's own queue is not another.
    let own = Queue::open(&dir.path().join(QUEUE_DB_FILE)).unwrap();
    let running = own.enqueue(&request("own", 1, &[]), me).unwrap();
    running.admit_now().unwrap();

    assert_eq!(
        running_in_other_queues(dir.path()),
        [OtherQueue {
            file: "queue-v2.db".to_owned(),
            running: vec!["e2e".to_owned(), "orphaned".to_owned(), "e2e".to_owned()],
        }]
    );
    // Read as another loadmaster would, nothing is left beside it.
    let mut files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("queue-v2"))
        .collect();
    files.sort();
    assert_eq!(files, ["queue-v2.db"]);
    drop(running);
}

#[test]
fn a_long_line_is_judged_in_little_time() {
    // 28 jobs, each using a resource of its own with room for one, on 32
    // cores (Codex); and 40 jobs sharing a few resources.
    let own: Vec<Job> = (1..=28)
        .map(|id| {
            let name = format!("r{id}");
            sized(job(id, Waiting, 1, vec![res(&name, 1, 1)], "/a"), 32, true)
        })
        .collect();
    let shared: Vec<Job> = (1..=40)
        .map(|id| {
            let uses = match id % 4 {
                0 => vec![browser(1)],
                1 => vec![db(1)],
                2 => vec![mem(1 + id as u32 % 5)],
                _ => vec![],
            };
            let state = if id <= 6 { Running } else { Waiting };
            sized(
                job(
                    id,
                    state,
                    1 + id as u32 % 4,
                    uses,
                    ["/a", "/b"][id as usize % 2],
                ),
                32,
                true,
            )
        })
        .collect();
    for jobs in [own, shared] {
        let started = std::time::Instant::now();
        let last = jobs.last().unwrap().id;
        let _ = decide(&jobs, last, None).unwrap();
        let _ = explain(&jobs, None);
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_millis(500),
            "took {took:?}"
        );
    }
}

#[test]
fn a_set_beside_a_start_is_named_only_if_it_can_run() {
    // Job 2 records a capacity of 4: beside its start nothing else runs, so
    // job 1 is not named.
    let jobs = [
        sized(job(1, Running, 3, vec![], "/a"), 8, false),
        sized(job(2, Waiting, 4, vec![], "/a"), 4, false),
        sized(job(3, Waiting, 1, vec![], "/a"), 8, false),
    ];
    assert_eq!(
        said(decide(&jobs, 3, None).unwrap()),
        "1 ahead; waits for 1 core (4 of 4 could be in use when #2 `job2` starts; the limit is \
         the one #2 `job2` got in line with)"
    );
}

#[test]
fn limits_that_keep_growing_do_not_make_the_rule_start_over() {
    // 100 parked jobs, then 100 waiting ones that use `r`, each recording a
    // larger capacity for it than the one before (Codex).
    let mut jobs: Vec<Job> = (1..=100)
        .map(|id| sized(job(id, JobState::Parked, 1, vec![], "/a"), 8, false))
        .collect();
    jobs.extend((101..=200).map(|id| {
        let capacity = (id - 100) as u32;
        sized(
            job(id, Waiting, 1, vec![res("r", 1, capacity)], "/a"),
            8,
            false,
        )
    }));
    let started = std::time::Instant::now();
    let _ = decide(&jobs, 200, None).unwrap();
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_secs(2), "took {took:?}");
}

/// A random line: the arrivals of `random_workload`, each running (with a
/// grant), waiting or parked, of kinds as many as jobs or a few.
fn random_line(rng: &mut Rng, n: usize, kinds: Option<usize>) -> Vec<Job> {
    let made: Vec<Job> = random_workload(rng, kinds.unwrap_or(n), true)
        .into_iter()
        .enumerate()
        .map(|(i, a)| {
            sized(
                job(
                    i as JobId + 1,
                    Waiting,
                    a.weight.min(a.capacity),
                    a.uses,
                    a.checkout,
                ),
                a.capacity,
                a.adapt,
            )
        })
        .collect();
    (0..n)
        .map(|i| {
            let pick = if kinds.is_some() {
                rng.below(made.len() as u32) as usize
            } else {
                i
            };
            let mut j = made[pick].clone();
            j.id = i as JobId + 1;
            j.task = format!("job{}", j.id);
            j.state = match rng.below(4) {
                0 => Running,
                1 => JobState::Parked,
                _ => Waiting,
            };
            j.granted = (j.state == Running).then(|| 1 + rng.below(j.weight));
            j.measure_only = j.state == Running && rng.below(15) == 0;
            j
        })
        .collect()
}

#[test]
fn one_start_stands_for_its_kind() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    for round in 0..3_000 {
        let n = 2 + rng.below(20) as usize;
        let kinds = (rng.below(2) == 0).then(|| 1 + rng.below(5) as usize);
        let jobs = random_line(&mut rng, n, kinds);
        let outside = (rng.below(2) == 0).then(|| Outside {
            busy: f64::from(rng.below(140)) / 10.0,
            cores: 14,
        });
        assert_eq!(
            explain(&jobs, outside),
            super::admission::explain_every_start(&jobs, outside),
            "round {round}: {jobs:#?}"
        );
        assert_explained_agrees(&jobs, outside);
    }
}

/// `explain` and `decide` agree on every job not started.
fn assert_explained_agrees(jobs: &[Job], outside: Option<Outside>) {
    for (id, admission) in explain(jobs, outside) {
        assert_eq!(admission, decide(jobs, id, outside).unwrap(), "job {id}");
    }
}

// ---- The most any rule could start, by brute force. ----

/// Whether `set` could all run at once: on each dimension, within the
/// smallest limit any of them records (CPU: capacity). Jobs not started count
/// at their weight.
fn fits_together(set: &[&Job]) -> bool {
    let mut used: BTreeMap<Dim, u64> = BTreeMap::new();
    let mut limits: BTreeMap<Dim, u64> = BTreeMap::new();
    for job in set {
        for (dim, amount, limit) in demands(job, None) {
            let (amount, limit) = if dim == Dim::Cpu {
                (
                    u64::from(job.granted.unwrap_or(job.weight)),
                    u64::from(job.capacity),
                )
            } else {
                (amount, limit)
            };
            *used.entry(dim).or_default() += amount;
            let entry = limits.entry(dim).or_insert(u64::MAX);
            *entry = (*entry).min(limit);
        }
    }
    used.iter().all(|(d, u)| *u <= limits[d])
}

/// The most permissive decision for the job at `at` that keeps every limit
/// whatever the jobs ahead do: it fits beside the running jobs ahead, and
/// beside every job X ahead not started together with any set of the others
/// that fits with X. Jobs may end and leave at any time, so every such set
/// may be what runs when X starts.
fn most_permissive(jobs: &[Job], at: usize) -> bool {
    let k = &jobs[at];
    let ahead: Vec<&Job> = jobs[..at].iter().collect();
    let running: Vec<&Job> = ahead
        .iter()
        .copied()
        .filter(|j| j.state == Running)
        .chain([k])
        .collect();
    if !fits_together(&running) {
        return false;
    }
    for (xi, x) in ahead.iter().enumerate().filter(|(_, j)| j.state != Running) {
        let others: Vec<&Job> = (0..ahead.len())
            .filter(|&i| i != xi)
            .map(|i| ahead[i])
            .collect();
        for mask in 0u32..(1 << others.len()) {
            let mut set: Vec<&Job> = vec![x];
            set.extend(
                (0..others.len())
                    .filter(|i| mask >> i & 1 == 1)
                    .map(|i| others[i]),
            );
            if fits_together(&set) {
                set.push(k);
                if !fits_together(&set) {
                    return false;
                }
            }
        }
    }
    true
}

#[test]
fn with_limits_all_jobs_agree_on_the_rule_starts_all_it_safely_could() {
    // CPU capacity 8, the browser 1, `db` 2 and 1 per checkout, `mem` 6 for
    // every job; no CPU outside the queue, no job that adapts to it. Every
    // job that waits after a step would, if it started, risk a limit.
    let mut rng = Rng(0x5EED_1234_0000_0002);
    let mut checked = 0;
    for round in 0..150 {
        let mut at = 0;
        let arrivals: Vec<Arrival> = (0..12)
            .map(|_| {
                at += rng.below(3);
                let mut uses = vec![];
                if rng.below(3) == 0 {
                    uses.push(db(1));
                }
                if rng.below(3) == 0 {
                    uses.push(mem(1 + rng.below(6)));
                }
                if rng.below(4) == 0 {
                    uses.push(browser(1));
                }
                Arrival {
                    at,
                    duration: 1 + rng.below(8),
                    weight: [1, 1, 2, 3, 4, 5, 8][rng.below(7) as usize],
                    uses,
                    checkout: ["/a", "/b"][rng.below(2) as usize],
                    leaves: (rng.below(5) == 0).then(|| at + 1 + rng.below(8)),
                    adapt: false,
                    capacity: 8,
                    parks: (rng.below(8) == 0).then(|| (at + rng.below(3), Some(at + 4))),
                }
            })
            .collect();
        simulate_with(&arrivals, false, &|_| None, &mut |jobs| {
            for at in (0..jobs.len()).filter(|&at| jobs[at].state == Waiting) {
                checked += 1;
                assert!(
                    !most_permissive(jobs, at),
                    "round {round}: job {} waits, but could start: {jobs:#?}",
                    jobs[at].id
                );
            }
        });
    }
    assert!(checked > 1_000, "{checked}");
}

#[test]
fn a_poll_sees_what_other_processes_changed() {
    let f = fixture();
    let other = Queue::open(&f._dir.path().join(QUEUE_DB_FILE)).unwrap();
    let holder = other.enqueue(&request("test", 8, &[]), f.me).unwrap();
    assert_eq!(holder.try_admit().unwrap(), Admission::Admitted);
    let next = f.queue.enqueue(&request("lint", 1, &[]), f.me).unwrap();
    for _ in 0..3 {
        assert!(matches!(next.try_admit().unwrap(), Admission::Waiting(_)));
    }
    drop(holder);
    assert_eq!(next.try_admit().unwrap(), Admission::Admitted);
}
