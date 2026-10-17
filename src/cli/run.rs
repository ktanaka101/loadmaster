//! `loadmaster run`: getting in line, waiting, running the task, and runs
//! nested in a job.

use crate::cli::cancel::cancel_signal;
use crate::cli::held::holder_here;
use crate::cli::words::{self, who};
use crate::*;
use loadmaster::usual::Usual;

/// A line at least this often while waiting: some agents' tools end a
/// command that has said nothing for a while (Gemini CLI's shell, after 5
/// minutes by default).
const HEARTBEAT: Duration = Duration::from_secs(60);

/// What a job waits for can change often, as short jobs ahead come and go:
/// a change is said at most this often.
const CHANGE_GAP: Duration = Duration::from_secs(15);

pub(crate) fn run(args: RunArgs) -> Result<i32> {
    let cwd = env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
    let repo = RepoConfig::discover(&cwd)?;
    let task = repo.task(&args.task)?;
    let command = runner::command(&task.cmd, &args.args, repo.dir());
    let max_wait = match args.max_wait {
        Some(secs) => Some(secs),
        None => max_wait_from_env()?,
    }
    .map(Duration::from_secs);

    if let Some(held) = env::var_os(HELD_ENV).filter(|v| !v.is_empty()) {
        return run_nested(&held.to_string_lossy(), &repo, task, command);
    }

    let machine = MachineConfig::load()?;
    // Weights belong to the repository and the capacity to the machine, so a
    // weight, or a min_weight, can exceed a small machine's capacity: each
    // then counts as all of it.
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
    let min_weight = task.min_weight.unwrap_or(task.weight).min(task.weight);
    let uses = resource_uses(&repo, task, &machine);
    // Resolved, so every worktree path that leads here is one checkout.
    let checkout = fs::canonicalize(repo.dir()).unwrap_or_else(|_| repo.dir().to_owned());

    let signals = Signals::install().map_err(|e| Error::io("cannot install signal handlers", e))?;
    let (state_dir, queue) = open_queue()?;
    if !machine.measure_only
        && let Some(others) = other_queues(&state_dir)
    {
        say!("loadmaster: warning: {others}; give every agent on this machine 0.12 or later");
    }
    let owner = ProcessId::current().map_err(|e| Error::io("cannot identify this process", e))?;
    let display = display_command(&task.cmd, &args.args);
    // Both clocks start here, so wait times include getting in line, and
    // `--max-wait` is one budget for both.
    let (queued_at, wait_started) = (SystemTime::now(), Instant::now());
    let request = JobRequest {
        task: &task.name,
        weight: task.weight,
        min_weight,
        dir: &cwd,
        args: &args.args,
        command: &display,
        capacity: machine.capacity,
        measure_only: machine.measure_only,
        uses: &uses,
        checkout: &checkout,
        adapt_cpu: machine.adapt_to_outside_cpu,
        stop_signal: task.stop_signal,
        asked_at: queued_at,
    };
    let rerun = words::rerun(&task.name, args.max_wait, &args.args);
    let mut ticket = match enqueue_once_moved(
        &queue,
        &request,
        owner,
        task,
        (wait_started, max_wait),
        &signals,
        (&rerun, &cwd),
    )? {
        Ok(ticket) => ticket,
        Err(code) => return Ok(code),
    };
    if ticket.resumed() {
        tell!(
            "loadmaster: {} resumed its place in line",
            who(ticket.id(), &task.name)
        );
    }

    let mut record = Record {
        job: ticket.id(),
        task: &task.name,
        weight: task.weight,
        capacity: machine.capacity,
        measure_only: machine.measure_only,
        dir: cwd.to_string_lossy(),
        checkout: Some(checkout.to_string_lossy()),
        workload: Some(config::workload(&checkout)),
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
        min_weight: (min_weight < task.weight).then_some(min_weight),
        uses: task.uses.clone(),
        limits: queue::limits(&uses),
    };
    // Set once this run's line is written.
    let mut logged = false;
    let log = |record: &Record| {
        if let Err(e) = history::append(&state_dir, record) {
            say!("loadmaster: warning: cannot write the history log: {e}");
        }
    };
    let result = (|| {
        let turn = if machine.measure_only {
            // Admitted whatever the capacity and place in line: the run
            // behaves as without loadmaster, and is only recorded.
            ticket.admit_now().map(|()| Turn::Admitted { said: false })
        } else {
            wait_turn(
                (&queue, &state_dir),
                &ticket,
                task,
                (wait_started, max_wait),
                &signals,
            )
        };
        record.wait_ms = millis(wait_started.elapsed());
        match turn? {
            Turn::Admitted { said } => run_admitted(
                ticket,
                &state_dir,
                command,
                task,
                &signals,
                (wait_started, said),
                &mut record,
            ),
            Turn::GaveUp(wait) => {
                // A process admitting jobs may have started it meanwhile:
                // then it runs. Logged once parked, so that a run that went
                // on to start never says it gave up: killed in between, this
                // run leaves no line (killed before parking, it is reaped and
                // logged as lost).
                if !ticket.park()? {
                    return run_admitted(
                        ticket,
                        &state_dir,
                        command,
                        task,
                        &signals,
                        (wait_started, true),
                        &mut record,
                    );
                }
                record.outcome = Outcome::GaveUp;
                record.exit_code = Some(EXIT_GAVE_UP);
                log(&record);
                logged = true;
                say!(
                    "loadmaster: {} gave up after waiting {} ({}) {} (exit {EXIT_GAVE_UP})",
                    who(record.job, &task.name),
                    format_duration(wait_started.elapsed()),
                    format_ahead(wait.ahead),
                    words::waits_for(&wait, None, &Usual::default()),
                );
                say!(
                    "loadmaster: next: run `{rerun}` again from {} within {}s: it resumes its \
                     place in line",
                    cwd.display(),
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
            let ran = record.run_ms.map(Duration::from_millis);
            let line = words::ended(&record, *code, ran);
            if !line.is_empty() {
                tell!("loadmaster: {line}");
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
    /// Its turn came; `said` if it said it waited.
    Admitted {
        said: bool,
    },
    GaveUp(Wait),
    Interrupted(i32),
}

/// Gets in line, once the queue has moved to this version's admission
/// protocol: while jobs of 0.12 to 0.14 finish in it, nothing gets in (see
/// [`Error::QueueMoving`]). Gives up as `--max-wait` says, counted from
/// `started`, or on a signal, with the exit code to end with: no place was
/// kept, so `rerun`, from `dir`, gets in line anew.
fn enqueue_once_moved<'q>(
    queue: &'q Queue,
    request: &JobRequest,
    owner: ProcessId,
    task: &Task,
    (started, max_wait): (Instant, Option<Duration>),
    signals: &Signals,
    (rerun, dir): (&str, &Path),
) -> Result<std::result::Result<Ticket<'q>, i32>> {
    let mut said: Option<Instant> = None;
    loop {
        match queue.enqueue(request, owner) {
            Err(Error::QueueMoving { left }) => {
                if let Some(&signal) = signals.take().first() {
                    return Ok(Err(128 + signal));
                }
                if max_wait.is_some_and(|max| started.elapsed() >= max) {
                    say!(
                        "loadmaster: `{}` gave up after {}, not yet in line: the queue is still \
                         moving to the admission protocol of 0.15 ({left} job(s) of loadmaster \
                         0.12 to 0.14 left) (exit {EXIT_GAVE_UP}); no place in line was kept",
                        task.name,
                        format_duration(started.elapsed())
                    );
                    say!(
                        "loadmaster: next: run `{rerun}` again from {}: it gets in line anew \
                         once the queue has moved",
                        dir.display()
                    );
                    return Ok(Err(EXIT_GAVE_UP));
                }
                if said.is_none_or(|at| at.elapsed() >= HEARTBEAT) {
                    tell!(
                        "loadmaster: `{}` is waiting to get in line: the queue is moving to the \
                         admission protocol of 0.15, once the {left} job(s) of loadmaster 0.12 \
                         to 0.14 in it are done (give every agent 0.15 or later)",
                        task.name
                    );
                    said = Some(Instant::now());
                }
                thread::sleep(POLL_INTERVAL);
            }
            other => return other.map(Ok),
        }
    }
}

/// Waits for `ticket`'s turn, until `--max-wait` has passed since `started`.
/// Says so when it starts waiting, when what it waits for or who holds it
/// changes, and at least every [`HEARTBEAT`]: with who holds what it waits
/// for, for how long, and how long they usually run (from the history in
/// `state_dir`).
fn wait_turn(
    (queue, state_dir): (&Queue, &Path),
    ticket: &Ticket,
    task: &Task,
    (started, max_wait): (Instant, Option<Duration>),
    signals: &Signals,
) -> Result<Turn> {
    let who = who(ticket.id(), &task.name);
    // When the last line was said, and what about.
    let mut said: Option<(Instant, String)> = None;
    let mut usual: Option<Usual> = None;
    loop {
        if let Some(&signal) = signals.take().first() {
            return Ok(Turn::Interrupted(signal));
        }
        if let Some(kill) = ticket.cancel_requested()? {
            say!("loadmaster: {who} was canceled with `loadmaster cancel` while it waited");
            return Ok(Turn::Interrupted(cancel_signal(kill, task)));
        }
        match ticket.try_admit()? {
            Admission::Admitted => {
                return Ok(Turn::Admitted {
                    said: said.is_some(),
                });
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
                let about = words::wait_key(&wait);
                let due = said.as_ref().is_none_or(|(at, last)| {
                    at.elapsed() >= HEARTBEAT || (*last != about && at.elapsed() >= CHANGE_GAP)
                });
                if due {
                    let usual = usual.get_or_insert_with(|| Usual::load(state_dir));
                    // For who holds it; without, the line still says what.
                    let snapshot = queue.snapshot().ok();
                    let what = words::waits_for(&wait, snapshot.as_ref(), usual);
                    let state = match said {
                        None => "is waiting".to_owned(),
                        Some(_) => format!("has waited {}", format_duration(started.elapsed())),
                    };
                    tell!(
                        "loadmaster: {who} {state} (weight {}, {}) {what}",
                        task.weight,
                        format_ahead(wait.ahead),
                    );
                    said = Some((Instant::now(), about));
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
    (wait_started, said): (Instant, bool),
    record: &mut Record,
) -> Result<i32> {
    if let Some(&signal) = signals.take().first() {
        record.outcome = Outcome::Interrupted;
        return Ok(128 + signal);
    }
    let who = who(ticket.id(), &task.name);
    if let Some(kill) = ticket.cancel_requested()? {
        say!("loadmaster: {who} was canceled with `loadmaster cancel`");
        record.outcome = Outcome::Interrupted;
        return Ok(128 + cancel_signal(kill, task));
    }
    // Fewer cores than its weight, which it may start with (its min_weight):
    // the task sizes its parallelism by them.
    let granted = ticket.granted()?;
    let mut start = format!("loadmaster: {who} started");
    if granted < task.weight {
        start += &format!(
            " with {granted} of its {} cores, the most that fit now ({WEIGHT_ENV}={granted})",
            task.weight
        );
        record.granted = Some(granted);
    }
    // Said whenever it said it waited, so the lines agree.
    let waited = wait_started.elapsed();
    if said || waited >= Duration::from_secs(1) {
        start += &format!(" after waiting {}", format_duration(waited));
    }
    tell!("{start}");
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
            "loadmaster: {who} was canceled with `loadmaster cancel`; sending it {}",
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
pub(crate) fn name_group(group: &process::ProcessGroup) -> String {
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
    // Capped at the capacity the holder's slot was granted under, as on its
    // own, and against what the holder got: it may have started with fewer
    // cores than its weight. A task that may start with fewer gets the
    // holder's.
    let weight = task.weight.min(holder.capacity);
    let least = task.min_weight.unwrap_or(weight).min(weight);
    let granted = holder.granted.unwrap_or(holder.weight);
    if least > granted {
        let (what, fix) = if granted < holder.weight {
            (
                format!(
                    "the {granted} cores (of its weight {}) that started",
                    holder.weight
                ),
                format!("give `{}` a min_weight of at most {granted}", task.name),
            )
        } else {
            (
                format!("the {granted} of"),
                format!("give `{}` a weight of at least {least}", holder.task),
            )
        };
        let needs = if least < weight {
            format!("needs at least {least} cores")
        } else {
            format!("has weight {weight}")
        };
        return does_not_fit(format!("`{}` {needs}, more than {what}", task.name), fix);
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

/// `$LOADMASTER_MAX_WAIT`, where empty means unset, as for LOADMASTER_HELD:
/// a script can turn the limit off for its steps by setting it to "".
pub(crate) fn max_wait_from_env() -> Result<Option<u64>> {
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
