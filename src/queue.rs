//! The machine-wide job queue, kept in SQLite.
//!
//! **Admission rule.** Jobs are considered in line order, on every dimension
//! at once: CPU (the weight), each machine-wide resource and each per-checkout
//! resource. A job starts if it fits in what running jobs use plus what is set
//! aside for the jobs ahead of it. A job that does not fit sets aside its
//! amount where it fits and holds back entirely the dimensions it is short
//! of, so no job is ever delayed by jobs that got in line after it. Every job
//! uses CPU, so with CPU alone this is a strictly FIFO line: a small job never
//! overtakes a big one waiting at the head, and big jobs cannot starve.
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
//!
//! A reaped job whose loadmaster process was killed never wrote its history
//! record, so the reaper writes one for it (outcome `lost`).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::history::{self, Outcome, Record};
use crate::process::{ProcessGroup, ProcessId};

pub type JobId = i64;

/// File name of the queue database inside the state directory.
pub const STATE_DB_FILE: &str = "state.db";

/// How long to wait for another process's lock on the database.
const BUSY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a parked job keeps its place in line without an owner.
pub const PARK_TTL: Duration = Duration::from_secs(120);

// AUTOINCREMENT: IDs are never reused, so a stale LOADMASTER_HELD can never
// point at an unrelated newer job. This is version 1; a new database is
// created from it and brought up to date by every migration in turn.
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

/// `MIGRATIONS[n - 1]` upgrades version `n` to `n + 1`.
const MIGRATIONS: [&str; 2] = [
    // The machine setting of each job, so a lost job can still be logged.
    "ALTER TABLE jobs ADD COLUMN capacity INTEGER;
     ALTER TABLE jobs ADD COLUMN measure_only INTEGER NOT NULL DEFAULT 0;",
    // Resources: what each job uses, and the checkout it was started from.
    "ALTER TABLE jobs ADD COLUMN checkout BLOB;
     ALTER TABLE jobs ADD COLUMN uses TEXT NOT NULL DEFAULT '[]';",
];

const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64 + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Waiting,
    /// Gave up waiting (`--max-wait`) but keeps its place for a re-run.
    Parked,
    Running,
}

impl JobState {
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
    /// The machine's capacity when the job got in line (unknown for jobs
    /// queued before it was recorded).
    pub capacity: Option<u32>,
    pub measure_only: bool,
    /// Resources the job uses, with the limits it got in line with.
    pub uses: Vec<Use>,
    /// The resolved directory of the `.loadmaster.toml` the job was started
    /// from (unknown for jobs queued before it was recorded).
    pub checkout: Option<PathBuf>,
}

/// A resource a job uses, and the limits it got in line with: every process
/// judges a job by these, whatever its own config says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Use {
    pub name: String,
    pub amount: u32,
    /// Machine-wide capacity; `None` if the resource is not limited
    /// machine-wide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
    /// Limit for the jobs of one checkout, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_checkout: Option<u32>,
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

    /// Reaped while it still had an owner: its loadmaster process was killed
    /// before it could log the result. Parked and handed-over jobs have none.
    fn was_lost(&self) -> bool {
        self.owner.is_some() && self.state != JobState::Parked
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
    /// The machine setting, kept so a lost job can still be logged.
    pub capacity: u32,
    pub measure_only: bool,
    pub uses: &'a [Use],
    /// The resolved directory of the `.loadmaster.toml` in use.
    pub checkout: &'a Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Admitted,
    Waiting {
        /// Jobs ahead in line.
        ahead: u32,
        /// Weight currently running.
        used: u64,
        /// Resources other than CPU that the job waits for.
        waiting_for: Vec<String>,
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
        conn.busy_timeout(BUSY_TIMEOUT)?;
        enable_wal(&conn)?;
        // Losing the queue on power loss is fine: after a reboot every PID in
        // it is dead and gets reaped anyway.
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
        if !(0..=SCHEMA_VERSION).contains(&found) {
            return Err(Error::SchemaVersion {
                path: self.path.clone(),
                found,
                expected: SCHEMA_VERSION,
            });
        }
        if found == 0 {
            tx.execute_batch(SCHEMA)?;
        }
        for migration in &MIGRATIONS[found.max(1) as usize - 1..] {
            tx.execute_batch(migration)?;
        }
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        tx.commit()?;
        Ok(())
    }

    fn begin(&self) -> rusqlite::Result<Transaction<'_>> {
        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
    }

    /// Runs `f` in a write transaction, after reaping dead jobs. Once that is
    /// committed, logs the reaped jobs that were lost, and returns them all.
    fn write<T>(&self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<(T, Vec<Job>)> {
        let tx = self.begin()?;
        let reaped = reap(&tx)?;
        let value = f(&tx)?;
        tx.commit()?;
        self.log_lost(&reaped);
        Ok((value, reaped))
    }

    fn log_lost(&self, reaped: &[Job]) {
        let Some(state_dir) = self.path.parent() else {
            return;
        };
        let now = SystemTime::now();
        for job in reaped.iter().filter(|job| job.was_lost()) {
            let waited_until = job.started_at.unwrap_or(now);
            let record = Record {
                job: job.id,
                task: &job.task,
                weight: job.weight,
                capacity: job.capacity,
                measure_only: job.measure_only,
                dir: job.dir.to_string_lossy(),
                args: &job.args,
                outcome: Outcome::Lost,
                queued_at: history::unix_ms(job.enqueued_at),
                wait_ms: millis(
                    waited_until
                        .duration_since(job.enqueued_at)
                        .unwrap_or_default(),
                ),
                run_ms: None,
                cpu_ms: None,
                load: None,
                signal: None,
                exit_code: None,
            };
            if let Err(e) = history::append(state_dir, &record) {
                let _ = writeln!(
                    std::io::stderr(),
                    "loadmaster: warning: cannot write the history log: {e}"
                );
            }
        }
    }

    /// Gets in line, or takes over a parked job with the same
    /// (`dir`, `task`, `args`) and with it that job's place.
    pub fn enqueue(&self, request: &JobRequest, owner: ProcessId) -> Result<Ticket<'_>> {
        // Raw bytes: a lossy string could make two directories look equal.
        let dir = request.dir.as_os_str().as_bytes();
        let checkout = request.checkout.as_os_str().as_bytes();
        let args = serde_json::to_string(request.args).expect("strings always serialize");
        let uses = serde_json::to_string(request.uses).expect("uses always serialize");
        let ((id, resumed), _) = self.write(|tx| {
            let parked: Option<JobId> = tx
                .query_row(
                    "SELECT id FROM jobs
                 WHERE state = 'parked' AND dir = ?1 AND task = ?2 AND args = ?3
                 ORDER BY id LIMIT 1",
                    params![dir, request.task, args],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(match parked {
                Some(id) => {
                    tx.execute(
                        "UPDATE jobs SET state = 'waiting', weight = ?2, command = ?3,
                         owner_pid = ?4, owner_start = ?5, parked_at = NULL,
                         capacity = ?6, measure_only = ?7, checkout = ?8, uses = ?9
                     WHERE id = ?1",
                        params![
                            id,
                            request.weight,
                            request.command,
                            owner.pid,
                            owner.start_time as i64,
                            request.capacity,
                            request.measure_only,
                            checkout,
                            uses
                        ],
                    )?;
                    (id, true)
                }
                None => {
                    tx.execute(
                        "INSERT INTO jobs (task, weight, state, dir, args, command,
                                       owner_pid, owner_start, enqueued_at,
                                       capacity, measure_only, checkout, uses)
                     VALUES (?1, ?2, 'waiting', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                        params![
                            request.task,
                            request.weight,
                            dir,
                            args,
                            request.command,
                            owner.pid,
                            owner.start_time as i64,
                            unix_ms(SystemTime::now()),
                            request.capacity,
                            request.measure_only,
                            checkout,
                            uses
                        ],
                    )?;
                    (tx.last_insert_rowid(), false)
                }
            })
        })?;
        Ok(Ticket {
            queue: self,
            id,
            resumed,
            armed: true,
        })
    }

    /// Reaps dead jobs and returns everything left.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let (jobs, reaped) = self.write(|tx| load_jobs(tx))?;
        Ok(Snapshot { jobs, reaped })
    }

    /// Removes jobs nobody holds any more and returns them.
    pub fn reap(&self) -> Result<Vec<Job>> {
        Ok(self.write(|_| Ok(()))?.1)
    }

    /// Job `id`, if it is running and still alive, i.e. a valid holder for
    /// nested `loadmaster run` calls and `loadmaster held`.
    pub fn holder(&self, id: JobId) -> Result<Option<Job>> {
        Ok(self
            .snapshot()?
            .jobs
            .into_iter()
            .find(|job| job.id == id && job.state == JobState::Running))
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
        self.write(|tx| Ok(tx.execute("DELETE FROM jobs WHERE id = ?1", [id])?))?;
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
        let (admission, _) = self.queue.write(|tx| {
            let jobs = load_jobs(tx)?;
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
            Ok(admission)
        })?;
        Ok(admission)
    }

    /// Starts the job at once, whatever its place in line and the capacity:
    /// measure-only mode.
    pub fn admit_now(&self) -> Result<()> {
        let (started, _) = self.queue.write(|tx| {
            Ok(tx.execute(
                "UPDATE jobs SET state = 'running', started_at = ?2
                 WHERE id = ?1 AND state = 'waiting'",
                params![self.id, unix_ms(SystemTime::now())],
            )?)
        })?;
        if started == 0 {
            return Err(Error::JobVanished(self.id));
        }
        Ok(())
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

/// Switches the database to WAL mode, retrying while it is busy.
///
/// The switch needs an exclusive lock that SQLite tries to take once, without
/// the busy handler, so several processes opening a brand-new database at the
/// same time can get "database is locked" despite the busy timeout.
fn enable_wal(conn: &Connection) -> rusqlite::Result<()> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0)) {
            Ok(_) => return Ok(()),
            Err(e)
                if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
}

/// A dimension of the admission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Dim<'a> {
    Cpu,
    Resource(&'a str),
    PerCheckout(&'a str, &'a Path),
}

/// What `job` needs on each dimension, and the limit it got in line with.
/// Amounts above a limit count as the whole limit. `capacity` stands in for
/// a CPU capacity the job did not record.
fn demands(job: &Job, capacity: u32) -> Vec<(Dim<'_>, u64, u64)> {
    let cpu = job.capacity.unwrap_or(capacity);
    let mut demands = vec![(Dim::Cpu, u64::from(job.weight.min(cpu)), u64::from(cpu))];
    for u in &job.uses {
        if let Some(limit) = u.capacity {
            let amount = u64::from(u.amount.min(limit));
            demands.push((Dim::Resource(&u.name), amount, u64::from(limit)));
        }
        if let (Some(limit), Some(checkout)) = (u.per_checkout, &job.checkout) {
            let amount = u64::from(u.amount.min(limit));
            demands.push((
                Dim::PerCheckout(&u.name, checkout),
                amount,
                u64::from(limit),
            ));
        }
    }
    demands
}

/// The admission rule (see the module docs): whether job `id` may start.
/// `capacity` is this process's CPU capacity, for jobs that did not record
/// theirs.
fn decide(jobs: &[Job], id: JobId, capacity: u32) -> Result<Admission> {
    let me = jobs
        .iter()
        .find(|job| job.id == id)
        .ok_or(Error::JobVanished(id))?;
    if me.state == JobState::Running {
        return Ok(Admission::Admitted);
    }
    // Sums of u32 amounts cannot overflow u64.
    let mut used: BTreeMap<Dim, u64> = BTreeMap::new();
    for job in jobs.iter().filter(|job| job.state == JobState::Running) {
        for (dim, amount, _) in demands(job, capacity) {
            *used.entry(dim).or_default() += amount;
        }
    }
    // For the jobs ahead: what they set aside and the smallest limit among
    // them (processes may disagree on a limit), and what they are short of.
    let mut set_aside: BTreeMap<Dim, (u64, u64)> = BTreeMap::new();
    let mut short: BTreeSet<Dim> = BTreeSet::new();
    let mut ahead = 0u32;
    for job in jobs.iter().filter(|job| job.state != JobState::Running) {
        let demands = demands(job, capacity);
        let fits: Vec<bool> = demands
            .iter()
            .map(|&(dim, amount, limit)| {
                let (reserved, reserved_limit) = set_aside.get(&dim).copied().unwrap_or((0, limit));
                !short.contains(&dim)
                    && used.get(&dim).copied().unwrap_or(0) + reserved + amount
                        <= limit.min(reserved_limit)
            })
            .collect();
        let fits_all = fits.iter().all(|&fits| fits);
        if job.id == id {
            if fits_all {
                return Ok(Admission::Admitted);
            }
            let mut waiting_for: Vec<String> = demands
                .iter()
                .zip(&fits)
                .filter_map(|(&(dim, ..), &fits)| match dim {
                    Dim::Resource(name) | Dim::PerCheckout(name, _) if !fits => {
                        Some(name.to_owned())
                    }
                    _ => None,
                })
                .collect();
            waiting_for.dedup();
            return Ok(Admission::Waiting {
                ahead,
                used: used.get(&Dim::Cpu).copied().unwrap_or(0),
                waiting_for,
            });
        }
        ahead = ahead.saturating_add(1);
        for (&(dim, amount, limit), &fits) in demands.iter().zip(&fits) {
            // A job that fits is about to start: it sets aside everything.
            if fits_all || fits {
                let entry = set_aside.entry(dim).or_insert((0, limit));
                *entry = (entry.0 + amount, entry.1.min(limit));
            } else {
                short.insert(dim);
            }
        }
    }
    Err(Error::JobVanished(id))
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
                child_pgid, child_start, enqueued_at, parked_at, started_at,
                capacity, measure_only, checkout, uses
         FROM jobs ORDER BY id",
    )?;
    let jobs = stmt
        .query_map([], job_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(jobs)
}

fn job_from_row(row: &Row) -> rusqlite::Result<Job> {
    fn json<T: serde::de::DeserializeOwned>(idx: usize, text: String) -> rusqlite::Result<T> {
        serde_json::from_str(&text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, e.into())
        })
    }
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
        args: json(5, args)?,
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
        capacity: row.get(14)?,
        measure_only: row.get(15)?,
        checkout: row
            .get::<_, Option<Vec<u8>>>(16)?
            .map(|bytes| PathBuf::from(OsString::from_vec(bytes))),
        uses: json(17, row.get(17)?)?,
    })
}

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn from_unix_ms(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

#[cfg(test)]
mod tests;
