//! The machine-wide job queue, kept in SQLite.
//!
//! **Admission rule.** A job starts only when it is at the head of the line
//! *and* `used + weight <= capacity`. The line is strictly FIFO: a small job
//! never overtakes a big one waiting at the head, so big jobs cannot starve.
//!
//! **Ordering.** Arrival order is the `INTEGER PRIMARY KEY` assigned inside a
//! `BEGIN IMMEDIATE` transaction, so it always matches the order in which the
//! writes happened. Wall-clock times are only displayed, never compared.
//!
//! **Liveness.** Every state change runs in a `BEGIN IMMEDIATE` transaction and
//! first reaps jobs nobody holds any more:
//! - a *waiting* job dies with its loadmaster process;
//! - a *running* job lives while its loadmaster process **or** its child
//!   process group is alive, so killing only loadmaster does not free a slot
//!   whose work is still running;
//! - a *parked* job (see [`Ticket::park`]) keeps its place for [`PARK_TTL`].

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::error::{Error, Result};
use crate::process::{ProcessGroup, ProcessId};

pub type JobId = i64;

/// File name of the queue database inside the state directory.
pub const STATE_DB_FILE: &str = "state.db";

/// How long a parked job keeps its place in line without an owner.
pub const PARK_TTL: Duration = Duration::from_secs(120);

const SCHEMA_VERSION: i64 = 1;

// AUTOINCREMENT: IDs are never reused, so a stale LOADMASTER_HELD can never
// point at an unrelated newer job.
const SCHEMA: &str = "
CREATE TABLE jobs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    task        TEXT    NOT NULL,
    weight      INTEGER NOT NULL CHECK (weight > 0),
    state       TEXT    NOT NULL CHECK (state IN ('waiting', 'parked', 'running')),
    dir         BLOB    NOT NULL,
    args        TEXT    NOT NULL,
    command     TEXT    NOT NULL,
    owner_pid   INTEGER,
    owner_start INTEGER,
    child_pgid  INTEGER,
    child_start INTEGER,
    enqueued_at INTEGER NOT NULL,
    parked_at   INTEGER,
    started_at  INTEGER
);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Waiting,
    /// Gave up waiting (`--max-wait`) but keeps its place for a re-run.
    Parked,
    Running,
}

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            JobState::Waiting => "waiting",
            JobState::Parked => "parked",
            JobState::Running => "running",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<JobState> {
        match s {
            "waiting" => Ok(JobState::Waiting),
            "parked" => Ok(JobState::Parked),
            "running" => Ok(JobState::Running),
            other => Err(rusqlite::Error::InvalidColumnType(
                0,
                format!("state={other}"),
                rusqlite::types::Type::Text,
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: JobId,
    pub task: String,
    pub weight: u32,
    pub state: JobState,
    /// The caller's working directory.
    pub dir: PathBuf,
    /// Extra arguments given after `--`.
    pub args: Vec<String>,
    /// Human-readable command line.
    pub command: String,
    /// The loadmaster process holding this job; `None` while parked.
    pub owner: Option<ProcessId>,
    /// The task's process group, once started.
    pub child: Option<ProcessGroup>,
    pub enqueued_at: SystemTime,
    pub parked_at: Option<SystemTime>,
    pub started_at: Option<SystemTime>,
}

impl Job {
    fn owner_alive(&self) -> bool {
        self.owner.is_some_and(|owner| owner.is_alive())
    }

    /// Whether anyone still holds this job's place or slot.
    fn is_live(&self, now: SystemTime) -> bool {
        match self.state {
            JobState::Waiting => self.owner_alive(),
            JobState::Parked => self
                .parked_at
                .is_some_and(|at| now.duration_since(at).unwrap_or_default() < PARK_TTL),
            JobState::Running => {
                self.owner_alive() || self.child.is_some_and(|child| child.is_alive())
            }
        }
    }

    /// Running, but its loadmaster process is gone; the slot is held until
    /// the task's process group exits.
    pub fn is_orphaned(&self) -> bool {
        self.state == JobState::Running && !self.owner_alive()
    }
}

/// What a caller wants to run.
#[derive(Debug, Clone, Copy)]
pub struct JobRequest<'a> {
    pub task: &'a str,
    pub weight: u32,
    /// The caller's working directory. Together with `task` and `args`, this
    /// is the key for resuming a parked job.
    pub dir: &'a Path,
    pub args: &'a [String],
    /// Human-readable command line, for `status`.
    pub command: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Admitted,
    Waiting {
        /// Jobs ahead in line.
        ahead: u32,
        /// Weight currently running.
        used: u64,
    },
}

/// The queue at one point in time.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// All jobs, in line order.
    pub jobs: Vec<Job>,
    /// Jobs reaped while taking this snapshot.
    pub reaped: Vec<Job>,
}

impl Snapshot {
    /// Weight currently running. A sum of `u32` weights cannot overflow `u64`.
    pub fn used(&self) -> u64 {
        self.jobs
            .iter()
            .filter(|j| j.state == JobState::Running)
            .map(|j| u64::from(j.weight))
            .sum()
    }
}

pub struct Queue {
    conn: Connection,
    path: PathBuf,
}

impl Queue {
    /// Opens (creating if needed) the queue database at `path`.
    pub fn open(path: &Path) -> Result<Queue> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(60))?;
        // Losing the queue on power loss is fine: after a reboot every PID in
        // it is dead and gets reaped anyway.
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous = NORMAL")?;
        let queue = Queue {
            conn,
            path: path.to_owned(),
        };
        queue.migrate()?;
        Ok(queue)
    }

    fn migrate(&self) -> Result<()> {
        // Usually the schema is already there; don't take the write lock then.
        let found: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if found == SCHEMA_VERSION {
            return Ok(());
        }
        let tx = self.begin()?;
        let found: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if found == 0 {
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        } else if found != SCHEMA_VERSION {
            return Err(Error::SchemaVersion {
                path: self.path.clone(),
                found,
                expected: SCHEMA_VERSION,
            });
        }
        tx.commit()?;
        Ok(())
    }

    fn begin(&self) -> rusqlite::Result<Transaction<'_>> {
        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
    }

    /// Gets in line, or takes over a parked job with the same
    /// (`dir`, `task`, `args`) and with it that job's place.
    pub fn enqueue(&self, request: &JobRequest, owner: ProcessId) -> Result<Ticket<'_>> {
        // Raw bytes: a lossy string could make two directories look equal.
        let dir = request.dir.as_os_str().as_bytes();
        let args = serde_json::to_string(request.args).expect("strings always serialize");
        let tx = self.begin()?;
        reap(&tx)?;
        let parked: Option<JobId> = tx
            .query_row(
                "SELECT id FROM jobs
                 WHERE state = 'parked' AND dir = ?1 AND task = ?2 AND args = ?3
                 ORDER BY id LIMIT 1",
                params![dir, request.task, args],
                |r| r.get(0),
            )
            .optional()?;
        let (id, resumed) = match parked {
            Some(id) => {
                tx.execute(
                    "UPDATE jobs SET state = 'waiting', weight = ?2, command = ?3,
                         owner_pid = ?4, owner_start = ?5, parked_at = NULL
                     WHERE id = ?1",
                    params![
                        id,
                        request.weight,
                        request.command,
                        owner.pid,
                        owner.start_time as i64
                    ],
                )?;
                (id, true)
            }
            None => {
                tx.execute(
                    "INSERT INTO jobs (task, weight, state, dir, args, command,
                                       owner_pid, owner_start, enqueued_at)
                     VALUES (?1, ?2, 'waiting', ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        request.task,
                        request.weight,
                        dir,
                        args,
                        request.command,
                        owner.pid,
                        owner.start_time as i64,
                        unix_ms(SystemTime::now())
                    ],
                )?;
                (tx.last_insert_rowid(), false)
            }
        };
        tx.commit()?;
        Ok(Ticket {
            queue: self,
            id,
            resumed,
            armed: true,
        })
    }

    /// Reaps dead jobs and returns everything left.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let tx = self.begin()?;
        let reaped = reap(&tx)?;
        let jobs = load_jobs(&tx)?;
        tx.commit()?;
        Ok(Snapshot { jobs, reaped })
    }

    /// Removes jobs nobody holds any more and returns them.
    pub fn reap(&self) -> Result<Vec<Job>> {
        let tx = self.begin()?;
        let reaped = reap(&tx)?;
        tx.commit()?;
        Ok(reaped)
    }

    /// Whether `id` is a running job that is still alive, i.e. a valid
    /// holder for nested `loadmaster run` calls.
    pub fn is_live_holder(&self, id: JobId) -> Result<bool> {
        let tx = self.begin()?;
        reap(&tx)?;
        let state: Option<String> = tx
            .query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        tx.commit()?;
        Ok(state.as_deref() == Some(JobState::Running.as_str()))
    }

    /// Records the process group of job `id`'s task, so the slot stays held
    /// while the group runs even if the job's loadmaster process dies.
    pub fn record_child(&self, id: JobId, child: ProcessGroup) -> Result<()> {
        let tx = self.begin()?;
        tx.execute(
            "UPDATE jobs SET child_pgid = ?2, child_start = ?3 WHERE id = ?1",
            params![id, child.pgid, child.leader_start_time.map(|t| t as i64)],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn remove(&self, id: JobId) -> Result<()> {
        let tx = self.begin()?;
        reap(&tx)?;
        tx.execute("DELETE FROM jobs WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }
}

/// A place in line, and later a slot. Dropping it leaves the queue (or frees
/// the slot), so no normal code path can leak capacity.
pub struct Ticket<'q> {
    queue: &'q Queue,
    id: JobId,
    resumed: bool,
    armed: bool,
}

impl Ticket<'_> {
    pub fn id(&self) -> JobId {
        self.id
    }

    /// Whether this ticket took over a parked job's place rather than joining
    /// the back of the line.
    pub fn resumed(&self) -> bool {
        self.resumed
    }

    /// Starts the job if it is at the head of the line and fits.
    pub fn try_admit(&self, capacity: u32) -> Result<Admission> {
        // Look without the write lock first: most polls change nothing, and
        // taking the lock for them would make every waiter contend with the
        // few writes that matter (admitting the head, recording a task).
        // Only running jobs and those ahead can change this job's decision;
        // dead jobs further back are reaped by whoever they are ahead of.
        let jobs = load_jobs(&self.queue.conn)?;
        let now = SystemTime::now();
        let relevant_alive = jobs
            .iter()
            .filter(|job| job.state == JobState::Running || job.id < self.id)
            .all(|job| job.is_live(now));
        if relevant_alive
            && let Ok(waiting @ Admission::Waiting { .. }) = decide(&jobs, self.id, capacity)
        {
            return Ok(waiting);
        }
        let tx = self.queue.begin()?;
        reap(&tx)?;
        let jobs = load_jobs(&tx)?;
        let admission = decide(&jobs, self.id, capacity)?;
        let running = jobs
            .iter()
            .any(|job| job.id == self.id && job.state == JobState::Running);
        if admission == Admission::Admitted && !running {
            tx.execute(
                "UPDATE jobs SET state = 'running', started_at = ?2 WHERE id = ?1",
                params![self.id, unix_ms(SystemTime::now())],
            )?;
        }
        tx.commit()?;
        Ok(admission)
    }

    /// Hands the slot over to the task's process group: it stays held until
    /// the group has exited, without this process. For a task whose main
    /// process exited while others it started are still running.
    pub fn detach(mut self) -> Result<()> {
        self.armed = false;
        let tx = self.queue.begin()?;
        tx.execute(
            "UPDATE jobs SET owner_pid = NULL, owner_start = NULL WHERE id = ?1",
            [self.id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Stops waiting but keeps the place in line for [`PARK_TTL`]: an
    /// [`Queue::enqueue`] with the same (`dir`, `task`, `args`) resumes it.
    pub fn park(mut self) -> Result<()> {
        self.armed = false;
        let tx = self.queue.begin()?;
        let parked = tx.execute(
            "UPDATE jobs SET state = 'parked', owner_pid = NULL, owner_start = NULL,
                 parked_at = ?2
             WHERE id = ?1 AND state = 'waiting'",
            params![self.id, unix_ms(SystemTime::now())],
        )?;
        tx.commit()?;
        if parked == 0 {
            return Err(Error::JobVanished(self.id));
        }
        Ok(())
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(e) = self.queue.remove(self.id) {
            // Not eprintln!: it panics if stderr is a closed pipe.
            let _ = writeln!(
                std::io::stderr(),
                "loadmaster: warning: could not release job #{}: {e} \
                 (it will be reclaimed once this process exits)",
                self.id
            );
        }
    }
}

/// The admission rule: job `id` may start if it is at the head of the line
/// and fits in what is left of `capacity`.
fn decide(jobs: &[Job], id: JobId, capacity: u32) -> Result<Admission> {
    let me = jobs
        .iter()
        .find(|job| job.id == id)
        .ok_or(Error::JobVanished(id))?;
    if me.state == JobState::Running {
        return Ok(Admission::Admitted);
    }
    // A sum of u32 weights cannot overflow u64.
    let used: u64 = jobs
        .iter()
        .filter(|job| job.state == JobState::Running)
        .map(|job| u64::from(job.weight))
        .sum();
    let ahead = jobs
        .iter()
        .filter(|job| job.state != JobState::Running && job.id < id)
        .count();
    let ahead = u32::try_from(ahead).unwrap_or(u32::MAX);
    Ok(
        if ahead == 0 && used + u64::from(me.weight) <= u64::from(capacity) {
            Admission::Admitted
        } else {
            Admission::Waiting { ahead, used }
        },
    )
}

fn reap(tx: &Transaction) -> Result<Vec<Job>> {
    let now = SystemTime::now();
    let dead: Vec<Job> = load_jobs(tx)?
        .into_iter()
        .filter(|job| !job.is_live(now))
        .collect();
    for job in &dead {
        tx.execute("DELETE FROM jobs WHERE id = ?1", [job.id])?;
    }
    Ok(dead)
}

fn load_jobs(conn: &Connection) -> Result<Vec<Job>> {
    let mut stmt = conn.prepare(
        "SELECT id, task, weight, state, dir, args, command, owner_pid, owner_start,
                child_pgid, child_start, enqueued_at, parked_at, started_at
         FROM jobs ORDER BY id",
    )?;
    let jobs = stmt
        .query_map([], job_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(jobs)
}

fn job_from_row(row: &Row) -> rusqlite::Result<Job> {
    let args: String = row.get(5)?;
    let owner_pid: Option<u32> = row.get(7)?;
    let owner_start: Option<i64> = row.get(8)?;
    let child_pgid: Option<u32> = row.get(9)?;
    let child_start: Option<i64> = row.get(10)?;
    Ok(Job {
        id: row.get(0)?,
        task: row.get(1)?,
        weight: row.get(2)?,
        state: JobState::parse(&row.get::<_, String>(3)?)?,
        dir: PathBuf::from(OsString::from_vec(row.get(4)?)),
        args: serde_json::from_str(&args).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into())
        })?,
        command: row.get(6)?,
        owner: owner_pid.zip(owner_start).map(|(pid, start)| ProcessId {
            pid,
            start_time: start as u64,
        }),
        child: child_pgid.map(|pgid| ProcessGroup {
            pgid,
            leader_start_time: child_start.map(|t| t as u64),
        }),
        enqueued_at: from_unix_ms(row.get(11)?),
        parked_at: row.get::<_, Option<i64>>(12)?.map(from_unix_ms),
        started_at: row.get::<_, Option<i64>>(13)?.map(from_unix_ms),
    })
}

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn from_unix_ms(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

#[cfg(test)]
mod tests {
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
        }
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
}
