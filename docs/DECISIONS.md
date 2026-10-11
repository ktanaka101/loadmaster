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

## Resuming after `--max-wait`

- The resume key is (caller's working directory, task name, extra arguments).
  The directory distinguishes worktrees; arguments distinguish, for example,
  test shards. The directory is stored and compared as raw bytes; a lossy
  string could make two different directories look the same. The task's weight and command are taken from the current config
  when resuming.
- A parked job keeps blocking the head of the line, exactly like a waiting job.
  If parked jobs let others pass, a big job whose waits keep timing out could
  starve: small jobs would refill the capacity during each gap between its
  attempts.
- Parked jobs expire after 120 seconds. That comfortably covers an agent
  reading the exit code and re-running the command, while limiting how long an
  abandoned place can block the line.
- `LOADMASTER_MAX_WAIT` sets the default, so a Claude Code `env` setting can
  apply it to every call without the agents having to remember the flag.
- `--max-wait` takes whole seconds and is a soft limit: it is checked after
  each poll, and a job that finds room on the poll that crosses the limit
  starts instead of giving up, since running is what the caller wanted and
  giving up would only make it re-run the command. That poll is normally at
  most 100 ms late, but can take longer while the database is busy.

## Job IDs are never reused

The `jobs` table uses `INTEGER PRIMARY KEY AUTOINCREMENT`. A plain
`INTEGER PRIMARY KEY` would also preserve arrival order, but it can hand out an
old ID again once the table empties, and then a leaked `LOADMASTER_HELD` could
validate against an unrelated new job.

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

## History log

`history.jsonl` in the state directory gets one JSON object per queued run:
job ID, task, weight, caller directory, arguments, outcome (`exited`,
`gave_up`, `interrupted`, `spawn_failed`, `error`), queue time, wait and run
durations, and the exit code `loadmaster run` returned. Every run that got in
line writes exactly one line, whatever happened afterwards. JSON Lines is easy
to analyze with `jq`. It is not rotated.

## Messages are in English

loadmaster is meant to be published as open source.
