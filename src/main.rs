//! The `loadmaster` command line: argument parsing, messages, exit codes.
//! Queue logic lives in the library.

use std::env;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use clap::{Args, Parser, Subcommand};

use loadmaster::config::{self, MachineConfig, RepoConfig, Task};
use loadmaster::history::{self, HISTORY_FILE, Outcome, Record};
use loadmaster::process::{self, ProcessId};
use loadmaster::queue::{
    Admission, Job, JobId, JobRequest, JobState, PARK_TTL, Queue, STATE_DB_FILE, Ticket,
};
use loadmaster::report::{self, format_duration};
use loadmaster::runner::{self, HELD_ENV, Signals, SpawnError, WEIGHT_ENV};
use loadmaster::{Error, Result};

/// `--max-wait` elapsed; the job keeps its place in line (EX_TEMPFAIL).
const EXIT_GAVE_UP: i32 = 75;
/// `loadmaster held`: this process is not inside a job of this checkout.
const EXIT_NOT_HELD: i32 = 1;
/// loadmaster itself failed, following the `env`/`timeout` convention.
const EXIT_ERROR: i32 = 125;
const EXIT_CANNOT_EXECUTE: i32 = 126;
const EXIT_NOT_FOUND: i32 = 127;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Writes a line to stderr, ignoring errors: `eprintln!` would panic if stderr
/// is a closed pipe (e.g. `loadmaster run test 2>&1 | head -1`).
macro_rules! say {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stderr(), $($arg)*);
    }};
}

#[derive(Parser)]
#[command(
    name = "loadmaster",
    version,
    about = "Weighted job admission for parallel AI agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a task from .loadmaster.toml once its weight fits in the machine's capacity
    Run(RunArgs),
    /// Show running and waiting jobs
    Status,
    /// Reclaim places and slots held by jobs whose processes are gone
    Clean,
    /// Summarize the history log: results, run and wait times, load
    Report(ReportArgs),
    /// Exit 0 if this process runs inside a loadmaster job started from this
    /// checkout, 1 if not; for scripts that must only run through the queue
    Held,
}

#[derive(Args)]
struct ReportArgs {
    /// Only checks from this recent period, e.g. 12h, 7d, 2w
    #[arg(long, value_name = "DURATION", value_parser = report::parse_since)]
    since: Option<Duration>,

    /// Only checks run from a directory whose path contains TEXT, e.g. a
    /// project name shared by its worktrees
    #[arg(long, value_name = "TEXT")]
    dir: Option<String>,
}

#[derive(Args)]
struct RunArgs {
    /// Task name, as defined under [tasks] in .loadmaster.toml
    task: String,

    /// Give up after waiting this long and exit with code 75, keeping the place
    /// in line: running the same command again from the same directory resumes it
    #[arg(long, value_name = "SECONDS", env = "LOADMASTER_MAX_WAIT")]
    max_wait: Option<u64>,

    /// Extra arguments, appended to the task's cmd
    #[arg(last = true, value_name = "ARGS")]
    args: Vec<String>,
}

fn main() {
    let cli = Cli::try_parse().unwrap_or_else(|e| {
        let _ = e.print();
        std::process::exit(if e.use_stderr() { EXIT_ERROR } else { 0 });
    });
    let result = match cli.command {
        Cmd::Run(args) => run(args),
        Cmd::Status => status(),
        Cmd::Clean => clean(),
        Cmd::Report(args) => report(args),
        Cmd::Held => held(),
    };
    let code = result.unwrap_or_else(|e| {
        say!("loadmaster: error: {e}");
        EXIT_ERROR
    });
    std::process::exit(code);
}

fn run(args: RunArgs) -> Result<i32> {
    let cwd = env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
    let repo = RepoConfig::discover(&cwd)?;
    let task = repo.task(&args.task)?;
    let command = runner::command(&task.cmd, &args.args, repo.dir());

    if let Some(held) = env::var_os(HELD_ENV).filter(|v| !v.is_empty()) {
        return run_nested(&held, command);
    }

    let machine = MachineConfig::load()?;
    if task.weight > machine.capacity {
        return Err(Error::WeightExceedsCapacity {
            task: task.name.clone(),
            weight: task.weight,
            capacity: machine.capacity,
            capacity_source: machine.capacity_source,
            path: repo.path().to_owned(),
        });
    }

    let signals = Signals::install().map_err(|e| Error::io("cannot install signal handlers", e))?;
    let state_dir = open_state_dir()?;
    let queue = Queue::open(&state_dir.join(STATE_DB_FILE))?;
    let owner = ProcessId::current().map_err(|e| Error::io("cannot identify this process", e))?;
    let display = display_command(&task.cmd, &args.args);
    let request = JobRequest {
        task: &task.name,
        weight: task.weight,
        dir: &cwd,
        args: &args.args,
        command: &display,
        capacity: machine.capacity,
        measure_only: machine.measure_only,
    };
    // Both clocks start here, so wait times include getting in line.
    let (queued_at, wait_started) = (SystemTime::now(), Instant::now());
    let ticket = queue.enqueue(&request, owner)?;
    if ticket.resumed() {
        say!("loadmaster: resumed your place in line for `{}`", task.name);
    }

    let mut record = Record {
        job: ticket.id(),
        task: &task.name,
        weight: task.weight,
        capacity: Some(machine.capacity),
        measure_only: machine.measure_only,
        dir: cwd.to_string_lossy(),
        args: &args.args,
        outcome: Outcome::Exited,
        queued_at: history::unix_ms(queued_at),
        wait_ms: 0,
        run_ms: None,
        cpu_ms: None,
        load: None,
        signal: None,
        exit_code: None,
    };
    let result = (|| {
        let max_wait = args.max_wait.map(Duration::from_secs);
        let turn = if machine.measure_only {
            // Admitted whatever the capacity and place in line: the run
            // behaves as without loadmaster, and is only recorded.
            ticket.admit_now().map(|()| Turn::Admitted)
        } else {
            wait_turn(&ticket, task, machine.capacity, max_wait, &signals)
        };
        record.wait_ms = millis(wait_started.elapsed());
        match turn? {
            Turn::Admitted => run_admitted(
                ticket,
                &state_dir,
                command,
                task,
                &signals,
                wait_started,
                &mut record,
            ),
            Turn::GaveUp { ahead, used } => {
                ticket.park()?;
                say!(
                    "loadmaster: gave up waiting for `{}` after {} ({}, {used}/{} in use)\n\
                     loadmaster: your place in line is kept for {}s; \
                     run the same command again from this directory to resume",
                    task.name,
                    format_duration(wait_started.elapsed()),
                    format_ahead(ahead),
                    machine.capacity,
                    PARK_TTL.as_secs(),
                );
                record.outcome = Outcome::GaveUp;
                Ok(EXIT_GAVE_UP)
            }
            Turn::Interrupted(signal) => {
                drop(ticket);
                record.outcome = Outcome::Interrupted;
                Ok(128 + signal)
            }
        }
    })();
    // Every run that got in line leaves exactly one history line.
    match &result {
        Ok(code) => record.exit_code = Some(*code),
        Err(_) => {
            record.outcome = Outcome::Error;
            record.exit_code = Some(EXIT_ERROR);
        }
    }
    if let Err(e) = history::append(&state_dir, &record) {
        say!("loadmaster: warning: cannot write the history log: {e}");
    }
    result
}

enum Turn {
    Admitted,
    GaveUp { ahead: u32, used: u64 },
    Interrupted(i32),
}

fn wait_turn(
    ticket: &Ticket,
    task: &Task,
    capacity: u32,
    max_wait: Option<Duration>,
    signals: &Signals,
) -> Result<Turn> {
    let started = Instant::now();
    let mut last_report: Option<Instant> = None;
    loop {
        if let Some(&signal) = signals.take().first() {
            return Ok(Turn::Interrupted(signal));
        }
        match ticket.try_admit(capacity)? {
            Admission::Admitted => {
                if last_report.is_some() {
                    say!(
                        "loadmaster: starting `{}` after waiting {}",
                        task.name,
                        format_duration(started.elapsed())
                    );
                }
                return Ok(Turn::Admitted);
            }
            Admission::Waiting { ahead, used } => {
                if max_wait.is_some_and(|max| started.elapsed() >= max) {
                    // A signal that arrived meanwhile wins: leave the line
                    // instead of parking.
                    if let Some(&signal) = signals.take().first() {
                        return Ok(Turn::Interrupted(signal));
                    }
                    return Ok(Turn::GaveUp { ahead, used });
                }
                if last_report.is_none_or(|at| at.elapsed() >= REPORT_INTERVAL) {
                    say!(
                        "loadmaster: waiting to run `{}` (weight {}): {}, {used}/{capacity} in use",
                        task.name,
                        task.weight,
                        format_ahead(ahead),
                    );
                    last_report = Some(Instant::now());
                }
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn run_admitted(
    ticket: Ticket,
    state_dir: &Path,
    mut command: Command,
    task: &Task,
    signals: &Signals,
    wait_started: Instant,
    record: &mut Record,
) -> Result<i32> {
    if let Some(&signal) = signals.take().first() {
        record.outcome = Outcome::Interrupted;
        return Ok(128 + signal);
    }
    command
        .env(HELD_ENV, ticket.id().to_string())
        .env(WEIGHT_ENV, task.weight.to_string())
        // Absolute, so nested runs find the same queue from any directory.
        .env(config::STATE_DIR_ENV, state_dir);
    let (db, id) = (state_dir.join(STATE_DB_FILE), ticket.id());
    // Runs on another thread, so it opens its own connection.
    let register = move |group| -> Result<()> { Queue::open(&db)?.record_child(id, group) };
    let cpu_before = process::children_cpu_time();
    let mut child = match runner::spawn(&mut command, register) {
        Ok(child) => child,
        Err(SpawnError::Register(e)) => return Err(e),
        Err(SpawnError::Spawn(e)) => {
            say!("loadmaster: error: cannot run `{}`: {e}", task.cmd[0]);
            record.outcome = Outcome::SpawnFailed;
            return Ok(spawn_failure_code(&e));
        }
    };
    // `spawn` returns once the task has been registered and exec'd: the
    // wait ends and the run begins here.
    let started = Instant::now();
    record.wait_ms = millis(started.duration_since(wait_started));
    let status =
        runner::wait(&mut child, signals).map_err(|e| Error::io("cannot wait for the task", e))?;
    // Measured before the slot is released, which may wait for the database.
    record.run_ms = Some(millis(started.elapsed()));
    record.cpu_ms = process::children_cpu_time()
        .zip(cpu_before)
        .and_then(|(after, before)| after.checked_sub(before))
        .map(millis);
    record.load = process::load_average();
    record.signal = status.signal();
    let handed_over = if child.group.is_alive() {
        say!(
            "loadmaster: `{}` exited, but processes it started are still running \
             (process group {}); its slot stays taken until they exit",
            task.name,
            child.group.pgid
        );
        ticket.detach()
    } else {
        drop(ticket);
        Ok(())
    };
    // Signals that arrived while the slot was being handed over or released
    // (even if that failed) still go to what is left of the task.
    runner::forward_pending(&child.group, signals);
    if let Err(e) = handed_over {
        // As with a failed release: the task's result stands, and the job
        // keeps its owner, so it is reclaimed once its processes are gone.
        say!(
            "loadmaster: warning: could not hand over job #{id}: {e} \
             (it will be reclaimed once its processes exit)"
        );
    }
    Ok(runner::exit_code(status))
}

/// A `loadmaster run` inside a task that already holds a slot: verify the
/// holder, then run directly. The nested task's load counts against the
/// holder's weight.
fn run_nested(held: &OsStr, mut command: Command) -> Result<i32> {
    let value = held.to_string_lossy().into_owned();
    let id: Option<JobId> = value.trim().parse().ok();
    let queue = Queue::open(&open_state_dir()?.join(STATE_DB_FILE))?;
    match id {
        Some(id) if queue.holder(id)?.is_some() => {}
        _ => return Err(Error::InvalidHeld { value }),
    }
    drop(queue);
    let program = command.get_program().to_string_lossy().into_owned();
    let err = command.exec();
    say!("loadmaster: error: cannot run `{program}`: {err}");
    Ok(spawn_failure_code(&err))
}

/// Whether this process runs inside a job started from this checkout: the
/// check made for nested runs, plus the checkout. Silent when it does, so a
/// script can use it as a guard.
fn held() -> Result<i32> {
    let cwd = env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
    let checkout =
        RepoConfig::find(&cwd).ok_or_else(|| Error::RepoConfigNotFound { start: cwd.clone() })?;
    let not_held = |reason: String| -> Result<i32> {
        say!(
            "loadmaster: not running through loadmaster: {reason}\n\
             loadmaster: start it with `loadmaster run <task>`; the tasks are in {}",
            checkout.display()
        );
        Ok(EXIT_NOT_HELD)
    };
    let Some(value) = env::var_os(HELD_ENV).filter(|v| !v.is_empty()) else {
        return not_held(format!("{HELD_ENV} is not set"));
    };
    let value = value.to_string_lossy().into_owned();
    let queue = Queue::open(&open_state_dir()?.join(STATE_DB_FILE))?;
    let job = match value.trim().parse::<JobId>() {
        Ok(id) => queue.holder(id)?,
        Err(_) => None,
    };
    let Some(job) = job else {
        return not_held(format!(
            "{HELD_ENV}={value} does not refer to a running job"
        ));
    };
    // Resolved, so a symlink to the same checkout matches.
    let here = fs::canonicalize(&checkout).ok();
    let theirs = RepoConfig::find(&job.dir).and_then(|path| fs::canonicalize(path).ok());
    if here.is_none() || theirs != here {
        return not_held(format!(
            "job #{} (`{}`) was started from another checkout, in {}",
            job.id,
            job.task,
            job.dir.display()
        ));
    }
    Ok(0)
}

fn spawn_failure_code(err: &io::Error) -> i32 {
    if err.kind() == io::ErrorKind::NotFound {
        EXIT_NOT_FOUND
    } else {
        EXIT_CANNOT_EXECUTE
    }
}

fn status() -> Result<i32> {
    let machine = MachineConfig::load()?;
    let queue = Queue::open(&open_state_dir()?.join(STATE_DB_FILE))?;
    let snapshot = queue.snapshot()?;
    let mut out = format!(
        "{}/{} in use (capacity {})\n",
        snapshot.used(),
        machine.capacity,
        machine.capacity_source
    );
    if machine.measure_only {
        out += "measure-only: jobs start at once and are only recorded (see `loadmaster report`)\n";
    }
    if !snapshot.reaped.is_empty() {
        out += &format!(
            "reclaimed {} job(s) whose processes were gone\n",
            snapshot.reaped.len()
        );
    }
    if snapshot.jobs.is_empty() {
        out += "no jobs\n";
    } else {
        out += "\n";
        out += &jobs_table(&snapshot.jobs);
    }
    print(&out);
    Ok(0)
}

fn jobs_table(jobs: &[Job]) -> String {
    let now = SystemTime::now();
    let since = |t: Option<SystemTime>| {
        t.and_then(|t| now.duration_since(t).ok())
            .map_or_else(|| "-".to_owned(), format_duration)
    };
    let mut rows =
        vec![["ID", "STATE", "TASK", "WEIGHT", "PID", "PGID", "FOR", "DIR"].map(String::from)];
    let (mut orphaned, mut parked) = (false, false);
    for job in jobs {
        let (state, at) = match job.state {
            JobState::Running if job.is_orphaned() => {
                orphaned = true;
                ("orphaned", job.started_at)
            }
            JobState::Running => ("running", job.started_at),
            JobState::Waiting => ("waiting", Some(job.enqueued_at)),
            JobState::Parked => {
                parked = true;
                ("parked", job.parked_at)
            }
        };
        rows.push([
            job.id.to_string(),
            state.to_owned(),
            job.task.clone(),
            job.weight.to_string(),
            job.owner.map_or("-".to_owned(), |o| o.pid.to_string()),
            job.child.map_or("-".to_owned(), |c| c.pgid.to_string()),
            since(at),
            job.dir.display().to_string(),
        ]);
    }
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|col| {
            rows.iter()
                .map(|r| r[col].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in &rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        out += cells.join("  ").trim_end();
        out += "\n";
    }
    if orphaned {
        out += "\norphaned: loadmaster is gone but the task's process group is still running; \
                the slot is held until it exits\n";
    }
    if parked {
        out += &format!(
            "\nparked: gave up waiting (--max-wait); keeps its place for {}s for a re-run \
             of the same command from the same directory\n",
            PARK_TTL.as_secs()
        );
    }
    out
}

fn clean() -> Result<i32> {
    let queue = Queue::open(&open_state_dir()?.join(STATE_DB_FILE))?;
    let reaped = queue.reap()?;
    let mut out = String::new();
    if reaped.is_empty() {
        out += "nothing to reclaim\n";
    } else {
        out += &format!("reclaimed {} job(s):\n", reaped.len());
        for job in &reaped {
            out += &format!(
                "  #{} {} (weight {}) in {}\n",
                job.id,
                job.task,
                job.weight,
                job.dir.display()
            );
        }
    }
    print(&out);
    Ok(0)
}

fn report(args: ReportArgs) -> Result<i32> {
    let path = config::state_dir()?.join(HISTORY_FILE);
    let (entries, unreadable) = match history::read(&path) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), 0),
        Err(e) => return Err(Error::io(format!("cannot read {}", path.display()), e)),
    };
    let now = history::unix_ms(SystemTime::now());
    let options = report::Options {
        source: &path,
        since: args.since.map(|d| now.saturating_sub(millis(d))),
        dir: args.dir.as_deref(),
        unreadable,
        cpus: thread::available_parallelism().ok().map(|n| n.get()),
    };
    print(&report::render(&entries, &options));
    Ok(0)
}

fn open_state_dir() -> Result<PathBuf> {
    let dir = config::state_dir()?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| Error::io(format!("cannot create {}", dir.display()), e))?;
    Ok(dir)
}

fn print(text: &str) {
    // Ignore EPIPE (e.g. `loadmaster status | head`).
    let _ = io::stdout().lock().write_all(text.as_bytes());
}

fn format_ahead(ahead: u32) -> String {
    match ahead {
        0 => "next in line".to_owned(),
        1 => "1 job ahead".to_owned(),
        n => format!("{n} jobs ahead"),
    }
}

fn display_command(cmd: &[String], args: &[String]) -> String {
    cmd.iter()
        .chain(args)
        .map(|arg| {
            let plain = !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c));
            if plain {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
