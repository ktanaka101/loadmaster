use super::*;

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
            let mut text = format!("{} ahead; waits for {}", wait.ahead, wait.waits_for());
            if let Some(reserved) = wait.reserves_text() {
                text += &format!("; sets aside {reserved}");
            }
            text
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
fn small_jobs_never_overtake_the_head() {
    let f = fixture();
    let running = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    assert_eq!(running.try_admit().unwrap(), Admission::Admitted);
    let big = f.queue.enqueue(&request("e2e", 8, &[]), f.me).unwrap();
    let small = f.queue.enqueue(&request("lint", 1, &[]), f.me).unwrap();
    // The small job would fit, but the big one is ahead of it.
    assert_eq!(
        said(small.try_admit().unwrap()),
        "1 ahead; waits for 1 core (behind #2 `e2e`, which waits for them too)"
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
        "1 ahead; waits for 1 core (8 free, but 8 set aside for #1 `e2e`)"
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

#[test]
fn a_job_short_of_a_resource_holds_back_only_that_resource() {
    let jobs = [
        job(1, Running, 2, vec![db(1)], "/a"),
        job(2, Running, 2, vec![db(1)], "/b"),
        job(3, Waiting, 2, vec![db(1)], "/c"),
        job(4, Waiting, 1, vec![], "/a"),
        job(5, Waiting, 2, vec![db(1)], "/d"),
    ];
    assert_eq!(
        said(decide(&jobs, 3, None).unwrap()),
        "0 ahead; waits for `db` (2 of 2 in use by #1 `job1` and #2 `job2`); \
         sets aside 2 cores and `db` in its checkout"
    );
    assert!(admitted(&jobs, 4), "a lint needs no db");
    assert!(!admitted(&jobs, 5), "job 3 is first in line for a db");
}

#[test]
fn a_job_waiting_for_a_resource_says_what_it_sets_aside() {
    // A long UI capture holds the one browser; a unit run that needs it too
    // sets its cores aside while it waits, and a lint no longer fits.
    let browser = |amount| Use {
        name: "browser".into(),
        amount,
        capacity: Some(1),
        per_checkout: None,
    };
    let mut jobs = vec![
        job(1, Running, 5, vec![browser(1)], "/a"),
        job(2, Waiting, 2, vec![browser(1)], "/b"),
        job(3, Waiting, 2, vec![], "/c"),
    ];
    jobs[0].task = "ui".into();
    jobs[1].task = "unit".into();
    jobs[2].task = "lint".into();
    let said_for = |id| said(decide(&jobs, id, None).unwrap());
    assert_eq!(
        said_for(2),
        "0 ahead; waits for `browser` (1 of 1 in use by #1 `ui`); sets aside 2 cores"
    );
    assert_eq!(
        said_for(3),
        "1 ahead; waits for 2 cores (3 free, but 2 set aside for #2 `unit`)"
    );
}

#[test]
fn a_smaller_limit_ahead_is_named_as_the_cause() {
    // A job queued under capacity 1 is parked ahead: the job behind it fits
    // in its own capacity of 8, but respects that one's.
    let mut small = job(1, JobState::Parked, 1, vec![], "/a");
    small.capacity = 1;
    let jobs = [small, job(2, Waiting, 4, vec![], "/b")];
    assert_eq!(
        said(decide(&jobs, 2, None).unwrap()),
        "1 ahead; waits for 4 cores (#1 `job1`, ahead, got in line with a limit of 1)"
    );
}

#[test]
fn what_a_waiting_job_sets_aside_includes_unlimited_resources() {
    // Job 2 records no limit for `gpu`, but sets it aside, and job 3, which
    // has one, waits for it.
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
        said(decide(&jobs, 2, None).unwrap()),
        "0 ahead; waits for `db` in its checkout (1 of 1 in use by #1 `job1`); \
         sets aside 1 core, `db` and `gpu`"
    );
    assert_eq!(
        said(decide(&jobs, 3, None).unwrap()),
        "1 ahead; waits for `gpu` (1 free, but 1 set aside for #2 `job2`); sets aside 1 core"
    );
}

#[test]
fn a_job_short_of_cpu_holds_back_everyone_behind_it() {
    let jobs = [
        job(1, Running, 6, vec![], "/a"),
        job(2, Waiting, 4, vec![], "/a"),
        job(3, Waiting, 1, vec![], "/b"),
    ];
    assert!(!admitted(&jobs, 3), "1 core is free, but job 2 needs it");
}

#[test]
fn jobs_behind_leave_a_waiting_job_its_share() {
    let jobs = [
        job(1, Running, 2, vec![db(1)], "/a"),
        job(2, Running, 2, vec![db(1)], "/b"),
        // Short of a db; it fits on CPU, so 2 cores are set aside for it.
        job(3, Waiting, 2, vec![db(1)], "/c"),
        job(4, Waiting, 2, vec![], "/a"),
        // 4 running + 2 set aside for job 3 + 2 for job 4, about to start.
        job(5, Waiting, 1, vec![], "/a"),
    ];
    assert!(admitted(&jobs, 4));
    assert!(!admitted(&jobs, 5));
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
        "0 ahead; waits for `db` in its checkout (1 of 1 in use by #1 `job1`); \
         sets aside 2 cores and `db`"
    );
    // Job 2 still sets aside a machine-wide db: the rule cannot know that
    // job 1 will free both, and it never risks delaying a job ahead.
    assert!(!admitted(&same_checkout, 3));
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
    assert!(!admitted(&jobs, 4), "job 3 is short of a db ahead of it");
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
fn a_job_behind_respects_the_smallest_limit_ahead() {
    // Processes may disagree on a capacity. Job 1 got in line with CPU
    // capacity 4 and waits for a db; job 2 says 8, but taking 3 cores would
    // leave job 1 too little by its own measure once a db frees up.
    let mut short_of_db = job(1, Waiting, 3, vec![db(1)], "/a");
    short_of_db.capacity = 4;
    let jobs = [
        job(3, Running, 1, vec![db(1)], "/a"),
        short_of_db,
        job(4, Waiting, 3, vec![], "/b"),
    ];
    let mut jobs = jobs.to_vec();
    jobs.sort_by_key(|j| j.id);
    assert!(!admitted(&jobs, 4));
}

/// A workload for the simulation: jobs in line order.
#[derive(Clone)]
struct Arrival {
    at: u32,
    duration: u32,
    weight: u32,
    uses: Vec<Use>,
    checkout: &'static str,
}

/// CPU busy outside the queue at each time step, of a machine with 14 cores
/// (the jobs' capacity is 8).
type Load<'a> = &'a dyn Fn(u32) -> Option<Outside>;

/// Runs `arrivals` through the admission rule in discrete time steps, letting
/// every waiting job poll (in `order`) until nothing changes, and returns
/// when each job started. Checks every limit after each step.
fn simulate(arrivals: &[Arrival], reverse: bool, outside: Load) -> Vec<u32> {
    let mut jobs: Vec<Job> = Vec::new();
    let mut started = vec![None; arrivals.len()];
    for t in 0..10_000 {
        // Finish, then arrive.
        jobs.retain(|j| {
            let i = j.id as usize - 1;
            started[i].is_none_or(|s: u32| s + arrivals[i].duration > t)
        });
        for (i, a) in arrivals.iter().enumerate().filter(|(_, a)| a.at == t) {
            let id = i as JobId + 1;
            jobs.push(job(
                id,
                Waiting,
                a.weight.min(8),
                a.uses.clone(),
                a.checkout,
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
        if started.iter().all(Option::is_some) && jobs.is_empty() {
            return started.into_iter().map(Option::unwrap).collect();
        }
    }
    panic!("the workload did not finish: {started:?}");
}

fn assert_within_limits(jobs: &[Job]) {
    let mut used: BTreeMap<Dim, u64> = BTreeMap::new();
    let mut limits: BTreeMap<Dim, u64> = BTreeMap::new();
    for job in jobs.iter().filter(|j| j.state == Running) {
        for (dim, amount, limit) in demands(job, None) {
            *used.entry(dim).or_default() += amount;
            limits.insert(dim, limit);
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
        assert_eq!(admission, decide(jobs, id, outside).unwrap());
        match admission {
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
                // Some jobs record no machine-wide capacity for `db`.
                match rng.below(6) {
                    0 | 1 => uses.push(db(1)),
                    2 => uses.push(uncapped_db(1)),
                    _ => {}
                }
            }
            if resources && rng.below(3) == 0 {
                uses.push(mem(1 + rng.below(6)));
            }
            Arrival {
                at,
                duration: 1 + rng.below(6),
                weight: [1, 1, 2, 4, 8, 9][rng.below(6) as usize],
                uses,
                checkout: ["/a", "/b", "/c"][rng.below(3) as usize],
            }
        })
        .collect()
}

#[test]
fn no_job_is_delayed_by_jobs_that_got_in_line_after_it() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for round in 0..300 {
        let arrivals = random_workload(&mut rng, 12, true);
        let all = simulate(&arrivals, false, &|_| None);
        assert_eq!(
            all,
            simulate(&arrivals, true, &|_| None),
            "round {round}: poll order"
        );
        for k in 0..arrivals.len() {
            let alone = simulate(&arrivals[..=k], false, &|_| None);
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
        let all = simulate(&arrivals, false, &outside);
        assert_eq!(
            all,
            simulate(&arrivals, true, &outside),
            "round {round}: poll order"
        );
        for k in 0..arrivals.len() {
            let alone = simulate(&arrivals[..=k], false, &outside);
            assert_eq!(alone[k], all[k], "round {round}: job {}", k + 1);
        }
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
        "0 ahead; waits for 2 cores (4 of 4 in use; 10 of 14 cores are busy outside the \
         queue)"
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
fn with_cpu_alone_jobs_start_in_line_order() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for round in 0..300 {
        let arrivals = random_workload(&mut rng, 12, false);
        let started = simulate(&arrivals, false, &|_| None);
        assert!(
            started.windows(2).all(|w| w[0] <= w[1]),
            "round {round}: {started:?}"
        );
    }
}
