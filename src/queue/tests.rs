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
    assert_eq!(
        c.try_admit(8).unwrap(),
        Admission::Waiting { ahead: 0, used: 8 }
    );
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
    assert_eq!(
        small.try_admit(8).unwrap(),
        Admission::Waiting { ahead: 1, used: 4 }
    );
    assert_eq!(
        big.try_admit(8).unwrap(),
        Admission::Waiting { ahead: 0, used: 4 }
    );
    drop(running);
    assert_eq!(big.try_admit(8).unwrap(), Admission::Admitted);
    assert_eq!(
        small.try_admit(8).unwrap(),
        Admission::Waiting { ahead: 0, used: 8 }
    );
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
    let running = f.queue.enqueue(&request("e2e", 8, &[]), f.me).unwrap();
    running.try_admit(8).unwrap();
    let first = f.queue.enqueue(&request("test", 4, &args), f.me).unwrap();
    let first_id = first.id();
    first.park().unwrap();
    let later = f.queue.enqueue(&request("test", 4, &[]), f.me).unwrap();
    assert!(!later.resumed(), "different args must not resume");

    let resumed = f.queue.enqueue(&request("test", 4, &args), f.me).unwrap();
    assert!(resumed.resumed());
    assert_eq!(resumed.id(), first_id);
    drop(running);
    assert_eq!(resumed.try_admit(4).unwrap(), Admission::Admitted);
    assert_eq!(
        later.try_admit(4).unwrap(),
        Admission::Waiting { ahead: 0, used: 4 }
    );
}

#[test]
fn parked_head_blocks_until_it_expires() {
    let f = fixture();
    let parked = f.queue.enqueue(&request("e2e", 8, &[]), f.me).unwrap();
    let parked_id = parked.id();
    parked.park().unwrap();
    let next = f.queue.enqueue(&request("lint", 1, &[]), f.me).unwrap();
    assert_eq!(
        next.try_admit(8).unwrap(),
        Admission::Waiting { ahead: 1, used: 0 }
    );
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
    assert!(!f.queue.is_live_holder(ticket.id()).unwrap(), "waiting");
    ticket.try_admit(8).unwrap();
    assert!(f.queue.is_live_holder(ticket.id()).unwrap());
    assert!(!f.queue.is_live_holder(ticket.id() + 100).unwrap());
    let id = ticket.id();
    drop(ticket);
    assert!(!f.queue.is_live_holder(id).unwrap());
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
    f.queue
        .conn
        .execute(
            "UPDATE jobs SET owner_start = owner_start + 1, started_at = enqueued_at + 250
             WHERE id = ?1",
            [running.id()],
        )
        .unwrap();
    std::mem::forget(running);
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
    assert_eq!(records.len(), 2, "{records:?}");
    for (record, task) in records.iter().zip(["lint", "test"]) {
        assert_eq!(record["task"], task);
        assert_eq!(record["outcome"], "lost");
        assert_eq!(record["capacity"], 8);
        assert_eq!(record["exit_code"], serde_json::Value::Null);
    }
    assert_eq!(records[1]["wait_ms"], 250);
    // Reaped once, logged once.
    f.queue.reap().unwrap();
    assert_eq!(history(&f).len(), 2);
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
        let v1 = SCHEMA.replace(
            ",\n    capacity     INTEGER,\n    measure_only INTEGER NOT NULL DEFAULT 0",
            "",
        );
        assert!(!v1.contains("capacity"));
        conn.execute_batch(&v1).unwrap();
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
}
