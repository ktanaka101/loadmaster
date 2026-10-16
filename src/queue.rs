//! The machine-wide job queue, kept in SQLite.
//!
//! **Admission rule** (see the `admission` module). Jobs are judged in line
//! order, on every dimension at once: CPU (the weight), each machine-wide
//! resource and each resource in each checkout. A job decides from the jobs
//! ahead of it alone (and measure-only jobs anywhere, which started without
//! the rule), so no job is ever delayed by jobs that got in line after it. It
//! starts if it fits now and will still fit beside every job ahead of it when
//! that one starts, so that the jobs that start never take the machine past
//! its limits, even when CPU busy outside the queue lowers a CPU limit (see
//! [`Outside`]). The first job in line waits only for room, so jobs cannot
//! starve.
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

use std::cell::RefCell;
use std::ffi::OsString;
use std::fmt;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::history::{self, Outcome, Record};
use crate::process::{self, ProcessGroup, ProcessId};

pub type JobId = i64;

/// File name of the queue database inside the state directory. The number
/// is the version of its layout and of the admission rule, the protocol
/// between the processes that share it: a loadmaster that changes either
/// uses a new file, never one another version made (but to tell of the jobs
/// that run there, see [`jobs_in_other_queues`]).
pub const QUEUE_DB_FILE: &str = "queue-v4.db";

/// How long to wait for another process's lock on the database.
const BUSY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a parked job keeps its place in line without an owner.
pub const PARK_TTL: Duration = Duration::from_secs(120);

/// How long a measurement of the CPU busy outside the queue stands before
/// it is measured again.
const CPU_FRESH: Duration = Duration::from_secs(3);
/// How long the CPU is measured over.
const CPU_SAMPLE: Duration = Duration::from_millis(250);
/// A measurement this recent is averaged with the next, so that a moment's
/// burst of other work does not decide what a job starts with.
const CPU_SMOOTH: Duration = Duration::from_secs(10);

// AUTOINCREMENT: IDs are never reused, so a stale LOADMASTER_HELD can never
// point at an unrelated newer job.
//
// Every layout keeps `jobs.task`, `state` (with 'running' for a job that
// holds its slot), `owner_pid` and `owner_start` (its loadmaster), and
// `child_pgid` and `child_start` (its task's process group), as they are
// here: loadmaster reads them from the queues of other layouts, to tell of
// the jobs that run there (see [`jobs_in_other_queues`]).
//
// `cancel_requests` holds the requests from `loadmaster cancel`, for the
// job's own loadmaster to act on: in an agent's sandbox, a process may not
// signal one started by another command. One row per job, kept until the job
// is gone, with the strongest stop asked for and the strongest one done, by
// the job's loadmaster and by a `cancel` that signalled the task of a job
// whose loadmaster is gone (0 the task's stop signal, 1 SIGKILL, -1 none):
// a later cancel that asks for no more than was done does nothing.
//
// `cpu` holds the last measurement of the cores busy outside the queue (see
// [`Queue::outside`]), so that the processes that wait take turns measuring.
//
// `PRAGMA user_version` is the layout: 1 is the one above (0.12 and 0.13),
// 2 adds what [`LAYOUT_2`] does. A database of layout 1 is brought up to 2 in
// place; the queue file itself never changes (see `docs/DECISIONS.md`, "The
// queue file").
const SCHEMA: &str = "
CREATE TABLE jobs (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    task         TEXT    NOT NULL,
    weight       INTEGER NOT NULL CHECK (weight > 0),
    state        TEXT    NOT NULL CHECK (state IN ('waiting', 'parked', 'running')),
    dir          BLOB    NOT NULL,
    args         TEXT    NOT NULL,
    command      TEXT    NOT NULL,
    owner_pid    INTEGER,
    owner_start  INTEGER,
    child_pgid   INTEGER,
    child_start  INTEGER,
    enqueued_at  INTEGER NOT NULL,
    parked_at    INTEGER,
    started_at   INTEGER,
    capacity     INTEGER NOT NULL,
    measure_only INTEGER NOT NULL,
    checkout     BLOB    NOT NULL,
    uses         TEXT    NOT NULL,
    adapt_cpu    INTEGER NOT NULL,
    stop_signal  INTEGER NOT NULL,
    granted      INTEGER
);
CREATE TABLE cancel_requests (
    job          INTEGER PRIMARY KEY,
    kill         INTEGER NOT NULL,
    owner_done   INTEGER NOT NULL DEFAULT -1,
    signal_done  INTEGER NOT NULL DEFAULT -1
);
CREATE TABLE cpu (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    at           INTEGER NOT NULL,
    cores        INTEGER NOT NULL,
    outside      REAL
);
PRAGMA user_version = 1;
";

/// From layout 1 to 2: when each job entered its state (`state_since`, Unix
/// milliseconds, null where that is not known), kept by triggers so that the
/// writers of 0.12 and 0.13, which share the file, keep it too; and the
/// admission protocol the queue's writers keep (see [`PROTOCOL`]).
///
/// A job enters its state when it gets in line, parks, resumes, starts, or
/// is handed over to its task's process group (its loadmaster gives it up).
/// Of jobs already there, a running job entered it when it started, a parked
/// one when it parked; when a waiting one did (it may have resumed) is not
/// known, nor when a handed-over one was.
const LAYOUT_2: &str = "
ALTER TABLE jobs ADD COLUMN state_since INTEGER;
UPDATE jobs SET state_since = CASE
    WHEN state = 'running' AND owner_pid IS NOT NULL THEN started_at
    WHEN state = 'parked' THEN parked_at
END;
CREATE TRIGGER jobs_enter_state_on_insert AFTER INSERT ON jobs
BEGIN
    UPDATE jobs SET state_since = CAST(ROUND((julianday('now') - 2440587.5) * 86400000) AS INTEGER)
    WHERE id = NEW.id;
END;
CREATE TRIGGER jobs_enter_state AFTER UPDATE OF state, owner_pid ON jobs
WHEN NEW.state IS NOT OLD.state OR (NEW.owner_pid IS NULL AND OLD.owner_pid IS NOT NULL)
BEGIN
    UPDATE jobs SET state_since = CAST(ROUND((julianday('now') - 2440587.5) * 86400000) AS INTEGER)
    WHERE id = NEW.id;
END;
CREATE TABLE protocol (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    version      INTEGER NOT NULL
);
INSERT INTO protocol (id, version) VALUES (1, 1);
PRAGMA user_version = 2;
";

/// Closes a queue of protocol 1 to new jobs, for its move to protocol 2, once
/// a run of this version asks for a place in it (see [`Queue::enqueue`]): a
/// trigger, which the writers of 0.12 to 0.14 obey too, though they do not
/// read the protocol. They finish the jobs they hold; once none is left the
/// queue moves (see [`LAYOUT_3`]).
const CLOSE_FOR_PROTOCOL_2: &str = "
CREATE TRIGGER queue_closed BEFORE INSERT ON jobs
BEGIN
    SELECT RAISE(ABORT, 'the queue is moving to the admission protocol of loadmaster 0.15 and later: give every agent on this machine loadmaster 0.15 or later');
END;
";

/// From layout 2 to 3, and from protocol 1 to 2, on an empty queue: each job
/// records the least cores it may start with (`min_weight`), and nothing of
/// 0.12 to 0.14 can use the queue any more. Their statements name `jobs.id`,
/// which becomes `job_id` (this version's name the row by `rowid`, which is
/// the same in both), except their insert, which a trigger refuses for want
/// of a `min_weight`.
const LAYOUT_3: &str = "
DROP TRIGGER IF EXISTS queue_closed;
ALTER TABLE jobs RENAME COLUMN id TO job_id;
ALTER TABLE jobs ADD COLUMN min_weight INTEGER;
CREATE TRIGGER jobs_of_protocol_2 BEFORE INSERT ON jobs WHEN NEW.min_weight IS NULL
BEGIN
    SELECT RAISE(ABORT, 'this queue keeps the admission protocol of loadmaster 0.15 and later: give every agent on this machine loadmaster 0.15 or later');
END;
UPDATE protocol SET version = 2;
PRAGMA user_version = 3;
";

/// The layout of the database this version reads and writes (see
/// [`SCHEMA`]).
const LAYOUT: i64 = 3;

/// The admission protocol every writer of this queue keeps: what a job may
/// start with and what it must fit beside (see `docs/DECISIONS.md`, "The
/// queue file"). 1 is that of 0.12 to 0.14; 2, from 0.15 on, lets a job start
/// with fewer cores than its weight wherever that fits, which protocol 1
/// does not protect. Every write transaction checks it first: a queue a later
/// loadmaster moved to another protocol is an error, never another file.
pub const PROTOCOL: i64 = 2;

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
    /// When its run asked for a place (see [`JobRequest::asked_at`]).
    pub enqueued_at: SystemTime,
    pub parked_at: Option<SystemTime>,
    pub started_at: Option<SystemTime>,
    /// The machine's capacity when the job got in line.
    pub capacity: u32,
    pub measure_only: bool,
    /// Resources the job uses, with the limits it got in line with.
    pub uses: Vec<Use>,
    /// The resolved directory of the `.loadmaster.toml` the job was started
    /// from.
    pub checkout: PathBuf,
    /// Whether it starts with fewer cores while CPU is busy outside the
    /// queue (see [`Outside`]): the machine setting it got in line with.
    pub adapt_cpu: bool,
    /// The signal that asks its task to stop, for a cancel that signals the
    /// task itself.
    pub stop_signal: i32,
    /// The cores it started with: its weight, or fewer while CPU was busy
    /// outside the queue. `None` until it starts.
    pub granted: Option<u32>,
    /// When it entered its state, if that is known (see [`LAYOUT_2`]).
    pub state_since: Option<SystemTime>,
    /// The least cores it may start with (see [`LAYOUT_3`]); `None` for a
    /// job of 0.12 to 0.14, in a queue that has not moved yet.
    pub min_weight: Option<u32>,
}

/// The CPU busy outside the queue, in cores: all the machine's busy CPU time
/// but what the running jobs were granted. A job that uses more than that
/// counts too: as far as the line goes, those cores are busy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outside {
    pub busy: f64,
    /// The machine's CPUs.
    pub cores: u32,
}

impl Outside {
    /// The most cores a job that got in line with `capacity` may start
    /// with: the capacity, less the CPU busy outside the queue beyond the
    /// cores the capacity leaves free, and at least one. A weight above it
    /// counts as all of it, as a weight above the capacity counts as the
    /// capacity, so the job at the head of the line still starts once the
    /// queue's own jobs ahead of it are done.
    pub fn cpu_limit(&self, capacity: u32) -> u32 {
        let spare = f64::from(self.cores.saturating_sub(capacity));
        // NaN takes nothing.
        let taken = (self.busy - spare).max(0.0);
        // A float above u32::MAX saturates.
        ((f64::from(capacity) - taken).round().max(1.0) as u32).min(capacity)
    }

    fn lowered(&self) -> OutsideBusy {
        OutsideBusy {
            busy: self.busy.round() as u64,
            cores: u64::from(self.cores),
        }
    }
}

/// CPU busy outside the queue that lowered a job's CPU limit, rounded, for
/// explanations: `busy` of the machine's `cores`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutsideBusy {
    pub busy: u64,
    pub cores: u64,
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
    /// The most cores it may start with: its weight, which above its
    /// capacity counts as all of it.
    pub fn most(&self) -> u32 {
        self.weight.min(self.capacity).max(1)
    }

    /// The least cores it may start with: its `min_weight`; for a job of
    /// 0.12 to 0.14, one if it adapts to CPU outside the queue, else its
    /// weight. Like the weight, capped at its capacity.
    pub fn least(&self) -> u32 {
        let weight = self.most();
        self.min_weight
            .unwrap_or(if self.adapt_cpu { 1 } else { weight })
            .clamp(1, weight)
    }

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
    /// The least cores it may start with: its `min_weight`, else its weight.
    pub min_weight: u32,
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
    /// The machine setting `adapt_to_outside_cpu`.
    pub adapt_cpu: bool,
    /// The task's stop signal.
    pub stop_signal: i32,
    /// When its run asked for a place. Waiting to get one, for the lock or
    /// while the queue moves, is part of its wait in every record of it.
    pub asked_at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Admitted,
    Waiting(Wait),
}

/// Why a job waits, as the admission rule decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    /// Jobs ahead in line that have not started.
    pub ahead: u32,
    /// What it is short of, and why. Never empty.
    pub short: Vec<Shortage>,
}

/// Something a waiting job is short of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortage {
    pub need: Need,
    /// How much of it the job needs.
    pub amount: u64,
    pub cause: Cause,
}

/// A dimension of the admission check, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    Cores,
    Resource(String),
    /// A resource's per-checkout limit, in the job's checkout.
    CheckoutResource(String),
}

/// Why a job cannot have what it needs yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// Not enough now: the jobs ahead that run or are about to start, listed
    /// in `by`, use `used` of `limit`. `limit_by` is the job ahead whose
    /// record set a limit smaller than this job's own (processes can disagree
    /// on one). For cores, `outside` is the CPU busy outside the queue if
    /// that lowered the limit.
    InUse {
        used: u64,
        limit: u64,
        by: Vec<JobRef>,
        limit_by: Option<JobRef>,
        outside: Option<OutsideBusy>,
    },
    /// It would not fit beside `start`, a job ahead that has not started,
    /// when that one starts: with it and what other jobs ahead could use
    /// beside it, `used` of `limit` could be in use then. `beside` names such
    /// a set of jobs when one was found, and is `None` when the amount is
    /// only a bound.
    BesideStart {
        start: JobRef,
        used: u64,
        limit: u64,
        beside: Option<Vec<JobRef>>,
        limit_by: Option<JobRef>,
    },
}

/// A job named in an explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRef {
    pub id: JobId,
    pub task: String,
}

impl JobRef {
    pub(crate) fn of(job: &Job) -> JobRef {
        JobRef {
            id: job.id,
            task: job.task.clone(),
        }
    }
}

impl fmt::Display for JobRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{} `{}`", self.id, self.task)
    }
}

impl Wait {
    /// What the job waits for and why, e.g. "`db` (1 of 1 in use by #41
    /// `ui`)".
    pub fn waits_for(&self) -> String {
        join_and(self.short.iter().map(Shortage::to_string).collect())
    }
}

impl fmt::Display for Shortage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = matches!(self.cause, Cause::InUse { limit, outside: None, .. }
            if self.need == Need::Cores && self.amount == limit && limit > 1);
        write!(f, "{}", quantity(&self.need, self.amount, whole))?;
        let limit_by = |f: &mut fmt::Formatter<'_>, by: &Option<JobRef>| match by {
            Some(job) => write!(f, "; the limit is the one {job} got in line with"),
            None => Ok(()),
        };
        match &self.cause {
            Cause::InUse {
                used,
                limit,
                by,
                limit_by: smaller,
                outside,
            } => {
                if whole {
                    write!(f, " ({used} in use")?;
                } else {
                    write!(f, " ({used} of {limit} in use")?;
                }
                // Every running job uses cores: naming them says nothing.
                if self.need != Need::Cores && !by.is_empty() {
                    write!(f, " by {}", list_jobs(by))?;
                }
                limit_by(f, smaller)?;
                if let Some(OutsideBusy { busy, cores }) = outside {
                    write!(
                        f,
                        "; {busy} of {cores} cores are busy beyond what running jobs got"
                    )?;
                }
                write!(f, ")")
            }
            Cause::BesideStart {
                start,
                used,
                limit,
                beside,
                limit_by: smaller,
            } => {
                write!(f, " ({used} of {limit} could be in use when {start} starts")?;
                match beside {
                    Some(jobs) if !jobs.is_empty() => write!(f, ", beside {}", list_jobs(jobs))?,
                    Some(_) => {}
                    None => write!(f, ", with jobs ahead that could run beside it")?,
                }
                limit_by(f, smaller)?;
                write!(f, ")")
            }
        }
    }
}

/// "3 cores", "all 12 cores", "`db`", "2 of `mem`", "`db` in its checkout".
fn quantity(need: &Need, amount: u64, whole: bool) -> String {
    let of = |name: &str| match amount {
        1 => format!("`{name}`"),
        n => format!("{n} of `{name}`"),
    };
    match need {
        Need::Cores if whole => format!("all {amount} cores"),
        Need::Cores if amount == 1 => "1 core".to_owned(),
        Need::Cores => format!("{amount} cores"),
        Need::Resource(name) => of(name),
        Need::CheckoutResource(name) => format!("{} in its checkout", of(name)),
    }
}

/// "#41 `ui`, #42 `lint` and 3 more".
fn list_jobs(jobs: &[JobRef]) -> String {
    const SHOWN: usize = 3;
    let mut parts: Vec<String> = jobs.iter().take(SHOWN).map(JobRef::to_string).collect();
    if jobs.len() > SHOWN {
        parts.push(format!("{} more", jobs.len() - SHOWN));
    }
    join_and(parts)
}

/// "a", "a and b", "a, b and c".
fn join_and(mut parts: Vec<String>) -> String {
    match parts.pop() {
        None => String::new(),
        Some(last) if parts.is_empty() => last,
        Some(last) => format!("{} and {last}", parts.join(", ")),
    }
}

/// The queue at one point in time.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// All jobs, in line order.
    pub jobs: Vec<Job>,
    /// Jobs reaped while taking this snapshot.
    pub reaped: Vec<Job>,
    /// The admission protocol of the queue: 1 while it holds the jobs of
    /// 0.12 to 0.14, which their own loadmaster admits by its rule, else
    /// [`PROTOCOL`].
    pub protocol: i64,
    /// How many jobs of 0.12 to 0.14 were left, if a run of this version had
    /// started to move the queue to its protocol (see
    /// [`CLOSE_FOR_PROTOCOL_2`]).
    pub moving: Option<usize>,
}

impl Snapshot {
    /// Cores held by running jobs: what each started with. A sum of `u32`
    /// amounts cannot overflow `u64`.
    pub fn used(&self) -> u64 {
        self.jobs
            .iter()
            .filter(|j| j.state == JobState::Running)
            .map(|j| u64::from(cpu_demand(j, None).0))
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
        queue.create_schema()?;
        Ok(queue)
    }

    /// Whether SQLite could only open the database for reading, as it does
    /// without telling when it may not write to it.
    pub fn is_read_only(&self) -> bool {
        self.conn.is_readonly(rusqlite::MAIN_DB).unwrap_or(false)
    }

    fn create_schema(&self) -> Result<()> {
        // Usually it is there; don't take the write lock then. One this
        // process may not write is for the caller to report. A queue of
        // protocol 1 is opened as it is: only getting in line moves it (see
        // [`Queue::enqueue`]), so that looking at it with this version never
        // stops 0.12 to 0.14.
        if self.is_read_only() || matches!(layout(&self.conn)?, 2 | LAYOUT) {
            return Ok(());
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // Again under the lock: another process may have made it since.
        match layout(&tx)? {
            0 => tx.execute_batch(&format!("{SCHEMA}{LAYOUT_2}{LAYOUT_3}"))?,
            // Brought up to date in place, as 0.14 does; 0.12 and 0.13 go on.
            1 => tx.execute_batch(LAYOUT_2)?,
            2 | LAYOUT => {}
            _ => return Err(self.unknown_layout(&tx)),
        }
        tx.commit()?;
        Ok(())
    }

    /// The error for a queue of a layout or protocol this version does not
    /// know: a later loadmaster changed it.
    fn unknown_layout(&self, conn: &Connection) -> Error {
        Error::QueueTooNew {
            path: self.path.clone(),
            layout: layout(conn).unwrap_or(-1),
            protocol: protocol(conn).ok().flatten(),
        }
    }

    /// What changes whenever the database does: `PRAGMA data_version`
    /// counts the commits of other connections, `total_changes` the rows
    /// this one changed.
    fn version(&self) -> Result<(i64, u64)> {
        let data: i64 = self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?;
        Ok((data, self.conn.total_changes()))
    }

    /// A write transaction, once the queue is checked to be of this
    /// version's layout and protocol: before anything is reaped or changed,
    /// so that a process that opened it before a later loadmaster changed it
    /// writes nothing.
    /// A queue of protocol 1 counts too: the writers of 0.12 to 0.14 admit
    /// their jobs in it, and this version reaps, shows and cancels them, but
    /// gets none of its own in line until it has moved it.
    fn begin(&self) -> Result<Transaction<'_>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        match (layout(&tx)?, protocol(&tx)?) {
            (LAYOUT, Some(PROTOCOL)) | (2, Some(1)) => {}
            _ => return Err(self.unknown_layout(&tx)),
        }
        Ok(tx)
    }

    /// Runs `f` in a write transaction, after reaping dead jobs, and once a
    /// queue closed for the move to protocol 2 is empty, after moving it:
    /// whichever write finds it so ends a move that a run started. Once that
    /// is committed, logs the reaped jobs that were lost, and returns them
    /// all.
    fn write<T>(&self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<(T, Vec<Job>)> {
        let tx = self.begin()?;
        let reaped = reap(&tx)?;
        if layout(&tx)? != LAYOUT && closed(&tx)? {
            move_or_close(&tx)?;
        }
        let value = f(&tx)?;
        tx.commit()?;
        self.log_lost(&reaped);
        Ok((value, reaped))
    }

    fn log_lost(&self, reaped: &[Job]) {
        let now = SystemTime::now();
        for job in reaped.iter().filter(|job| job.was_lost()) {
            // A task that never started waited until it ended, here.
            let waited_until = match job.child {
                Some(_) => job.started_at.unwrap_or(now),
                None => now,
            };
            self.log(&Record {
                // The task is let through right after its group is
                // recorded; a job admitted but killed before that never ran.
                started: Some(job.child.is_some()),
                ..record(job, Outcome::Lost, waited_until)
            });
        }
    }

    fn log(&self, record: &Record) {
        let Some(state_dir) = self.path.parent() else {
            return;
        };
        if let Err(e) = history::append(state_dir, record) {
            let _ = writeln!(
                std::io::stderr(),
                "loadmaster: warning: cannot write the history log: {e}"
            );
        }
    }

    /// Stops job `id` for `loadmaster cancel` (see [`Queue::stop`]) and logs
    /// a `canceled` record, so the report can leave the check out.
    pub fn cancel(&self, id: JobId, kill: bool) -> Result<Canceled> {
        let canceled = self.stop(id, kill)?;
        if let Canceled::DroppedPlace(job) | Canceled::Requested(job) | Canceled::Signaled(job) =
            &canceled
        {
            let now = SystemTime::now();
            self.log(&Record {
                run_ms: job
                    .started_at
                    .map(|at| millis(now.duration_since(at).unwrap_or_default())),
                ..record(job, Outcome::Canceled, job.started_at.unwrap_or(now))
            });
        }
        Ok(canceled)
    }

    /// Stops job `id`, without logging. A job with a live loadmaster gets a
    /// request, which that loadmaster acts on (see
    /// [`Ticket::cancel_requested`]): a waiting job leaves the line and a
    /// running one's task gets SIGTERM, or SIGKILL with `kill`. That needs
    /// no permission to signal it, which an agent's sandbox may not give. A
    /// parked job loses its place. A task whose loadmaster is gone is
    /// signalled from here (see [`signal_task`]). Also for a canceled job
    /// whose state has changed since.
    pub fn stop(&self, id: JobId, kill: bool) -> Result<Canceled> {
        let (canceled, _) = self.write(|tx| {
            let Some(job) = load_jobs(tx)?.into_iter().find(|job| job.id == id) else {
                return Ok(Canceled::NotFound);
            };
            if job.state == JobState::Parked {
                delete_job(tx, job.id)?;
                return Ok(Canceled::DroppedPlace(job));
            }
            tx.execute(
                "INSERT INTO cancel_requests (job, kill) VALUES (?1, ?2)
                 ON CONFLICT (job) DO UPDATE SET kill = max(kill, excluded.kill)",
                params![job.id, kill],
            )?;
            if job.owner_alive() {
                return Ok(Canceled::Requested(job));
            }
            signal_task(tx, job)
        })?;
        Ok(canceled)
    }

    /// Gets in line, or takes over a parked job with the same
    /// (`dir`, `task`, `args`) and with it that job's place.
    pub fn enqueue(&self, request: &JobRequest, owner: ProcessId) -> Result<Ticket<'_>> {
        // Raw bytes: a lossy string could make two directories look equal.
        let dir = request.dir.as_os_str().as_bytes();
        let checkout = request.checkout.as_os_str().as_bytes();
        let args = serde_json::to_string(request.args).expect("strings always serialize");
        let uses = serde_json::to_string(request.uses).expect("uses always serialize");
        let (got, _) = self.write(|tx| {
            // Getting in line is what moves a queue of protocol 1 to this
            // version's: at once if it is empty, else it is closed to new
            // jobs, and takes none until it has moved. An answer, not a
            // failure: what the write reaped stays reaped.
            if layout(tx)? != LAYOUT {
                move_or_close(tx)?;
            }
            if let Some(left) = moving(tx)? {
                return Ok(Err(left));
            }
            let parked: Option<JobId> = tx
                .query_row(
                    // Only with the same demands: jobs behind may have
                    // started next to what the parked job set aside, and
                    // could delay it if it needed more now.
                    "SELECT rowid FROM jobs
                 WHERE state = 'parked' AND dir = ?1 AND task = ?2 AND args = ?3
                   AND weight = ?4 AND capacity = ?5 AND uses = ?6 AND checkout = ?7
                   AND adapt_cpu = ?8 AND min_weight = ?9
                 ORDER BY rowid LIMIT 1",
                    params![
                        dir,
                        request.task,
                        args,
                        request.weight,
                        request.capacity,
                        uses,
                        checkout,
                        request.adapt_cpu,
                        request.min_weight
                    ],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(Ok(match parked {
                Some(id) => {
                    tx.execute(
                        "UPDATE jobs SET state = 'waiting', weight = ?2, command = ?3,
                         owner_pid = ?4, owner_start = ?5, parked_at = NULL,
                         capacity = ?6, measure_only = ?7, checkout = ?8, uses = ?9,
                         stop_signal = ?10
                     WHERE rowid = ?1",
                        params![
                            id,
                            request.weight,
                            request.command,
                            owner.pid,
                            owner.start_time as i64,
                            request.capacity,
                            request.measure_only,
                            checkout,
                            uses,
                            request.stop_signal
                        ],
                    )?;
                    (id, true)
                }
                None => {
                    tx.execute(
                        "INSERT INTO jobs (task, weight, state, dir, args, command,
                                       owner_pid, owner_start, enqueued_at,
                                       capacity, measure_only, checkout, uses, adapt_cpu,
                                       stop_signal, min_weight)
                     VALUES (?1, ?2, 'waiting', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                             ?14, ?15)",
                        params![
                            request.task,
                            request.weight,
                            dir,
                            args,
                            request.command,
                            owner.pid,
                            owner.start_time as i64,
                            unix_ms(request.asked_at),
                            request.capacity,
                            request.measure_only,
                            checkout,
                            uses,
                            request.adapt_cpu,
                            request.stop_signal,
                            request.min_weight
                        ],
                    )?;
                    (tx.last_insert_rowid(), false)
                }
            }))
        })?;
        let (id, resumed) = got.map_err(|left| Error::QueueMoving { left })?;
        Ok(Ticket {
            queue: self,
            id,
            resumed,
            armed: true,
            last: RefCell::new(None),
        })
    }

    /// The CPU busy outside the queue (see [`Outside`]), as last measured, or
    /// measured again over [`CPU_SAMPLE`] if that is older than
    /// [`CPU_FRESH`]; `jobs` are the queue's jobs now. `None` if the OS does
    /// not report CPU time.
    pub fn outside(&self, jobs: &[Job]) -> Result<Option<Outside>> {
        // A measurement from the future (the clock went back) is as good as
        // none.
        let fresh = |cpu: &CpuRow| {
            SystemTime::now()
                .duration_since(cpu.at)
                .is_ok_and(|age| age < CPU_FRESH)
        };
        let last = read_cpu(&self.conn)?;
        if let Some(cpu) = last.as_ref().filter(|cpu| fresh(cpu)) {
            return Ok(cpu.outside());
        }
        let Some(cores) = process::online_cpus().and_then(|n| u32::try_from(n).ok()) else {
            return Ok(None);
        };
        // Over a short time of its own, not since the last measurement: a job
        // that started and ended in between would count as outside.
        let Some(from) = process::cpu_ticks() else {
            return Ok(None);
        };
        std::thread::sleep(CPU_SAMPLE);
        let Some(to) = process::cpu_ticks() else {
            return Ok(None);
        };
        // The jobs' own CPU is what they were granted: a job that uses more
        // counts as outside, which those cores are, as far as the line goes.
        // Before or after the measurement, whichever is more: a job that ended
        // meanwhile counts as the queue's.
        let in_queue = |jobs: &[Job]| -> u64 {
            jobs.iter()
                .filter(|job| job.state == JobState::Running)
                .map(|job| u64::from(cpu_demand(job, None).0))
                .sum()
        };
        let in_queue = in_queue(jobs).max(in_queue(&load_jobs(&self.conn)?));
        let now = SystemTime::now();
        let busy = to
            .busy_since(from)
            .map(|share| (share * f64::from(cores) - in_queue as f64).clamp(0.0, f64::from(cores)));
        let last = last.and_then(|cpu| Some((cpu.outside?, now.duration_since(cpu.at).ok()?)));
        let busy = smoothed(busy, last);
        let at = unix_ms(now);
        // Another process may have stored a later measurement meanwhile; that
        // one stands.
        let tx = self.begin()?;
        tx.execute(
            "INSERT INTO cpu (id, at, cores, outside) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT (id) DO UPDATE SET at = ?1, cores = ?2, outside = ?3
             WHERE cpu.at < ?1",
            params![at, cores, busy],
        )?;
        tx.commit()?;
        Ok(busy.map(|busy| Outside { busy, cores }))
    }

    /// Reaps dead jobs and returns everything left.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let ((jobs, protocol, left), reaped) = self.write(|tx| {
            Ok((
                load_jobs(tx)?,
                protocol(tx)?.unwrap_or(PROTOCOL),
                moving(tx)?,
            ))
        })?;
        Ok(Snapshot {
            jobs,
            reaped,
            protocol,
            moving: left,
        })
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
            "UPDATE jobs SET child_pgid = ?2, child_start = ?3 WHERE rowid = ?1",
            params![id, child.pgid, child.leader_start_time.map(|t| t as i64)],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn remove(&self, id: JobId) -> Result<()> {
        self.write(|tx| delete_job(tx, id))?;
        Ok(())
    }
}

/// What [`Queue::cancel`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Canceled {
    /// The job is not in the queue (any more).
    NotFound,
    /// It was parked: its kept place is gone.
    DroppedPlace(Job),
    /// Its loadmaster was asked to stop it, and the job leaves the queue
    /// once that has taken effect.
    Requested(Job),
    /// Its loadmaster is gone, so its task was signalled directly.
    Signaled(Job),
}

/// Signals the task of `job`, whose loadmaster is gone, for its request:
/// SIGKILL as often as it is asked, its stop signal only if it has had none,
/// from its loadmaster (which records what it sent before it hands the slot
/// over) or from an earlier cancel. Within the write transaction `tx`, so
/// that the decision, the signal and its record are one step. If the signal
/// is not allowed, the transaction is rolled back with the error, so nothing
/// is recorded as sent.
fn signal_task(tx: &Transaction, job: Job) -> Result<Canceled> {
    let row: Option<(bool, i64, i64)> = tx
        .query_row(
            "SELECT kill, owner_done, signal_done FROM cancel_requests WHERE job = ?1",
            [job.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((kill, owner_done, signal_done)) = row else {
        return Ok(Canceled::NotFound);
    };
    if !kill && owner_done.max(signal_done) >= 0 {
        return Ok(Canceled::Requested(job));
    }
    let Some(child) = job.child.filter(|child| child.is_alive()) else {
        // Gone in the meantime; the next look reaps it.
        return Ok(Canceled::Requested(job));
    };
    match child.signal(if kill { libc::SIGKILL } else { job.stop_signal }) {
        Ok(()) => {
            // Lets a stopped task act on it.
            let _ = child.signal(libc::SIGCONT);
        }
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => return Ok(Canceled::Requested(job)),
        Err(source) => {
            return Err(Error::CannotSignal {
                id: job.id,
                why: format!(
                    "its loadmaster is gone, and its task's process group {} could not be \
                     signalled",
                    child.pgid
                ),
                source,
            });
        }
    }
    tx.execute(
        "UPDATE cancel_requests SET signal_done = max(signal_done, kill) WHERE job = ?1",
        [job.id],
    )?;
    Ok(Canceled::Signaled(job))
}

fn delete_job(tx: &Transaction, id: JobId) -> Result<()> {
    tx.execute("DELETE FROM jobs WHERE rowid = ?1", [id])?;
    tx.execute("DELETE FROM cancel_requests WHERE job = ?1", [id])?;
    Ok(())
}

/// A history record for a job that its loadmaster process did not log
/// itself, waiting until `waited_until`.
fn record(job: &Job, outcome: Outcome, waited_until: SystemTime) -> Record<'_> {
    Record {
        job: job.id,
        task: &job.task,
        weight: job.weight,
        capacity: job.capacity,
        measure_only: job.measure_only,
        dir: job.dir.to_string_lossy(),
        args: &job.args,
        outcome,
        queued_at: history::unix_ms(job.enqueued_at),
        wait_ms: millis(
            waited_until
                .duration_since(job.enqueued_at)
                .unwrap_or_default(),
        ),
        run_ms: None,
        cpu_ms: None,
        load: None,
        machine_busy: None,
        signal: None,
        exit_code: None,
        started: None,
        ok: None,
        stopped: false,
        granted: job.granted.filter(|&granted| granted < job.weight),
        min_weight: job.min_weight.filter(|&least| least < job.weight),
        uses: job
            .uses
            .iter()
            .map(|u| (u.name.clone(), u.amount))
            .collect(),
    }
}

/// A place in line, and later a slot. Dropping it leaves the queue (or frees
/// the slot), so no normal code path can leak capacity.
pub struct Ticket<'q> {
    queue: &'q Queue,
    id: JobId,
    resumed: bool,
    armed: bool,
    /// The queue as this job's last poll read it (see [`Ticket::try_admit`]).
    last: RefCell<Option<Poll>>,
}

/// What a poll read, and what the rule made of it.
struct Poll {
    /// The version of the database it was read at (see [`Queue::version`]).
    version: (i64, u64),
    jobs: Vec<Job>,
    /// The decision, once worked out, and the CPU outside the queue it was
    /// worked out with.
    decided: Option<(Option<Outside>, Admission)>,
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

    /// A request from `loadmaster cancel` that this job's loadmaster has
    /// not acted on, taken so it is acted on once: `Some(true)` to kill the
    /// task, `Some(false)` to stop it with SIGTERM.
    pub fn cancel_requested(&self) -> Result<Option<bool>> {
        // Almost always there is none; look without the write lock first.
        let pending: bool = self.queue.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM cancel_requests
                            WHERE job = ?1 AND kill > owner_done)",
            [self.id],
            |r| r.get(0),
        )?;
        if !pending {
            return Ok(None);
        }
        let tx = self.queue.begin()?;
        let kill: Option<bool> = tx
            .query_row(
                "UPDATE cancel_requests SET owner_done = kill
                 WHERE job = ?1 AND kill > owner_done
                 RETURNING kill",
                [self.id],
                |r| r.get(0),
            )
            .optional()?;
        tx.commit()?;
        Ok(kill)
    }

    /// Starts the job if it is at the head of the line and fits.
    pub fn try_admit(&self) -> Result<Admission> {
        // Look without the write lock first: most polls change nothing, and
        // taking the lock for them would make every waiter contend with the
        // few writes that matter (admitting the head, recording a task).
        // Only running jobs and those ahead can change this job's decision;
        // dead jobs further back are reaped by whoever they are ahead of.
        //
        // The rule is a function of the jobs and the CPU outside the queue:
        // while neither changed, the last decision stands. Whether the jobs
        // are alive, and how old the CPU measurement is, change without a
        // write, so those are looked at on every poll.
        let version = self.queue.version()?;
        let mut last = self.last.borrow_mut();
        if last.as_ref().is_none_or(|poll| poll.version != version) {
            *last = Some(Poll {
                version,
                jobs: load_jobs(&self.queue.conn)?,
                decided: None,
            });
        }
        let poll = last.as_mut().expect("read above");
        let adapts = poll
            .jobs
            .iter()
            .any(|job| job.state != JobState::Running && job.adapt_cpu);
        let outside = if adapts {
            self.queue.outside(&poll.jobs)?
        } else {
            None
        };
        // Measuring the CPU outside the queue writes only its own row.
        poll.version.1 = self.queue.conn.total_changes();
        let now = SystemTime::now();
        let relevant_alive = poll
            .jobs
            .iter()
            .filter(|job| job.state == JobState::Running || job.id < self.id)
            .all(|job| job.is_live(now));
        if relevant_alive {
            if poll.decided.as_ref().is_none_or(|(o, _)| *o != outside) {
                poll.decided = decide(&poll.jobs, self.id, outside)
                    .ok()
                    .map(|admission| (outside, admission));
            }
            if let Some((_, waiting @ Admission::Waiting(_))) = &poll.decided {
                return Ok(waiting.clone());
            }
        }
        drop(last);
        // Every waiting job the rule lets start now starts, in this
        // transaction, with the cores it gets: each counts as running for the
        // jobs behind it, so they are to start together. Their loadmasters
        // find them started on their next poll.
        let (admission, _) = self.queue.write(|tx| {
            let jobs = load_jobs(tx)?;
            let admission = decide(&jobs, self.id, outside)?;
            let now = unix_ms(SystemTime::now());
            for (id, cores) in admission::plan(&jobs, outside) {
                tx.execute(
                    "UPDATE jobs SET state = 'running', started_at = ?2, granted = ?3
                     WHERE rowid = ?1 AND state = 'waiting'",
                    params![id, now, cores],
                )?;
            }
            Ok(admission)
        })?;
        Ok(admission)
    }

    /// Starts the job at once, whatever its place in line and the capacity:
    /// measure-only mode.
    /// The cores this job started with (see [`Job::granted`]).
    pub fn granted(&self) -> Result<u32> {
        self.queue
            .conn
            .query_row(
                "SELECT granted FROM jobs WHERE rowid = ?1 AND granted IS NOT NULL",
                [self.id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::JobVanished(self.id))
    }

    pub fn admit_now(&self) -> Result<()> {
        let (started, _) = self.queue.write(|tx| {
            Ok(tx.execute(
                "UPDATE jobs SET state = 'running', started_at = ?2, granted = weight
                 WHERE rowid = ?1 AND state = 'waiting'",
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
    ///
    /// `forward` passes on the signals that arrived meanwhile and says
    /// whether the task has had a SIGTERM; it runs in the same write
    /// transaction, so that this is recorded for a `cancel`, which signals
    /// what is left of the task itself, with nothing in between.
    pub fn hand_over(mut self, forward: impl FnOnce() -> bool) -> Result<()> {
        self.armed = false;
        let tx = self.queue.begin()?;
        if forward() {
            tx.execute(
                "INSERT INTO cancel_requests (job, kill, owner_done) VALUES (?1, 0, 0)
                 ON CONFLICT (job) DO UPDATE SET owner_done = max(owner_done, 0)",
                [self.id],
            )?;
        }
        tx.execute(
            "UPDATE jobs SET owner_pid = NULL, owner_start = NULL WHERE rowid = ?1",
            [self.id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Stops waiting but keeps the place in line for [`PARK_TTL`]: an
    /// [`Queue::enqueue`] with the same (`dir`, `task`, `args`) resumes it.
    /// Unless another process, admitting jobs, started it meanwhile: then it
    /// is to run, the ticket is kept, and this returns false.
    pub fn park(&mut self) -> Result<bool> {
        let tx = self.queue.begin()?;
        let parked = tx.execute(
            "UPDATE jobs SET state = 'parked', owner_pid = NULL, owner_start = NULL,
                 parked_at = ?2
             WHERE rowid = ?1 AND state = 'waiting'",
            params![self.id, unix_ms(SystemTime::now())],
        )?;
        let started: bool = tx
            .query_row(
                "SELECT state = 'running' FROM jobs WHERE rowid = ?1",
                [self.id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        tx.commit()?;
        match (parked, started) {
            (1, _) => {
                self.armed = false;
                Ok(true)
            }
            (_, true) => Ok(false),
            _ => Err(Error::JobVanished(self.id)),
        }
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
/// Amounts above a limit count as the whole limit.
fn demands(job: &Job, outside: Option<Outside>) -> Vec<(Dim<'_>, u64, u64)> {
    let (cpu, limit) = cpu_demand(job, outside);
    let mut demands = vec![(Dim::Cpu, u64::from(cpu), u64::from(limit))];
    for u in &job.uses {
        // Without a capacity of its own, the job is still counted: resource
        // names are shared, and other jobs may have one.
        let (amount, limit) = match u.capacity {
            Some(limit) => (u.amount.min(limit), u64::from(limit)),
            None => (u.amount, u64::MAX),
        };
        demands.push((Dim::Resource(&u.name), u64::from(amount), limit));
        // Counted in its checkout too, with or without a limit there: other
        // jobs of the checkout may have one.
        let (amount, limit) = match u.per_checkout {
            Some(limit) => (u.amount.min(limit), u64::from(limit)),
            None => (u.amount, u64::MAX),
        };
        demands.push((
            Dim::PerCheckout(&u.name, &job.checkout),
            u64::from(amount),
            limit,
        ));
    }
    demands
}

/// The cores `job` holds, or would start with, and the CPU limit it is
/// judged by: a running job keeps what it was granted, whatever the CPU
/// outside the queue has done since; a waiting one is judged by its
/// capacity lowered by the CPU busy outside the queue now.
/// The cores `job` holds, or the most it starts with now, and the limit on
/// CPU it starts within: its capacity, lowered by CPU busy outside the queue
/// if it adapts to it, but never below the least it may start with.
fn cpu_demand(job: &Job, outside: Option<Outside>) -> (u32, u32) {
    if job.state == JobState::Running {
        return (job.granted.unwrap_or(job.weight), job.capacity);
    }
    let limit = outside.filter(|_| job.adapt_cpu).map_or(job.capacity, |o| {
        o.cpu_limit(job.capacity).max(job.least()).min(job.capacity)
    });
    (job.weight.min(limit), limit)
}

impl Dim<'_> {
    fn need(self) -> Need {
        match self {
            Dim::Cpu => Need::Cores,
            Dim::Resource(name) => Need::Resource(name.to_owned()),
            Dim::PerCheckout(name, _) => Need::CheckoutResource(name.to_owned()),
        }
    }
}

/// The admission rule (see the `admission` module): whether job `id` may
/// start, with `outside` the CPU busy outside the queue.
fn decide(jobs: &[Job], id: JobId, outside: Option<Outside>) -> Result<Admission> {
    let at = jobs
        .iter()
        .position(|job| job.id == id)
        .ok_or(Error::JobVanished(id))?;
    Ok(admission::decide(jobs, at, outside).admission)
}

/// Whether the last of `jobs` may start by the admission rule, with no CPU
/// busy outside the queue: for `loadmaster check`, which works out what the
/// rule makes of a config on a made-up queue.
pub fn would_start(jobs: &[Job]) -> bool {
    jobs.last()
        .is_some_and(|last| decide(jobs, last.id, None).is_ok_and(|a| a == Admission::Admitted))
}

/// The admission rule for every job that is not running, in line order:
/// whether it may start and, if not, why. For `loadmaster status`, which
/// thereby says exactly what the rule decides.
pub fn explain(jobs: &[Job], outside: Option<Outside>) -> Vec<(JobId, Admission)> {
    admission::explain(jobs, outside)
        .into_iter()
        .map(|d| (d.id, d.admission))
        .collect()
}

/// [`explain`], with the cores each job that may start starts with.
pub fn explain_with_cores(
    jobs: &[Job],
    outside: Option<Outside>,
) -> Vec<(JobId, Admission, Option<u32>)> {
    admission::explain(jobs, outside)
        .into_iter()
        .map(|d| (d.id, d.admission, d.cores))
        .collect()
}

fn reap(tx: &Transaction) -> Result<Vec<Job>> {
    let now = SystemTime::now();
    let dead: Vec<Job> = load_jobs(tx)?
        .into_iter()
        .filter(|job| !job.is_live(now))
        .collect();
    for job in &dead {
        delete_job(tx, job.id)?;
    }
    Ok(dead)
}

/// A new measurement of the CPU busy outside the queue (`None` if nothing
/// could be measured), averaged with the last one and its age if that is
/// younger than [`CPU_SMOOTH`].
fn smoothed(now: Option<f64>, last: Option<(f64, Duration)>) -> Option<f64> {
    let last = last
        .filter(|&(_, age)| age < CPU_SMOOTH)
        .map(|(busy, _)| busy);
    match (now, last) {
        (Some(now), Some(last)) => Some((now + last) / 2.0),
        (now, last) => now.or(last),
    }
}

/// The last measurement of the CPU busy outside the queue (see
/// [`Queue::outside`]).
struct CpuRow {
    at: SystemTime,
    cores: u32,
    /// `None` if no tick passed, or a count went back.
    outside: Option<f64>,
}

impl CpuRow {
    fn outside(&self) -> Option<Outside> {
        self.outside.map(|busy| Outside {
            busy,
            cores: self.cores,
        })
    }
}

fn read_cpu(conn: &Connection) -> Result<Option<CpuRow>> {
    Ok(conn
        .query_row("SELECT at, cores, outside FROM cpu WHERE id = 1", [], |r| {
            Ok(CpuRow {
                at: from_unix_ms(r.get(0)?),
                cores: r.get(1)?,
                outside: r.get(2)?,
            })
        })
        .optional()?)
}

/// Whether the schema has been made: the last statement of [`SCHEMA`] marks
/// it.
/// Moves a queue of protocol 1 to protocol 2 if it is empty, else closes it
/// to new jobs until it is (see [`CLOSE_FOR_PROTOCOL_2`]).
fn move_or_close(tx: &Transaction) -> rusqlite::Result<()> {
    let empty: bool = tx.query_row("SELECT NOT EXISTS (SELECT 1 FROM jobs)", [], |r| r.get(0))?;
    if empty {
        tx.execute_batch(LAYOUT_3)
    } else if !closed(tx)? {
        tx.execute_batch(CLOSE_FOR_PROTOCOL_2)
    } else {
        Ok(())
    }
}

/// Whether the queue is closed to new jobs for its move to protocol 2.
fn closed(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'trigger' AND name = 'queue_closed')",
        [],
        |r| r.get(0),
    )
}

/// The jobs of a queue that a run of this version started to move to
/// protocol 2 and that has not moved yet: all of 0.12 to 0.14, which hold
/// their places by protocol 1. `None` if the queue has moved, or no run has
/// started to move it.
pub fn moving(conn: &Connection) -> rusqlite::Result<Option<usize>> {
    if layout(conn)? == LAYOUT || !closed(conn)? {
        return Ok(None);
    }
    conn.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
        .map(|n| Some(n as usize))
}

/// The layout of the database (see [`SCHEMA`]), 0 if it has none yet.
fn layout(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

/// The admission protocol of the queue, if it records one.
fn protocol(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT version FROM protocol WHERE id = 1", [], |r| {
        r.get(0)
    })
    .optional()
}

fn load_jobs(conn: &Connection) -> Result<Vec<Job>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT rowid, task, weight, state, dir, args, command, owner_pid, owner_start,
                    child_pgid, child_start, enqueued_at, parked_at, started_at,
                    capacity, measure_only, checkout, uses, adapt_cpu, stop_signal, granted,
                    state_since, {}
             FROM jobs ORDER BY rowid",
        if layout(conn)? == LAYOUT {
            "min_weight"
        } else {
            "NULL"
        }
    ))?;
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
        checkout: PathBuf::from(OsString::from_vec(row.get(16)?)),
        uses: json(17, row.get(17)?)?,
        adapt_cpu: row.get(18)?,
        stop_signal: row.get(19)?,
        granted: row.get(20)?,
        state_since: row.get::<_, Option<i64>>(21)?.map(from_unix_ms),
        min_weight: row.get(22)?,
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

mod admission;
mod other_queues;
pub use other_queues::{OtherJobs, OtherQueue, jobs_in_other_queues};

#[cfg(test)]
mod tests;
