# Design decisions

The design decisions behind loadmaster, and why they were made.

## The task runs in its own process group

The task is spawned as the leader of a new process group, and the group's ID
and leader start time are recorded in the queue.

- A slot must stay held while the task is alive even if `loadmaster` itself is
  killed. That requires being able to name the task's processes as a unit,
  which a dedicated process group gives us.
- Agent tools commonly stop a command by signalling its whole process group:
  Claude Code, for example, sends SIGTERM and, if the command is still running
  a few seconds later, SIGKILL. With the task in a separate group, that SIGKILL
  can leave the task running after `loadmaster` dies. Keeping the slot held while
  the task's group lives makes the accounting stay true in that case: the work
  is still running, so the capacity is still used.
- Signals that `loadmaster` receives (SIGINT, SIGTERM, SIGHUP) are forwarded to
  the whole group. A process group is never in the terminal's foreground here,
  so this is the only way keyboard interrupts reach the task.

## A task is tracked before it can run

`Command::spawn` returns only after the child has exec'd, so recording the
process group afterwards would leave a window in which `loadmaster` can be
killed and the running task go untracked (and the window can last as long as
the database is busy). Instead, the child reports its PID over a pipe and then
blocks just before `exec` until a helper thread has recorded the group; only
then is it let through. If `loadmaster` dies first, the child sees the pipe
close and exits without running the task. If recording fails (the database
is unwritable, for example), the task is not run either and `loadmaster`
exits with 125: running it untracked would let its slot be freed while it
still runs. The child-side code uses only async-signal-safe calls (`close`,
`getpid`, `write`, `read`).

Gated spawns are serialized within a process. A child forked by one spawn
inherits the other spawn's gate descriptors until it execs, so two children
that both wait for their gate to close would keep each other's gate open
forever. The CLI spawns only once per process, but the library's own tests
deadlocked this way.

## Processes a task leaves behind keep its slot

When the task's main process exits but other processes in its group are still
running (a wrapper like `npm` that exits while its workers shut down, a
background job started by a `sh -c` script), `loadmaster` exits right away with
the main process's code, so the caller is not blocked, and hands the slot over
to the group: the owner is cleared and the slot is freed once the group has no
running member. This matches the rule "held while loadmaster or the task's
process group is alive". The cost is that a task that deliberately leaves a
daemon in its own group holds its slot until the daemon exits; `loadmaster`
says so on stderr, and `status` lists the job as orphaned. Most daemons
(Gradle, sccache, ...) start their own session and are not affected.

## A task gets no terminal input when stdin is a terminal

A background process group that reads from the terminal or changes its modes
is stopped by SIGTTIN/SIGTTOU, which would hang the task. Handing the terminal
to the task's group (as shells do) brings job control with it (Ctrl-Z, `fg`).
Tasks are batch jobs, so when stdin is a terminal the task gets `/dev/null`
instead; tools then behave as in CI (`npx` assumes `--yes`, watch modes lose
their keyboard shortcuts). Pipes and files on stdin are passed through.

A task that opens `/dev/tty` itself (a password prompt, a pager) is still
stopped. Every forwarded signal is followed by SIGCONT, so Ctrl-C still ends
such a task.

## Signals are forwarded no matter who sent them

Ideally a Ctrl-C that the terminal already delivered to the whole foreground
group would not be re-sent. On macOS, though, a terminal-generated SIGINT and a
`kill` from another process both arrive with `si_code == 0` and a non-zero
`si_pid` (measured), so there is no portable way to tell them apart. Since the
task lives in its own group, it does not receive terminal signals directly
anyway, so forwarding delivers each signal once in the common cases.

Signals that were already ignored when `loadmaster` started (SIGHUP under
`nohup`, SIGINT in a background job of a non-interactive script) are left
ignored, and the task inherits that. SIGCHLD is reset to its default action,
because an inherited "ignore" would make the task's exit status unobtainable.

Each intercepted signal has its own pending flag, so a SIGINT followed by a
SIGHUP within one poll forwards both. Signals that arrive just as the task's
main process exits, or while its slot is being released or handed over, are
still forwarded to the rest of its group. Before forwarding, the group is
checked by its leader's start time, so a group ID the OS has reused for
someone else after the task ended is not signalled. One theoretical gap
remains: if, after the leader was reaped, the group ID were reused by a new
group whose own leader had also already exited, that group's remaining
members would look like ours. That needs the PID space to wrap around within
milliseconds, so it is accepted rather than paid for with a more complex wait.

## Liveness

- A process counts as alive when a process with the same PID *and* start time
  exists. Zombies count as dead.
- On macOS the start time comes from `proc_pidinfo`, which takes about
  0.16 µs. It fails with EPERM for other users' processes, and a dead job's PID
  may since have gone to a root daemon, so EPERM falls back to
  `sysctl(KERN_PROC_PID)`, which works for any process but takes about 7 µs
  (measured). Using `sysctl` for everything made a 200-job stress test four
  times slower. The `kinfo_proc` prefix we read (`p_starttime` at offset 0,
  `p_stat` at offset 36) was checked against the system headers, and a test
  checks that both sources agree. On Linux it comes from `/proc/<pid>/stat`.
- A process group counts as alive while it has a member that is not a zombie,
  found by listing the members (`proc_listpgrppids` on macOS, a scan of `/proc`
  on Linux). `killpg(pgid, 0)` cannot tell on its own: Linux reports success
  for a group of zombies and macOS reports EPERM. It is still used as a fast
  path, because ESRCH reliably means "no process at all". Listing members keeps
  the answer correct even in a container whose PID 1 never reaps orphans.
- Members can fork and exit while they are being checked, so "no running
  member" is only accepted once two consecutive *complete* listings of zombies
  are identical: every member of the earlier listing was then already dead
  when the later one was taken, so anything they started would have shown up
  in it. Otherwise, after ten tries, the group counts as alive.
- A Linux `/proc` scan is not a snapshot. If any process vanished mid-scan or
  could not be read (a process that changed credentials), the listing is
  incomplete: that process could have been a member. With `/proc` mounted with
  `hidepid`, some processes are not listed at all, so every listing counts as
  incomplete. An empty
  listing of a group that `killpg` says exists is no evidence either. In both
  cases the group stays alive until a clean listing or ESRCH says otherwise.
- On Linux, a thread-group leader that called `pthread_exit` shows as a zombie
  while its other threads keep running; a zombie whose `num_threads` is above
  one counts as running. `/proc/<pid>/stat` is parsed as bytes, since process
  names need not be UTF-8.
- macOS's libproc reports failures as "0 entries" with errno set, so errno is
  cleared before the call and checked after it.
- A group whose leader PID now belongs to a different process is dead: the OS
  does not reuse a PID while a process group with that ID exists.
- When the OS cannot answer (an unexpected error), the job counts as alive, so
  a live job's slot is never handed to someone else.

## Exit codes

- 125 for loadmaster's own failures, 126/127 when the task's program cannot be
  executed or found. This follows `env`, `nice` and `timeout`, so a failing
  wrapper is distinguishable from a failing task in the common cases.
  Command-line usage errors also return 125 rather than clap's default 2,
  because 2 is a common exit code for linters.
- 75 (`EX_TEMPFAIL` from sysexits.h) for `--max-wait`: "try again later" is
  exactly what the caller should do.
- 128 + n when the task dies from signal n, and also when loadmaster itself is
  interrupted by signal n while waiting. loadmaster does not re-raise the
  signal on itself, so the caller always gets an ordinary exit code.
- 1 from `loadmaster held` for "not running through loadmaster", the way
  `test` and `grep` say "no", so a guard script can tell it from loadmaster
  failing (125, for example when there is no `.loadmaster.toml`).

## Resuming after `--max-wait`

- The resume key is (caller's working directory, task name, extra arguments).
  The directory distinguishes worktrees; arguments distinguish, for example,
  test shards. The directory is stored and compared as raw bytes; a lossy
  string could make two different directories look the same.
- A re-run resumes the place only if it asks for exactly what the parked job
  did: the same weight, resources and limits, from the same checkout. Jobs
  behind may have started next to what the parked job set aside, and would
  delay a re-run that needed more; one whose config changed in the meantime
  gets in line again instead.
- A parked job holds its place exactly like a waiting job: it sets aside what
  it needs, or holds back what it is short of. If parked jobs let others pass
  freely, a big job whose waits keep timing out could starve: small jobs would
  refill the capacity during each gap between its attempts.
- Parked jobs expire after 120 seconds. That comfortably covers an agent
  reading the exit code and re-running the command, while limiting how long an
  abandoned place can block the line.
- `LOADMASTER_MAX_WAIT` sets the default, so a Claude Code `env` setting can
  apply it to every call without the agents having to remember the flag. An
  empty value means unset, as for `LOADMASTER_HELD`, so a script can turn the
  limit off for the checks it runs by setting it to "" (clap's own `env`
  support would reject the empty value).
- `--max-wait` takes whole seconds and is a soft limit: it is checked after
  each poll, and a job that finds room on the poll that crosses the limit
  starts instead of giving up, since running is what the caller wanted and
  giving up would only make it re-run the command. That poll is normally at
  most 100 ms late, but can take longer while the database is busy.

## Opening the queue

Switching a new database to WAL mode takes an exclusive lock that SQLite tries
to get once, without consulting the busy timeout. Five agents starting at the
same moment on a fresh state directory occasionally got "database is locked"
(found in the real-tools demo, about 1 run in 300). The switch is retried for
up to the busy timeout; everything else relies on SQLite's busy handler.

The schema version lives in `PRAGMA user_version`. A database from an older
version is upgraded in place (version 1 gains each job's capacity and mode);
an older `loadmaster` refuses a newer database with an error instead of
guessing.

## Job IDs are never reused

The `jobs` table uses `INTEGER PRIMARY KEY AUTOINCREMENT`. A plain
`INTEGER PRIMARY KEY` would also preserve arrival order, but it can hand out an
old ID again once the table empties, and then a leaked `LOADMASTER_HELD` could
validate against an unrelated new job.

## Resources

CPU is not the only thing that runs out. An e2e suite may need one of a few
local database stacks, a linter may need 11 GiB of memory, and two e2e runs in
one checkout collide on its ports and database. A weight sized for one of
those makes the CPU accounting wrong (the earlier advice was a weight above
half the capacity, to keep two runs apart), so tasks can also use named
resources.

- A resource is declared in `.loadmaster.toml` under `[resources.NAME]`, with
  an optional machine-wide `capacity` and an optional `per_checkout` limit,
  and a task lists what it uses: `uses = { NAME = amount }`. Every name a task
  uses must be declared, so a typo fails loudly, as unknown keys do.
- The machine config can set or override a capacity under `[resources]`. The
  repository's value is a default: a count such as "two e2e database stacks"
  belongs to the project and suits most machines, and a repository's config
  should work without setting up each machine. A resource whose size the
  repository cannot know, such as memory, is declared without a capacity and
  set per machine. A resource with no capacity anywhere is not limited
  machine-wide.
- Resource names are shared by all repositories on the machine, like the
  capacity. A job that records no capacity for a resource still counts toward
  it. When repositories disagree on a capacity, each job is admitted by its
  own record (bounded by the jobs ahead of it), so set a capacity they should
  all share in the machine config.
- A per-checkout limit counts only jobs started from the same checkout: the
  directory containing `.loadmaster.toml`, resolved, so a worktree whose
  `.loadmaster.toml` is a symlink to another's file is still a checkout of its
  own. It describes the project (one database per checkout), so the machine
  config cannot change it.
- An amount above a limit counts as the whole limit, like a weight above the
  capacity.
- Each job records the limits it got in line with, and every process judges
  each job by that record. A process from another repository, which may not
  know a resource at all, can still tell whether a job fits.

**Admission.** The line stays first come, first served, but a job waiting in
it holds back only what it is short of. Jobs are considered in line order, on
every dimension at once (CPU, each machine-wide resource, each per-checkout
resource of each checkout):

- A job fits if, on every dimension, what running jobs use plus what is set
  aside for jobs ahead of it plus its own amount is within the limit, and it
  uses nothing that a job ahead of it is short of. A job that fits may start.
- A job that does not fit is short of some dimensions: no job behind it may
  use those at all. On the dimensions where it fits, its amount is set aside,
  so jobs behind it can only use what is left.
- A job ahead that fits but has not started yet (its own poll comes later)
  has its whole amount set aside.

A job behind can therefore start first only by using nothing the waiting job
is short of and leaving it its share everywhere else, so no job is ever
delayed by jobs that got in line after it. Every task uses CPU, so a job
waiting for CPU still holds back everyone behind it: with CPU alone, no job
starts while one ahead of it is short of CPU, as in the strict line of the
first version. (One difference is for the better: a parked job that fits used
to block everyone until it was resumed or expired, and now only sets aside its
weight.) Without the rule, a third e2e run waiting
for a database stack would hold up every lint behind it, though no lint needs
a stack.

The rule does not know which running job will free what, so where in doubt it
sets aside. A job waiting for its checkout's limit still sets aside its
machine-wide amount, so a job from another checkout may wait for a stack that
is free. That takes two jobs of one checkout in line for the same resource,
which one agent per worktree rarely causes. Skipping that set-aside when the
release the job waits for would free enough anyway was considered; it breaks
the guarantee when a waiting job leaves the line instead.

A unit test simulates random workloads with several resources and checks that
each job starts at the same time whether or not the jobs that got in line
after it exist. The soak test adds resources and a second checkout and checks
every limit from inside the tasks.

Nested runs still skip every check, so an outer task must declare what its
nested runs use.

## Checking that a script runs through the queue

`loadmaster held` lets a script refuse to run outside the queue, whatever
started it. Permission rules and hooks only see an agent's own commands, and
not every agent has them.

- It accepts what a nested run accepts, a `LOADMASTER_HELD` that names a
  running job, and also requires that job to have been started from the same
  checkout, in the sense of per-checkout limits: the resolved directory of the
  `.loadmaster.toml` found from the current directory. Otherwise a value that
  leaked into another worktree would pass. The file is only located, not
  parsed, so a config edited while the job runs makes no difference.
- It does not check the task's name. A guarded script can be run by several
  tasks, and by a nested run, whose job is the outer task's. What such a
  script depends on is a resource: `--uses NAME` also requires the job to use
  NAME.
- It does not check that the caller descends from the task's process. Tools
  start helpers in process groups of their own, and orphaned processes are
  re-parented, so such a check would refuse legitimate runs. A value copied
  into another shell of the same checkout therefore passes while its job runs,
  as it does for nested runs. Like the hook, this is a guardrail against
  accidents, not a security boundary.
- It prints nothing when the check passes, and the reason and how to run the
  task through loadmaster when it fails.

## Where things run and live

- Tasks run in the directory containing `.loadmaster.toml`, so `cmd` can
  always be written relative to the repository root.
- Config and state follow the XDG layout on macOS as well
  (`~/.config/loadmaster/config.toml`, `~/.local/state/loadmaster/`), because
  that is where a CLI user expects them and what the documentation shows.
  `LOADMASTER_CONFIG` and `LOADMASTER_STATE_DIR` override them (the tests use
  this for isolation). Relative overrides are resolved against the current
  directory, and the resolved state directory is passed on to the task, so a
  nested run finds the same queue even though the task runs elsewhere.
- The state directory is created with mode 0700: it holds command lines and
  paths.

## Polling

Waiting jobs poll the queue every 100 ms; that bounds the extra latency of each
admission. A poll first reads the queue without the write lock and only takes
it (`BEGIN IMMEDIATE`) when something can change: the job is at the head and
fits, or a job that can affect its decision (a running one, or one ahead of
it) is dead and must be reaped. Otherwise every waiter would take the write
lock ten times a second and compete with the writes that matter, such as
recording a task that is held at its gate. The decision is always made again
under the lock before admitting. `Queue::open` likewise checks the schema
version without the lock. A running task is polled for exit every 20 ms. The waiting message is
printed when a job starts waiting and then every 10 seconds: often enough to
show progress, rarely enough not to flood an agent's context.

Only the head of the line can be admitted, and the new head notices free
capacity on its own next poll, so a backlog drains at roughly one admission per
poll. That is negligible for tasks that run for seconds or minutes, but visible
with tiny ones: 200 simultaneous 50 ms jobs took 16 s on a 14-core Mac (zero
capacity violations). Admitting the whole fitting prefix of the line in one
transaction would remove this cost; it is left out until it matters.

## Configuration details

- `cmd` must be an array. A string `cmd` is rejected with a hint showing the
  array form and the `["sh", "-c", ...]` escape hatch.
- Unknown keys are errors (`deny_unknown_fields`), so typos such as `wieght`
  fail loudly instead of being ignored.
- Each process reads the capacity when it starts; a changed capacity applies to
  new runs. Weights are summed as 64-bit integers, so even `capacity =
  4294967295` cannot overflow the admission check.
- Nested runs skip the capacity check and inherit the holder's
  `LOADMASTER_WEIGHT`: they run inside the holder's slot.
- A weight above the capacity counts as the whole capacity, and the task runs
  alone. Weights are committed with the repository while the capacity belongs
  to each machine, so a repository's weights have to work on smaller machines
  too (a CI runner, a laptop); rejecting them, as the first version did, made
  the shared config unusable there. A note on stderr still points out a typo
  such as `weight = 80`. The capped weight is what the job records, what
  `LOADMASTER_WEIGHT` says and what the history log shows: it is the slot the
  task really held.

## History log

`history.jsonl` in the state directory gets one JSON object per queued run:
job ID, task, weight, capacity, whether the machine was in measure-only mode,
caller directory, arguments, outcome (`exited`, `gave_up`, `interrupted`,
`spawn_failed`, `error`, `lost`), queue time, wait and run durations, CPU
time, the 1-minute load average when the task ended, the signal that killed
the task, if any, and the exit code `loadmaster run` returned. Every run that
got in line writes exactly one line, whatever happened afterwards: a
`loadmaster` killed with SIGKILL cannot write its own, so the process that
reaps its job writes a `lost` line for it, after the reaping is committed.
That needs the capacity and mode of the job, which are therefore kept in the
queue database. JSON Lines is easy to analyze with `jq`. It is not rotated.
Fields added later are optional when read back, so older logs still work with
`loadmaster report`.

Known gaps, each needing an unusual combination of events, accepted rather
than paid for with per-attempt IDs and logging inside the queue's
transactions:

- A `loadmaster` killed right after removing, parking or handing over its job
  but before writing its line leaves no line.
- If releasing or handing over a finished job fails (the database stayed
  locked for the whole 60-second busy timeout), `loadmaster` warns, exits with
  the task's code and logs its result, and the job is later also logged as
  `lost`. The slot stays held until the task's processes are gone either way.
- A run that gave up can, in theory, append its `gave_up` line after a re-run
  that resumed its place has already finished; `report` then counts two
  checks.
- While a `loadmaster` from before `lost` logging is still running during an
  upgrade, it can reap jobs without logging them.

## Measuring the effect

Whether admission helps depends on the checks and on how often agents overlap,
so users need a way to find out from their own use.

- **Measure-only mode** admits every job at once, ignoring both the capacity
  and the line (a place kept by `--max-wait` from before the switch would
  otherwise still block it), but keeps everything else: jobs are still in the
  queue (and in `status`) and logged. This gives a baseline that differs from
  admission in one thing only, with the same agent instructions, permissions
  and hooks. Simply not using loadmaster would leave nothing to compare with.
  Weights above the capacity are capped the same way in both modes, so both
  periods run with the same configuration.
- **A check is one request.** A re-run after exit code 75 resumes the same job,
  so `report` joins all records of a job ID until one ends in something other
  than `gave_up`. Its wait runs from the first time it got in line to the
  start, which includes the agent's own delay before re-running: that delay is
  part of what admission costs. Job IDs restart if the database is deleted,
  so a record only joins a kept place if it has the queue's resume key (caller
  directory, task, arguments) and starts within the place's lifetime of giving
  up (120 seconds, plus two minutes for a busy database and clocks). A `lost`
  record carries the time the job first got in line, so it may start earlier
  than the attempts it joins.
- **Results**: a task killed by a signal (recorded from its exit status), a
  signal while waiting and a `lost` run are killed (usually a tool timeout);
  exit code 0 is passed and any other code failed, so a task that exits with
  143 on its own is a failure. Records without a `signal` field (from before
  it was logged; `null` means "not killed") fall back to treating 129–192 as
  killed. A place given up and never resumed, a
  failed spawn and a loadmaster error are not run. A wait that never ended in
  a start still counts, up to when it ended, so waits that ran into a timeout
  are not hidden.
- **Demand** replays the checks as if each had started the moment it got in
  line, with its real run time. Runs under contention would have been longer,
  so this underestimates the overload loadmaster prevented; it still shows
  whether the agents overlap at all. The measure-only section uses the real
  start times instead. Only checks whose run time is known are replayed, and
  each counts until its main process exited: a slot kept by processes a task
  left behind, or held by a `lost` run, is not counted. Run time and CPU time
  are taken as soon as the task exits, before its slot is released, since
  releasing it may wait for the database.
- **Grouping**: tasks are grouped by name, across repositories, since
  worktrees of one project have unrelated paths. `--dir TEXT` keeps only
  checks run from directories whose path contains TEXT, which usually picks
  out one project with all its worktrees.
- **CPU time** is the task's `RUSAGE_CHILDREN` delta, which covers the task and
  every descendant that was waited for. loadmaster has one child, so nothing
  else is mixed in. Processes a task leaves behind are not counted.
- **Load average** is the system-wide 1-minute average: it lags and includes
  everything else on the machine, so it is only a rough signal. It is recorded
  because "the machine is sluggish" is part of the problem, and nothing else
  measures it.

## Messages are in English

loadmaster is meant to be published as open source.
