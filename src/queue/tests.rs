use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    queue: Queue,
    me: ProcessId,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let queue = Queue::open(&dir.path().join(STATE_DB_FILE)).unwrap();
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
    }
}

fn waiting(ahead: u32, used: u64) -> Admission {
    Admission::Waiting {
        ahead,
        used,
        waiting_for: vec![],
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
    assert_eq!(a.try_admit(8).unwrap(), Admission::Admitted);
    assert_eq!(b.try_admit(8).unwrap(), Admission::Admitted);
    assert_eq!(c.try_admit(8).unwrap(), waiting(0, 8));
    drop(a);
    assert_eq!(c.try_admit(8).unwrap(), Admission::Admitted);
    assert_eq!(f.queue.snapshot().unwrap().used(), 5);
}

#[test]
fn small_jobs_never_overtake_the_head() {
    let f = fixture();
    let running = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    assert_eq!(running.try_admit(8).unwrap(), Admission::Admitted);
    let big = f.queue.enqueue(&request("e2e", 8, &[]), f.me).unwrap();
    let small = f.queue.enqueue(&request("lint", 1, &[]), f.me).unwrap();
    // The small job would fit, but the big one is ahead of it.
    assert_eq!(small.try_admit(8).unwrap(), waiting(1, 4));
    assert_eq!(big.try_admit(8).unwrap(), waiting(0, 4));
    drop(running);
    assert_eq!(big.try_admit(8).unwrap(), Admission::Admitted);
    assert_eq!(small.try_admit(8).unwrap(), waiting(0, 8));
}

#[test]
fn dropping_a_waiting_ticket_leaves_the_line() {
    let f = fixture();
    let running = f.queue.enqueue(&request("e2e", 8, &[]), f.me).unwrap();
    running.try_admit(8).unwrap();
    let head = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    let next = f.queue.enqueue(&request("lint", 1, &[]), f.me).unwrap();
    drop(head);
    drop(running);
    assert_eq!(next.try_admit(8).unwrap(), Admission::Admitted);
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
    assert_eq!(alive.try_admit(8).unwrap(), Admission::Admitted);
    std::mem::forget(dead);
    let snapshot = f.queue.snapshot().unwrap();
    assert_eq!(snapshot.jobs.len(), 1);
}

#[test]
fn running_job_survives_its_owner_while_child_group_lives() {
    let f = fixture();
    let ticket = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    ticket.try_admit(8).unwrap();
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
    live.try_admit(8).unwrap();
    let task = LiveGroup::start();
    f.queue.record_child(live.id(), task.group).unwrap();
    live.detach().unwrap();

    let gone = f.queue.enqueue(&request("lint", 2, &[]), f.me).unwrap();
    gone.try_admit(8).unwrap();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    child.wait().unwrap();
    let dead_group = ProcessGroup {
        pgid: child.id(),
        leader_start_time: None,
    };
    f.queue.record_child(gone.id(), dead_group).unwrap();
    gone.detach().unwrap();

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
    running.try_admit(8).unwrap();
    let first = f.queue.enqueue(&small(&args), f.me).unwrap();
    let first_id = first.id();
    first.park().unwrap();
    let later = f.queue.enqueue(&small(&[]), f.me).unwrap();
    assert!(!later.resumed(), "different args must not resume");

    let resumed = f.queue.enqueue(&small(&args), f.me).unwrap();
    assert!(resumed.resumed());
    assert_eq!(resumed.id(), first_id);
    drop(running);
    assert_eq!(resumed.try_admit(4).unwrap(), Admission::Admitted);
    assert_eq!(later.try_admit(4).unwrap(), waiting(0, 4));
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
    assert_eq!(next.try_admit(8).unwrap(), waiting(1, 0));
    let expired = unix_ms(SystemTime::now() - PARK_TTL - Duration::from_secs(1));
    f.queue
        .conn
        .execute(
            "UPDATE jobs SET parked_at = ?2 WHERE id = ?1",
            params![parked_id, expired],
        )
        .unwrap();
    assert_eq!(next.try_admit(8).unwrap(), Admission::Admitted);
}

#[test]
fn live_holder_check() {
    let f = fixture();
    let ticket = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    assert_eq!(f.queue.holder(ticket.id()).unwrap(), None, "waiting");
    ticket.try_admit(8).unwrap();
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
    running.try_admit(8).unwrap();
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
    admitted.try_admit(8).unwrap();
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
    ticket.try_admit(8).unwrap();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    child.wait().unwrap();
    let gone = ProcessGroup {
        pgid: child.id(),
        leader_start_time: None,
    };
    f.queue.record_child(ticket.id(), gone).unwrap();
    ticket.detach().unwrap();
    assert_eq!(f.queue.reap().unwrap().len(), 1);
    assert!(history(&f).is_empty());
}

#[test]
fn admit_now_ignores_the_line_and_the_capacity() {
    let f = fixture();
    let big = f.queue.enqueue(&request("e2e", 8, &[]), f.me).unwrap();
    big.try_admit(8).unwrap();
    let parked = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    parked.park().unwrap();
    let late = f.queue.enqueue(&request("lint", 4, &[]), f.me).unwrap();
    late.admit_now().unwrap();
    assert_eq!(f.queue.snapshot().unwrap().used(), 12);
}

#[test]
fn migrates_a_version_1_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_DB_FILE);
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        conn.execute(
            "INSERT INTO jobs (task, weight, state, dir, args, command, enqueued_at)
             VALUES ('old', 1, 'parked', x'2f', '[]', 'old', 0)",
            [],
        )
        .unwrap();
    }
    let queue = Queue::open(&path).unwrap();
    let version: i64 = queue
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let jobs = load_jobs(&queue.conn).unwrap();
    assert_eq!(jobs[0].capacity, None);
    assert!(!jobs[0].measure_only);
    assert_eq!(jobs[0].uses, []);
    assert_eq!(jobs[0].checkout, None);
}

#[test]
fn migrates_a_version_2_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_DB_FILE);
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute_batch("PRAGMA user_version = 2").unwrap();
        conn.execute(
            "INSERT INTO jobs (task, weight, state, dir, args, command, enqueued_at, capacity)
             VALUES ('old', 1, 'parked', x'2f', '[]', 'old', 0, 8)",
            [],
        )
        .unwrap();
    }
    let queue = Queue::open(&path).unwrap();
    let jobs = load_jobs(&queue.conn).unwrap();
    assert_eq!(jobs[0].capacity, Some(8));
    assert_eq!(jobs[0].uses, []);
    assert_eq!(jobs[0].checkout, None);
}

#[test]
fn refuses_a_newer_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_DB_FILE);
    Connection::open(&path)
        .unwrap()
        .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
        .unwrap();
    let Err(Error::SchemaVersion { found, .. }) = Queue::open(&path) else {
        panic!("expected a schema version error");
    };
    assert_eq!(found, SCHEMA_VERSION + 1);
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
    assert_eq!(job.checkout.as_deref(), Some(Path::new("/repo-a")));
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
        capacity: Some(8),
        measure_only: false,
        uses,
        checkout: Some(PathBuf::from(checkout)),
    }
}

use JobState::{Running, Waiting};

fn admitted(jobs: &[Job], id: JobId) -> bool {
    decide(jobs, id, 8).unwrap() == Admission::Admitted
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
        decide(&jobs, 3, 8).unwrap(),
        Admission::Waiting {
            ahead: 0,
            used: 4,
            waiting_for: vec!["db".into()]
        }
    );
    assert!(admitted(&jobs, 4), "a lint needs no db");
    assert!(!admitted(&jobs, 5), "job 3 is first in line for a db");
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
        decide(&same_checkout, 2, 8).unwrap(),
        Admission::Waiting {
            ahead: 0,
            used: 2,
            waiting_for: vec!["db".into()]
        }
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
    short_of_db.capacity = Some(4);
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

/// Runs `arrivals` through the admission rule in discrete time steps, letting
/// every waiting job poll (in `order`) until nothing changes, and returns
/// when each job started. Checks every limit after each step.
fn simulate(arrivals: &[Arrival], reverse: bool) -> Vec<u32> {
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
            let admit = ids.into_iter().find(|&id| admitted(&jobs, id));
            let Some(id) = admit else { break };
            jobs.iter_mut().find(|j| j.id == id).unwrap().state = Running;
            started[id as usize - 1] = Some(t);
        }
        assert_within_limits(&jobs);
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
        for (dim, amount, limit) in demands(job, 8) {
            *used.entry(dim).or_default() += amount;
            limits.insert(dim, limit);
        }
    }
    for (dim, used) in used {
        assert!(used <= limits[&dim], "{dim:?}: {used} > {}", limits[&dim]);
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
        let all = simulate(&arrivals, false);
        assert_eq!(all, simulate(&arrivals, true), "round {round}: poll order");
        for k in 0..arrivals.len() {
            let alone = simulate(&arrivals[..=k], false);
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
fn with_cpu_alone_jobs_start_in_line_order() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for round in 0..300 {
        let arrivals = random_workload(&mut rng, 12, false);
        let started = simulate(&arrivals, false);
        assert!(
            started.windows(2).all(|w| w[0] <= w[1]),
            "round {round}: {started:?}"
        );
    }
}
