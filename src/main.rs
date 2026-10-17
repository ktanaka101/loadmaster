//! The `loadmaster` command line: argument parsing, messages, exit codes.
//! Queue logic lives in the library.

use std::env;
use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use clap::{Args, Parser, Subcommand};

mod json;

use loadmaster::check;
use loadmaster::config::{self, MachineConfig, RepoConfig, Task};
use loadmaster::history::{self, HISTORY_FILE, Outcome, Record};
use loadmaster::process::{self, ProcessId};
use loadmaster::queue::{
    self as queue, Admission, Canceled, Job, JobId, JobRequest, JobState, PARK_TTL, QUEUE_DB_FILE,
    Queue, Ticket, Use, Wait,
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

/// The default for `--max-wait`.
const MAX_WAIT_ENV: &str = "LOADMASTER_MAX_WAIT";

const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Writes a line to stderr, ignoring errors: `eprintln!` would panic if stderr
/// is a closed pipe (e.g. `loadmaster run test 2>&1 | head -1`).
macro_rules! say {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stderr(), $($arg)*);
    }};
}

/// [`say!`] for a line that only informs (a job started, waits, ended),
/// which `LOADMASTER_QUIET` leaves out; what asks for action is always said.
macro_rules! tell {
    ($($arg:tt)*) => {{
        if !crate::quiet() {
            say!($($arg)*);
        }
    }};
}

/// Set (not empty, not `0`): `run` says only what asks for action
/// (warnings, errors, giving up, `next:`), so the task's stderr passes
/// through as it is.
const QUIET_ENV: &str = "LOADMASTER_QUIET";

fn quiet() -> bool {
    static QUIET: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *QUIET.get_or_init(|| env::var_os(QUIET_ENV).is_some_and(|v| !v.is_empty() && v != "0"))
}

// After `say!`, which the commands use.
mod cli;

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
    /// How AI agents run checks here: the rules, and this repository's tasks
    Guide,
    /// Whether loadmaster works for agents here (their tool's sandbox and
    /// timeouts, the configs, the queue), and what to change where not; exit
    /// code 1 if something does not work
    Doctor(DoctorArgs),
    /// Run a task from .loadmaster.toml once its weight fits in the machine's capacity
    Run(RunArgs),
    /// Show running and waiting jobs
    Status(StatusArgs),
    /// Reclaim places and slots held by jobs whose processes are gone
    Clean,
    /// Summarize the history log: results, run and wait times, load
    Report(ReportArgs),
    /// What other weights or min_weights of this checkout's tasks would have
    /// done to this machine's checks, replayed through the admission rule
    Tune(TuneArgs),
    /// Validate .loadmaster.toml and the machine config, and show what they
    /// mean on this machine; does not touch the queue
    Check(CheckArgs),
    /// Stop jobs, waiting or running, and wait until they have left the queue
    Cancel(CancelArgs),
    /// Exit 0 if this process runs inside a loadmaster job started from this
    /// checkout, 1 if not; for scripts that must only run through the queue
    Held(HeldArgs),
}

#[derive(Args)]
struct CheckArgs {
    /// Instead, judge the queue FILE describes, written as `status --json`
    /// writes it (`-` for stdin), by the admission rule, with the CPU busy
    /// outside the queue it gives; reads no config and no queue
    #[arg(long, value_name = "FILE")]
    scenario: Option<PathBuf>,
}

#[derive(Args)]
struct DoctorArgs {
    /// Print the findings as JSON, for tools
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct StatusArgs {
    /// Print it as JSON, in the documented format, for tools
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CancelArgs {
    /// Job IDs, as `loadmaster status` shows them
    #[arg(
        value_name = "ID",
        required_unless_present = "here",
        conflicts_with = "here"
    )]
    ids: Vec<JobId>,

    /// Every job started from this checkout, except the one this runs in
    #[arg(long)]
    here: bool,

    /// SIGKILL instead of SIGTERM: for a task that does not stop. A running
    /// task gets it directly, so it has no chance to clean up
    #[arg(long)]
    kill: bool,
}

#[derive(Args)]
struct HeldArgs {
    /// Also require the job to use this resource; can be repeated
    #[arg(long = "uses", value_name = "RESOURCE")]
    uses: Vec<String>,
}

#[derive(Args)]
struct ReportArgs {
    /// Only checks that got in line from this time on: a period ago (12h,
    /// 7d, 2w) or a local time (2026-10-12T16:44, 2026-10-12)
    #[arg(long, value_name = "TIME", value_parser = report::parse_time)]
    since: Option<report::Time>,

    /// Only checks that got in line before this time, given like --since
    #[arg(long, value_name = "TIME", value_parser = report::parse_time)]
    until: Option<report::Time>,

    /// Only checks run from a directory whose path contains TEXT, e.g. a
    /// project name shared by its worktrees
    #[arg(long, value_name = "TEXT")]
    dir: Option<String>,
    /// Only checks with an extra argument (after `--`) that contains TEXT,
    /// e.g. a flag tried with a task
    #[arg(long, value_name = "TEXT")]
    args: Option<String>,
    /// Print it as JSON, for tools
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TuneArgs {
    /// Only checks that got in line from this time on, as for `report`
    #[arg(long, value_name = "TIME", value_parser = report::parse_time)]
    since: Option<report::Time>,
    /// Only checks that got in line before this time, as for `report`
    #[arg(long, value_name = "TIME", value_parser = report::parse_time)]
    until: Option<report::Time>,
    /// Also replay a change of your own: TASK.weight=N or TASK.min_weight=N
    /// (or `none`); can be repeated, the changes taken together
    #[arg(long, value_name = "TASK.KEY=VALUE")]
    set: Vec<String>,
    /// Print it as JSON, for tools
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct RunArgs {
    /// Task name, as defined under [tasks] in .loadmaster.toml
    task: String,

    /// Give up after waiting this long and exit with code 75, keeping the place
    /// in line: running the same command again from the same directory resumes
    /// it. Defaults to $LOADMASTER_MAX_WAIT; empty or unset means no limit
    #[arg(long, value_name = "SECONDS")]
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
        Cmd::Guide => cli::guide::guide(),
        Cmd::Doctor(args) => cli::doctor::doctor(args),
        Cmd::Run(args) => cli::run::run(args),
        Cmd::Status(args) => cli::status::status(args),
        Cmd::Clean => cli::cancel::clean(),
        Cmd::Report(args) => cli::report::report(args),
        Cmd::Tune(args) => cli::tune::tune(args),
        Cmd::Check(args) => match args.scenario {
            Some(path) => cli::check::check_scenario(&path),
            None => cli::check::check(),
        },
        Cmd::Cancel(args) => cli::cancel::cancel(args),
        Cmd::Held(args) => cli::held::held(args),
    };
    let code = result.unwrap_or_else(|e| {
        say!("loadmaster: error: {e}");
        if let Some(next) = e.next() {
            say!("loadmaster: next: {next}");
        }
        EXIT_ERROR
    });
    std::process::exit(code);
}

/// The jobs that run in the queues of other loadmaster versions, which this
/// one does not count, if any.
fn other_queues(state_dir: &Path) -> Option<String> {
    let others = queue::jobs_in_other_queues(state_dir);
    if others.is_empty() {
        return None;
    }
    let count: usize = others
        .iter()
        .filter_map(|other| Some(other.jobs.as_ref()?.count()))
        .sum();
    let list: Vec<String> = others
        .iter()
        .map(|other| {
            let Some(jobs) = &other.jobs else {
                return format!("{}: could not be read, so it may hold some", other.file);
            };
            let named = |what: &str, tasks: &[String]| {
                (!tasks.is_empty()).then(|| {
                    let tasks: Vec<String> = tasks.iter().map(|t| format!("`{t}`")).collect();
                    format!("{what} {}", tasks.join(", "))
                })
            };
            let parts: Vec<String> = [
                named("running", &jobs.running),
                named("waiting", &jobs.waiting),
                named("parked (may resume)", &jobs.parked),
            ]
            .into_iter()
            .flatten()
            .collect();
            format!("{}: {}", other.file, parts.join(", "))
        })
        .collect();
    Some(format!(
        "{count} job(s) in the queue of another loadmaster version, which this one does not \
         count ({})",
        list.join("; ")
    ))
}

/// The state directory, created if needed, and the queue in it. Not being
/// allowed to write there, as in an agent's sandbox, is said in so many
/// words.
fn open_queue() -> Result<(PathBuf, Queue)> {
    let dir = config::state_dir()?;
    let not_writable = |detail: String| Error::StateDirNotWritable {
        dir: dir.clone(),
        detail,
    };
    if let Err(e) = DirBuilder::new().recursive(true).mode(0o700).create(&dir) {
        return Err(match e.kind() {
            io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => {
                not_writable(e.to_string())
            }
            _ => Error::io(format!("cannot create {}", dir.display()), e),
        });
    }
    // Made sure of first: SQLite's errors don't say whether the directory is
    // to blame (its database could be a directory, say), and it opens a
    // database it may not write for reading without saying so.
    // A new file each time (an existing one, a link say, is never opened),
    // which must also be removable.
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let probe = dir.join(format!(".write-test-{}-{nanos}", std::process::id()));
    let made = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|_| fs::remove_file(&probe));
    if let Err(e) = made {
        return Err(not_writable(e.to_string()));
    }
    let path = dir.join(QUEUE_DB_FILE);
    let queue = Queue::open(&path)?;
    if queue.is_read_only() {
        return Err(Error::QueueReadOnly { path });
    }
    Ok((dir, queue))
}

/// `path` with the home directory as `~`: shorter, for an agent's context.
fn tilde(path: &Path) -> String {
    let home = env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
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
