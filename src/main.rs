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
    Status(StatusArgs),
    /// Reclaim places and slots held by jobs whose processes are gone
    Clean,
    /// Summarize the history log: results, run and wait times, load
    Report(ReportArgs),
    /// Validate .loadmaster.toml and the machine config, and show what they
    /// mean on this machine; does not touch the queue
    Check,
    /// Stop jobs, waiting or running, and wait until they have left the queue
    Cancel(CancelArgs),
    /// Exit 0 if this process runs inside a loadmaster job started from this
    /// checkout, 1 if not; for scripts that must only run through the queue
    Held(HeldArgs),
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
        Cmd::Run(args) => run(args),
        Cmd::Status(args) => status(args),
        Cmd::Clean => clean(),
        Cmd::Report(args) => report(args),
        Cmd::Check => check(),
        Cmd::Cancel(args) => cancel(args),
        Cmd::Held(args) => held(args),
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
    let max_wait = match args.max_wait {
        Some(secs) => Some(secs),
        None => max_wait_from_env()?,
    };

    if let Some(held) = env::var_os(HELD_ENV).filter(|v| !v.is_empty()) {
        return run_nested(&held.to_string_lossy(), &repo, task, command);
    }

    let machine = MachineConfig::load()?;
    // Weights belong to the repository and the capacity to the machine, so a
    // weight can exceed a small machine's capacity: the task then takes all
    // of it.
    if task.weight > machine.capacity
        && !machine.measure_only
        && let Some(note) = check::whole_machine(task, machine.capacity)
    {
        say!("loadmaster: {note} (capacity {})", machine.capacity_source);
    }
    let task = &Task {
        weight: task.weight.min(machine.capacity),
        ..task.clone()
    };
    let uses = resource_uses(&repo, task, &machine);
    // Resolved, so every worktree path that leads here is one checkout.
    let checkout = fs::canonicalize(repo.dir()).unwrap_or_else(|_| repo.dir().to_owned());

    let signals = Signals::install().map_err(|e| Error::io("cannot install signal handlers", e))?;
    let (state_dir, queue) = open_queue()?;
    if !machine.measure_only
        && let Some(others) = other_queues(&state_dir)
    {
        say!("loadmaster: warning: {others}; give every agent on this machine the same version");
    }
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
        uses: &uses,
        checkout: &checkout,
        adapt_cpu: machine.adapt_to_outside_cpu,
        stop_signal: task.stop_signal,
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
        capacity: machine.capacity,
        measure_only: machine.measure_only,
        dir: cwd.to_string_lossy(),
        args: &args.args,
        outcome: Outcome::Exited,
        queued_at: history::unix_ms(queued_at),
        wait_ms: 0,
        run_ms: None,
        cpu_ms: None,
        load: None,
        machine_busy: None,
        signal: None,
        exit_code: None,
        started: None,
        ok: None,
        stopped: false,
        granted: None,
        uses: task.uses.clone(),
    };
    // Set once this run's line is written: a place that gave up logs before
    // it is parked, since a parked job has no owner whose loss would be.
    let mut logged = false;
    let log = |record: &Record| {
        if let Err(e) = history::append(&state_dir, record) {
            say!("loadmaster: warning: cannot write the history log: {e}");
        }
    };
    let result = (|| {
        let max_wait = max_wait.map(Duration::from_secs);
        let turn = if machine.measure_only {
            // Admitted whatever the capacity and place in line: the run
            // behaves as without loadmaster, and is only recorded.
            ticket.admit_now().map(|()| Turn::Admitted)
        } else {
            wait_turn(&ticket, task, max_wait, &signals)
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
            Turn::GaveUp(wait) => {
                // Logged first: killed before parking, the job is reaped and
                // logged as lost, which the report joins to this record;
                // killed after parking, nothing else would log it.
                record.outcome = Outcome::GaveUp;
                record.exit_code = Some(EXIT_GAVE_UP);
                log(&record);
                logged = true;
                ticket.park()?;
                say!(
                    "loadmaster: gave up waiting to run `{}` after {} ({}); it waited for {}\n\
                     loadmaster: your place in line is kept for {}s; \
                     run the same command again from this directory to resume",
                    task.name,
                    format_duration(wait_started.elapsed()),
                    format_ahead(wait.ahead),
                    wait.waits_for(),
                    PARK_TTL.as_secs(),
                );
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
    if logged {
        return result;
    }
    match &result {
        Ok(code) => {
            record.exit_code = Some(*code);
            if record.outcome == Outcome::Exited && record.signal.is_none() {
                record.ok = Some(task.ok_exit_codes.contains(code));
            }
            if record.outcome == Outcome::Exited
                && let Some(note) = saturated_note(&record)
            {
                say!("{note}");
            }
        }
        Err(_) => {
            record.outcome = Outcome::Error;
            record.exit_code = Some(EXIT_ERROR);
        }
    }
    log(&record);
    result
}

/// What `task` uses, with this machine's limits: the machine config's
/// capacity, else the repository's. Like a weight above the capacity, an
/// amount above a limit takes all of it.
fn resource_uses(repo: &RepoConfig, task: &Task, machine: &MachineConfig) -> Vec<Use> {
    task.uses
        .iter()
        .map(|(name, &amount)| {
            let declared = repo.resource(name).cloned().unwrap_or_default();
            let capacity = machine.resources.get(name).copied().or(declared.capacity);
            for (limit, what) in [
                (capacity, "capacity"),
                (declared.per_checkout, "per-checkout limit"),
            ] {
                if let Some(limit) = limit.filter(|&limit| amount > limit) {
                    say!(
                        "loadmaster: `{}` uses {amount} of `{name}`, above its {what} of \
                         {limit}; it takes all of it",
                        task.name
                    );
                }
            }
            Use {
                name: name.clone(),
                amount,
                capacity,
                per_checkout: declared.per_checkout,
            }
        })
        .collect()
}

enum Turn {
    Admitted,
    GaveUp(Wait),
    Interrupted(i32),
}

fn wait_turn(
    ticket: &Ticket,
    task: &Task,
    max_wait: Option<Duration>,
    signals: &Signals,
) -> Result<Turn> {
    let started = Instant::now();
    let mut last_report: Option<Instant> = None;
    loop {
        if let Some(&signal) = signals.take().first() {
            return Ok(Turn::Interrupted(signal));
        }
        if let Some(kill) = ticket.cancel_requested()? {
            say!(
                "loadmaster: `{}` was canceled with `loadmaster cancel` while it waited",
                task.name
            );
            return Ok(Turn::Interrupted(cancel_signal(kill, task)));
        }
        match ticket.try_admit()? {
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
            Admission::Waiting(wait) => {
                if max_wait.is_some_and(|max| started.elapsed() >= max) {
                    // A signal that arrived meanwhile wins: leave the line
                    // instead of parking.
                    if let Some(&signal) = signals.take().first() {
                        return Ok(Turn::Interrupted(signal));
                    }
                    return Ok(Turn::GaveUp(wait));
                }
                if last_report.is_none_or(|at| at.elapsed() >= REPORT_INTERVAL) {
                    say!(
                        "loadmaster: waiting to run `{}` (weight {}, {}) for {}",
                        task.name,
                        task.weight,
                        format_ahead(wait.ahead),
                        wait.waits_for(),
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
    if let Some(kill) = ticket.cancel_requested()? {
        say!(
            "loadmaster: `{}` was canceled with `loadmaster cancel`",
            task.name
        );
        record.outcome = Outcome::Interrupted;
        return Ok(128 + cancel_signal(kill, task));
    }
    // Fewer cores than its weight while CPU is busy outside the queue: the
    // task can size its parallelism by them.
    let granted = ticket.granted()?;
    if granted < task.weight {
        say!(
            "loadmaster: starting `{}` with {granted} of its {} cores, since CPU is busy \
             beyond what running jobs got ({WEIGHT_ENV}={granted})",
            task.name,
            task.weight
        );
        record.granted = Some(granted);
    }
    command
        .env(HELD_ENV, ticket.id().to_string())
        .env(WEIGHT_ENV, granted.to_string())
        // Absolute, so nested runs find the same queue and machine config
        // from any directory: the task runs where .loadmaster.toml is.
        .env(config::STATE_DIR_ENV, state_dir);
    // Only if set: the default place is the same from anywhere, and a file
    // named explicitly must exist.
    if let Some(path) = config::machine_config_override()? {
        command.env(config::CONFIG_ENV, path);
    }
    runner::deliver_stop_signal(&mut command, task.stop_signal);
    let (db, id) = (state_dir.join(QUEUE_DB_FILE), ticket.id());
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
    let machine_before = process::cpu_ticks();
    record.wait_ms = millis(started.duration_since(wait_started));
    // A request to kill always goes through; one to stop only if the task
    // has had its stop signal yet (see `runner::wait`).
    let canceled = |stop_sent: bool| {
        // A failed look (a busy database) only puts it off to the next one.
        let kill = ticket.cancel_requested().ok()??;
        if !kill && stop_sent {
            return None;
        }
        say!(
            "loadmaster: `{}` was canceled with `loadmaster cancel`; sending it {}",
            task.name,
            runner::signal_name(cancel_signal(kill, task))
        );
        // SIGTERM asks to stop: the forwarder sends the task's stop signal.
        Some(if kill { libc::SIGKILL } else { libc::SIGTERM })
    };
    let mut forwarder = runner::Forwarder::new(task.stop_signal);
    let mut descendants = process::Descendants::default();
    let (status, stopped) = runner::wait(
        &mut child,
        signals,
        &mut forwarder,
        &mut descendants,
        canceled,
    )
    .map_err(|e| Error::io("cannot wait for the task", e))?;
    // Measured before the slot is released, which may wait for the database.
    record.run_ms = Some(millis(started.elapsed()));
    record.cpu_ms = process::children_cpu_time()
        .zip(cpu_before)
        .and_then(|(after, before)| after.checked_sub(before))
        .map(millis);
    record.load = process::load_average();
    record.machine_busy = machine_busy(machine_before);
    record.signal = status.signal();
    // Asked to stop before it exited: one sent to what is left after does
    // not make a failure a stop.
    record.stopped = stopped;
    warn_of_processes_left(task, &child.group, &mut descendants);
    let handed_over = if child.group.is_alive() {
        say!(
            "loadmaster: `{}` exited, but processes it started are still running in its \
             process group ({}); its slot stays taken until they exit",
            task.name,
            name_group(&child.group)
        );
        // Signals that arrived meanwhile still go to them, and whether they
        // had the stop signal is recorded, in one step with the handover, so
        // a cancel of the job sends them no second one.
        ticket.hand_over(|| {
            forwarder.forward_pending(&child.group, signals);
            forwarder.stop_sent()
        })
    } else {
        drop(ticket);
        Ok(())
    };
    if let Err(e) = handed_over {
        // The job keeps its owner, so signals still go to what is left.
        forwarder.forward_pending(&child.group, signals);
        // As with a failed release: the task's result stands, and the job
        // keeps its owner, so it is reclaimed once its processes are gone.
        say!(
            "loadmaster: warning: could not hand over job #{id}: {e} \
             (it will be reclaimed once its processes exit)"
        );
    }
    Ok(runner::exit_code(status))
}

/// How long processes a task started outside its process group get to exit
/// with it before they count as left running.
const LEFT_GRACE: Duration = Duration::from_millis(500);

/// Warns of processes the task started that left its process group and
/// still run: nothing holds a slot for them or sends them signals, and they
/// may keep ports or files a later run needs (a server that a test runner
/// started, say). Not stopped: some are meant to stay, such as a build
/// tool's daemon.
fn warn_of_processes_left(
    task: &Task,
    group: &process::ProcessGroup,
    seen: &mut process::Descendants,
) {
    seen.look(group);
    let started = Instant::now();
    let mut left = seen.outside_group(group.pgid);
    while !left.is_empty() && started.elapsed() < LEFT_GRACE {
        thread::sleep(POLL_INTERVAL);
        left = seen.outside_group(group.pgid);
    }
    if left.is_empty() {
        return;
    }
    let list = name_processes(&left);
    say!(
        "loadmaster: warning: `{}` exited, but processes it started outside its process group \
         are still running ({list}); nothing holds a slot for them, and they may hold ports or \
         files that a later run needs",
        task.name
    );
}

/// "12345 `node`, 12346 `node` and 2 more".
fn name_processes(processes: &[process::Descendant]) -> String {
    const SHOWN: usize = 3;
    let mut names: Vec<String> = processes
        .iter()
        .take(SHOWN)
        .map(|p| format!("{} `{}`", p.pid, p.name))
        .collect();
    if processes.len() > SHOWN {
        names.push(format!("{} more", processes.len() - SHOWN));
    }
    let last = names.pop().unwrap_or_default();
    if names.is_empty() {
        last
    } else {
        format!("{} and {last}", names.join(", "))
    }
}

/// The processes holding a slot whose task's command exited: its process
/// group's members, or the group if none can be seen.
fn name_group(group: &process::ProcessGroup) -> String {
    let members = group.members();
    if members.is_empty() {
        format!("process group {}", group.pgid)
    } else {
        format!("process group {}: {}", group.pgid, name_processes(&members))
    }
}

/// The machine's CPUs busy since `before`, on average, in cores (one
/// decimal), whether inside the queue or not.
fn machine_busy(before: Option<process::CpuTicks>) -> Option<f64> {
    let share = process::cpu_ticks()?.busy_since(before?)?;
    let cores = process::online_cpus()? as f64;
    Some((share * cores * 10.0).round() / 10.0)
}

/// A note for a task that failed or was killed while the machine's CPUs were
/// nearly all busy: loadmaster cannot tell why it failed, but a check slowed
/// that much can run into a timeout of its own or of the caller.
fn saturated_note(record: &Record) -> Option<String> {
    let failed = record.signal.is_some() || record.ok == Some(false);
    let busy = record.machine_busy?;
    let cores = process::online_cpus()? as f64;
    (failed && busy >= report::SATURATED * cores).then(|| {
        format!(
            "loadmaster: note: the machine's CPUs were {:.0}% busy while `{}` ran ({busy:.1} \
             of {cores} cores, inside and outside the queue): a check slowed that much can run \
             into a timeout, so a failure may come from the load rather than the code",
            100.0 * busy / cores,
            record.task
        )
    })
}

/// A `loadmaster run` inside a task that already holds a slot: it runs
/// directly in that slot, since queueing for more from inside it could
/// deadlock. So it must come from the holder's checkout and fit in what the
/// holder holds: its weight and every resource it uses.
fn run_nested(value: &str, repo: &RepoConfig, task: &Task, mut command: Command) -> Result<i32> {
    let holder =
        holder_here(value, repo.path())?.map_err(|reason| Error::InvalidHeld { reason })?;
    let does_not_fit = |what: String, fix: String| {
        Err(Error::NestedDoesNotFit {
            reason: format!(
                "{what} job #{} (`{}`) that it would run in: {fix}, or run `{}` on its own",
                holder.id, holder.task, task.name
            ),
        })
    };
    // Capped at the capacity the holder's slot was granted under.
    let weight = task.weight.min(holder.capacity);
    if weight > holder.weight {
        return does_not_fit(
            format!(
                "`{}` has weight {weight}, more than the {} of",
                task.name, holder.weight
            ),
            format!("give `{}` a weight of at least {weight}", holder.task),
        );
    }
    for (name, &amount) in &task.uses {
        let held = holder
            .uses
            .iter()
            .find(|u| &u.name == name)
            .map_or(0, |u| u.amount);
        if amount > held {
            return does_not_fit(
                format!(
                    "`{}` uses {amount} of `{name}`, more than the {held} of",
                    task.name
                ),
                format!("add `{name} = {amount}` to the `uses` of `{}`", holder.task),
            );
        }
    }
    // It runs in the holder's group, which gets the holder's stop signal: one
    // it asks for of its own would never come.
    if task.stop_signal != libc::SIGTERM && task.stop_signal != holder.stop_signal {
        let (own, held) = (
            runner::signal_name(task.stop_signal),
            runner::signal_name(holder.stop_signal),
        );
        return Err(Error::NestedStopSignal {
            reason: format!(
                "`{}` stops on {own}, but job #{} (`{}`) that it would run in is stopped with \
                 {held}: set `stop_signal = \"{own}\"` for `{}`, or run `{}` on its own",
                task.name, holder.id, holder.task, holder.task, task.name
            ),
        });
    }
    let program = command.get_program().to_string_lossy().into_owned();
    let err = command.exec();
    say!("loadmaster: error: cannot run `{program}`: {err}");
    Ok(spawn_failure_code(&err))
}

/// Whether this process runs inside a job started from this checkout: the
/// check made for nested runs, plus the checkout. Silent when it does, so a
/// script can use it as a guard.
fn held(args: HeldArgs) -> Result<i32> {
    let cwd = env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
    let checkout = RepoConfig::locate(&cwd)?;
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
    let job = match holder_here(&value.to_string_lossy(), &checkout)? {
        Ok(job) => job,
        Err(reason) => return not_held(reason),
    };
    if let Some(missing) = args
        .uses
        .iter()
        .find(|name| !job.uses.iter().any(|u| &u.name == *name))
    {
        return not_held(format!(
            "job #{} (`{}`) does not use `{missing}`",
            job.id, job.task
        ));
    }
    Ok(0)
}

/// The job that `value` (a LOADMASTER_HELD) names, if it is running and was
/// started from the checkout of `config`, this checkout's .loadmaster.toml;
/// otherwise why not. Both `held` and nested runs check this.
fn holder_here(value: &str, config: &Path) -> Result<std::result::Result<Job, String>> {
    let (_, queue) = open_queue()?;
    let job = match value.trim().parse::<JobId>() {
        Ok(id) => queue.holder(id)?,
        Err(_) => None,
    };
    let Some(job) = job else {
        return Ok(Err(format!(
            "{HELD_ENV}={value} does not refer to a running job"
        )));
    };
    // A checkout is the resolved directory of its .loadmaster.toml, as for
    // per-checkout limits: a symlink to the same checkout matches, a worktree
    // whose config is a symlink to another's file does not.
    let here = config.parent().and_then(|dir| fs::canonicalize(dir).ok());
    if here.as_ref() != Some(&job.checkout) {
        return Ok(Err(format!(
            "job #{} (`{}`) was started from another checkout, in {}",
            job.id,
            job.task,
            job.dir.display()
        )));
    }
    Ok(Ok(job))
}

/// `$LOADMASTER_MAX_WAIT`, where empty means unset, as for LOADMASTER_HELD:
/// a script can turn the limit off for its steps by setting it to "".
fn max_wait_from_env() -> Result<Option<u64>> {
    let Some(value) = env::var_os(MAX_WAIT_ENV) else {
        return Ok(None);
    };
    let value = value.to_string_lossy().into_owned();
    if value.trim().is_empty() {
        return Ok(None);
    }
    value
        .trim()
        .parse()
        .map(Some)
        .map_err(|_| Error::InvalidEnv {
            var: MAX_WAIT_ENV,
            value,
            expected: "a whole number of seconds",
        })
}

fn spawn_failure_code(err: &io::Error) -> i32 {
    if err.kind() == io::ErrorKind::NotFound {
        EXIT_NOT_FOUND
    } else {
        EXIT_CANNOT_EXECUTE
    }
}

fn status(args: StatusArgs) -> Result<i32> {
    let machine = MachineConfig::load()?;
    let (state_dir, queue) = open_queue()?;
    let snapshot = queue.snapshot()?;
    // Each job adapts as the machine config it got in line with said.
    let adapting = machine.adapt_to_outside_cpu && !machine.measure_only;
    let outside = if adapting
        || snapshot
            .jobs
            .iter()
            .any(|job| job.state == JobState::Waiting && job.adapt_cpu)
    {
        queue.outside(&snapshot.jobs)?
    } else {
        None
    };
    if args.json {
        let explained = queue::explain(&snapshot.jobs, outside);
        let others = queue::running_in_other_queues(&state_dir);
        let value = json::status(&json::Status {
            machine: &machine,
            snapshot: &snapshot,
            outside: outside.filter(|_| adapting),
            resources: &resources_in_use(&snapshot.jobs, &machine),
            others: &others,
            explained: &explained,
        });
        print(&format!("{value:#}\n"));
        return Ok(0);
    }
    let mut out = format!(
        "{}/{} in use (capacity {})\n",
        snapshot.used(),
        machine.capacity,
        machine.capacity_source
    );
    for (name, used, capacity) in resources_in_use(&snapshot.jobs, &machine) {
        out += &match capacity {
            Some(capacity) => format!("{name}: {used}/{capacity} in use\n"),
            None => format!("{name}: {used} in use (no machine-wide limit)\n"),
        };
    }
    out += &load_line();
    if let Some(outside) = outside.filter(|_| adapting) {
        out += &format!(
            "CPU busy beyond what running jobs got: {:.1} of {} cores",
            outside.busy, outside.cores
        );
        let limit = outside.cpu_limit(machine.capacity);
        if limit < machine.capacity {
            out += &format!(", so jobs start with at most {limit} cores");
        }
        // The measurement cannot tell a job that uses more than it got from
        // load outside the queue; the history can, per task.
        if snapshot
            .jobs
            .iter()
            .any(|job| job.state == JobState::Running)
        {
            out += " (load outside the queue, or jobs using more than they got: see CORES in \
                    `loadmaster report`)";
        }
        out += "\n";
    }
    if let Some(others) = other_queues(&state_dir) {
        out += &format!("{others}\n");
    }
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
        out += &why_jobs_wait(&snapshot.jobs, outside);
    }
    print(&out);
    Ok(0)
}

/// The jobs that run in the queues of other loadmaster versions, which this
/// one does not count, if any.
fn other_queues(state_dir: &Path) -> Option<String> {
    let others = queue::running_in_other_queues(state_dir);
    let count: usize = others.iter().map(|other| other.running.len()).sum();
    if count == 0 {
        return None;
    }
    let list: Vec<String> = others
        .iter()
        .map(|other| {
            let tasks: Vec<String> = other.running.iter().map(|t| format!("`{t}`")).collect();
            format!("{}: {}", other.file, tasks.join(", "))
        })
        .collect();
    Some(format!(
        "{count} job(s) run in the queue of another loadmaster version, which this one does \
         not count ({})",
        list.join("; ")
    ))
}

/// The machine's load, which also counts work outside the queue.
fn load_line() -> String {
    let Some(load) = process::load_average() else {
        return String::new();
    };
    let Some(cores) = process::online_cpus() else {
        return format!("load average {load:.1} over the last minute\n");
    };
    let mut line = format!("load average {load:.1} over the last minute, on {cores} cores");
    if load > cores as f64 {
        line += ": more than the cores, often from work outside the queue or tasks that \
                 use more cores than their weight (see `loadmaster report`); on Linux, \
                 processes waiting for disks count too";
    }
    line + "\n"
}

/// One line per waiting job: what the admission rule says it waits for.
fn why_jobs_wait(jobs: &[Job], outside: Option<queue::Outside>) -> String {
    let lines: Vec<String> = queue::explain(jobs, outside)
        .into_iter()
        .filter_map(|(id, admission)| {
            let job = jobs.iter().find(|job| job.id == id)?;
            if job.state != JobState::Waiting || job.measure_only {
                return None;
            }
            let who = format!("#{id} `{}`", job.task);
            Some(match admission {
                Admission::Admitted => format!("  {who} is about to start\n"),
                Admission::Waiting(wait) => format!("  {who} waits for {}\n", wait.waits_for()),
            })
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!("\nwhy jobs wait:\n{}", lines.concat())
}

/// Each machine-wide resource that any job uses: how much running jobs use,
/// and the machine config's capacity, else the one jobs recorded.
fn resources_in_use<'j>(
    jobs: &'j [Job],
    machine: &MachineConfig,
) -> Vec<(&'j str, u64, Option<u32>)> {
    let mut resources: std::collections::BTreeMap<&str, (u64, Option<u32>)> =
        std::collections::BTreeMap::new();
    for job in jobs {
        for u in &job.uses {
            let entry = resources
                .entry(&u.name)
                .or_insert((0, machine.resources.get(&u.name).copied()));
            entry.1 = entry.1.or(u.capacity);
            if job.state == JobState::Running {
                entry.0 += u64::from(u.capacity.map_or(u.amount, |c| u.amount.min(c)));
            }
        }
    }
    resources
        .into_iter()
        .map(|(name, (used, capacity))| (name, used, capacity))
        .collect()
}

/// A job's state as `status` shows it, and since when it has been in it: a
/// running job whose loadmaster is gone is orphaned while its task's command
/// runs, and lingering once that exited and processes it left in its group
/// hold the slot.
fn shown_state(job: &Job) -> (&'static str, Option<SystemTime>) {
    match job.state {
        JobState::Running if job.is_orphaned() => match job.child {
            Some(group) if group.leader_exited() => ("lingering", job.started_at),
            _ => ("orphaned", job.started_at),
        },
        JobState::Running => ("running", job.started_at),
        JobState::Waiting => ("waiting", Some(job.enqueued_at)),
        JobState::Parked => ("parked", job.parked_at),
    }
}

fn jobs_table(jobs: &[Job]) -> String {
    let now = SystemTime::now();
    let since = |t: Option<SystemTime>| {
        t.and_then(|t| now.duration_since(t).ok())
            .map_or_else(|| "-".to_owned(), format_duration)
    };
    let mut rows = vec![
        [
            "ID", "STATE", "TASK", "WEIGHT", "USES", "PID", "PGID", "FOR", "DIR",
        ]
        .map(String::from),
    ];
    let (mut orphaned, mut parked) = (false, false);
    // Jobs whose task's command exited, and what holds their slot.
    let mut lingering = Vec::new();
    for job in jobs {
        let (state, at) = shown_state(job);
        match state {
            "lingering" => {
                if let Some(group) = job.child {
                    lingering.push(format!(
                        "  #{} `{}`: {}\n",
                        job.id,
                        job.task,
                        name_group(&group)
                    ));
                }
            }
            "orphaned" => orphaned = true,
            "parked" => parked = true,
            _ => {}
        }
        rows.push([
            job.id.to_string(),
            state.to_owned(),
            job.task.clone(),
            // "6 of 8": it started with fewer cores, for CPU busy outside.
            match job.granted.filter(|&granted| granted < job.weight) {
                Some(granted) => format!("{granted} of {}", job.weight),
                None => job.weight.to_string(),
            },
            if job.uses.is_empty() {
                "-".to_owned()
            } else {
                job.uses
                    .iter()
                    .map(|u| format!("{}={}", u.name, u.amount))
                    .collect::<Vec<_>>()
                    .join(",")
            },
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
        out += "\norphaned: loadmaster is gone but the task is still running; the slot is held \
                until its process group exits\n";
    }
    if !lingering.is_empty() {
        out += "\nlingering: the task's command exited, but processes it started in its process \
                group still run, and hold the slot until they exit (`loadmaster cancel ID` stops \
                them):\n";
        out += &lingering.concat();
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
    let (_, queue) = open_queue()?;
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

/// After this long, `cancel` says which jobs are still stopping.
const CANCEL_HINT_AFTER: Duration = Duration::from_secs(10);

/// The signal a cancel request stands for: the task's stop signal, or
/// SIGKILL.
fn cancel_signal(kill: bool, task: &Task) -> i32 {
    if kill {
        libc::SIGKILL
    } else {
        task.stop_signal
    }
}

fn cancel(args: CancelArgs) -> Result<i32> {
    let (_, queue) = open_queue()?;
    // The job this runs in, if any: canceling it would end this command.
    let own = env::var(HELD_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<JobId>().ok());
    let ids: Vec<JobId> = if args.here {
        let cwd =
            env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
        let config = RepoConfig::locate(&cwd)?;
        let here = config.parent().and_then(|dir| fs::canonicalize(dir).ok());
        queue
            .snapshot()?
            .jobs
            .into_iter()
            .filter(|job| here.as_ref() == Some(&job.checkout))
            .map(|job| job.id)
            .collect()
    } else {
        // Each once: a second SIGTERM would reach the task again.
        let mut named = std::collections::BTreeSet::new();
        args.ids
            .iter()
            .copied()
            .filter(|&id| named.insert(id))
            .collect()
    };
    let mut out = String::new();
    // Each job asked to stop, and what it was doing for how long.
    let mut stopping: Vec<(Job, String)> = Vec::new();
    for id in ids {
        if Some(id) == own {
            say!("loadmaster: not canceling job #{id}: this command runs in it");
            continue;
        }
        let job = match queue.cancel(id, args.kill)? {
            Canceled::NotFound if args.here => continue,
            Canceled::NotFound => {
                say!("loadmaster: job #{id} is not in the queue");
                continue;
            }
            Canceled::DroppedPlace(job) => {
                out += &format!("dropped the kept place of #{id} `{}`\n", job.task);
                continue;
            }
            Canceled::Requested(job) | Canceled::Signaled(job) => job,
        };
        let (what, since) = match job.state {
            JobState::Running => ("running", job.started_at),
            _ => ("waiting", Some(job.enqueued_at)),
        };
        let duration = since
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map_or(String::new(), |d| format!(" for {}", format_duration(d)));
        stopping.push((job, format!("{what}{duration}")));
    }
    if stopping.is_empty() && out.is_empty() {
        print("no jobs to cancel\n");
        return Ok(0);
    }
    // Gone once its loadmaster has left the line, or its task has exited and
    // freed the slot. The request in the queue records what was asked and
    // what was done, so nothing is sent twice (see `Queue::stop`). A job's
    // loadmaster acts on it; one stopped with Ctrl-Z acts on nothing, so it
    // is continued. A job whose loadmaster is gone has its task signalled.
    // A job can change meanwhile: one that gave up waiting (exit code 75)
    // keeps its place, which is dropped rather than left to expire, and a
    // re-run that resumes the place takes the request on.
    let started = Instant::now();
    let mut hinted = false;
    loop {
        let jobs = queue.snapshot()?.jobs;
        let left: Vec<&Job> = jobs
            .iter()
            .filter(|job| stopping.iter().any(|(stopped, _)| stopped.id == job.id))
            .collect();
        if left.is_empty() {
            break;
        }
        for job in &left {
            if job.state == JobState::Parked || job.is_orphaned() {
                queue.stop(job.id, args.kill)?;
            } else if let Some(owner) = job.owner.filter(ProcessId::is_stopped)
                && let Err(source) = owner.signal(libc::SIGCONT)
            {
                // Only if it still holds the job, stopped: since the
                // snapshot it may have parked the job, handed it over or
                // died, and the next look then stops the job without it.
                let jobs = queue.snapshot()?.jobs;
                if owner.is_stopped()
                    && jobs
                        .iter()
                        .any(|j| j.id == job.id && j.owner == Some(owner))
                {
                    return Err(Error::CannotSignal {
                        id: job.id,
                        why: format!(
                            "its loadmaster (process {}) is stopped, and could not be continued \
                             to act on the request",
                            owner.pid
                        ),
                        source,
                    });
                }
            }
        }
        if !hinted && started.elapsed() >= CANCEL_HINT_AFTER {
            hinted = true;
            for job in left {
                say!(
                    "loadmaster: #{} `{}` is still stopping{}",
                    job.id,
                    job.task,
                    if args.kill {
                        String::new()
                    } else {
                        format!("; `loadmaster cancel --kill {}` ends it at once", job.id)
                    }
                );
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
    for (job, what) in &stopping {
        out += &format!("canceled #{} `{}` ({what})\n", job.id, job.task);
    }
    print(&out);
    Ok(0)
}

fn check() -> Result<i32> {
    let cwd = env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
    let repo = RepoConfig::discover(&cwd)?;
    let machine = MachineConfig::load()?;
    print(&check::render(&repo, &machine));
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
    let since = args.since.map(|time| time.at(now));
    let until = args.until.map(|time| time.at(now));
    if let (Some(since), Some(until)) = (since, until)
        && until <= since
    {
        return Err(Error::Usage("--until must be after --since".to_owned()));
    }
    let options = report::Options {
        source: &path,
        since,
        until,
        dir: args.dir.as_deref(),
        unreadable,
        cpus: process::online_cpus(),
    };
    print(&report::render(&entries, &options));
    Ok(0)
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
