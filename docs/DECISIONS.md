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
  the whole group (a SIGTERM as the task's stop signal; see
  [Signals are forwarded no matter who sent them](#signals-are-forwarded-no-matter-who-sent-them)). A process group is never in the terminal's foreground here,
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
says so on stderr, naming the processes in the group, and `status` lists the
job as lingering, with them. A job is orphaned only while its task's command
still runs without its loadmaster: the two used to share that name, and with
it the explanation that loadmaster was gone, which did not say what held the
slot of a job whose command had exited (clew, 0.10.0). Most daemons
(Gradle, sccache, ...) start their own session and are not affected.

Processes that start a session of their own leave the group, and with it
everything loadmaster does for the task: they get no signals and hold no
slot. Some are meant to (a daemon); others are left behind by accident, as
the `webServer` a Playwright run leaves when it ends on SIGTERM, which keeps
its port and may serve the next run stale code without an error. loadmaster
warns of them: it walks the task's process tree about once a second while
the task runs, and just before each signal it sends the task (and from the
processes found before, since an orphan belongs to PID 1), and when the task
exits, names those still running outside its group, after half a second for
them to go with it. The look before a signal is for the process started
since the last look, which only its parent leads to, when the signal ends
that parent: a Playwright run canceled a moment after it started its
`webServer` exited on SIGTERM before the next look, and the server it left
was not named (clew, 0.9.0). A look over 100 processes
takes about a millisecond (macOS and Linux, measured), during which signals
wait. Each process is checked by its start time and each child by its
parent, so a PID handed out again meanwhile is not taken for one of them. It does not stop them or
hold a slot for them, which would break the daemons. One whose parent exits
on its own between two looks is missed: neither OS keeps a record of where a process
came from, and macOS does not show another process's environment, which
would carry the job's `LOADMASTER_HELD` (measured). Linux's child subreaper
would catch every one, but makes loadmaster reap every orphan, and a macOS
equivalent does not exist.

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

Every process in the task's group gets the signal, as every process in a
terminal's foreground group gets Ctrl-C. Tools are written for that, and the
group, not one process, is what has to stop. A wrapper that also forwards
signals to its child (one that reads an env file and starts the real tool,
say) makes the child get each signal twice, as it would in a terminal; the
README says so, and that the wrapper should `exec` the child.

A SIGTERM asks loadmaster to stop the task, as a cancel does, and reaches the
task as its stop signal: SIGTERM unless the task sets `stop_signal`. Tools
differ in the signal they clean up on: Playwright exits at once on SIGTERM,
leaving its `webServer` running, and shuts it down on SIGINT; Docker's
`STOPSIGNAL` and Compose's `stop_signal` exist for the same reason. The
SIGTERM of an agent's timeout is such a request, so it is converted too, as
`dumb-init --rewrite` would. SIGINT is not the default: commands that a
non-interactive shell starts in the background ignore it.

The stop signal reaches the task once: a further request, from whatever
source, is not passed on (see [Canceling jobs](#canceling-jobs)). SIGINT and
SIGHUP are forwarded as they came, each time, since tasks commonly act on a
second Ctrl-C or carry on after SIGHUP; one that is the stop signal counts as
the stop, so a cancel after a Ctrl-C sends nothing more. The history log
records that the task was asked to stop, and the report counts a run that
then exited with a code of its own (Playwright exits with 130) as stopped,
like one a signal ended, not as failed.

Signals that were already ignored when `loadmaster` started (SIGHUP under
`nohup`, SIGINT in a background job of a non-interactive script) are left
ignored, and the task inherits that, except its stop signal: that is how
loadmaster asks it to stop, so the task gets its default action. A task
stopped with SIGINT, run by an agent in the background (`loadmaster run e2e
&`), would otherwise ignore every request to stop, and a shell cannot even
trap a signal it starts with ignored. SIGCHLD is reset to its default action,
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
  This happens on macOS too: right after a group is killed with SIGKILL,
  about one kill in three had an instant in which `killpg` found the group
  but `proc_listpgrppids` listed nobody (measured; it lasted one sample, and
  the next try listed the zombies).
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
  did: the same weight and `min_weight`, resources and limits, from the same
  checkout. Jobs
  behind may have started beside what the parked job would start with, and
  would delay a re-run that needed more; one whose config changed in the
  meantime gets in line again instead.
- A parked job holds its place exactly like a waiting job: every job behind
  it leaves room for its start. If parked jobs let others pass
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
  giving up would only make it re-run the command. So does one that another
  loadmaster started meanwhile (see [Polling](#polling)). That poll is normally at
  most 100 ms late, but can take longer while the database is busy.

## Opening the queue

Switching a new database to WAL mode takes an exclusive lock that SQLite tries
to get once, without consulting the busy timeout. Five agents starting at the
same moment on a fresh state directory occasionally got "database is locked"
(found in the real-tools demo, about 1 run in 300). The switch is retried for
up to the busy timeout; everything else relies on SQLite's busy handler.

## The queue file

One machine has one place where admission is decided. Until 0.11 the queue's
file name carried the version of its layout and rule (`queue-v3.db` and
before), and a version that changed either started a new file: during an
upgrade, or for as long as an agent ran an older version (pinned by a
project's tool versions, say), two queues admitted jobs without counting each
other's, and the machine and its resources were used beyond their capacity.
Beyond a resource's capacity is not only slower: two at a time for their
ports, clew's end-to-end tests would have started a third and a fourth during
an upgrade. Upgrading safely meant asking every other agent to wait until the
old file was empty, which a Codex session cannot be asked (clew, 0.12).

So from 0.12 on the file stays `queue-v4.db`, and the database says what it
holds: its layout (`PRAGMA user_version`) and the admission protocol its
writers keep (the `protocol` table). The protocol is what a rule must keep to
share the queue. In protocol 1 (0.12 to 0.14) a job starts with its weight,
or with fewer cores only while nothing runs ahead of it; it fits now beside
the running jobs ahead, within the smallest limit any of them records; and it
fits beside every start ahead next to every set of the jobs ahead that fits
within the limits its members record (see [Resources](#resources)). In
protocol 2 (0.15) a job may start with any number of cores from its
`min_weight` to its weight that fits, and the jobs behind it leave room for
that. Rules that keep the same protocol may work it out differently and run
side by side: 0.12, 0.13 and 0.14 do, since each fits every job beside every
such set. From 0.14 on, every write
transaction checks the layout and the protocol first, before it reaps or
changes anything, so a process that opened the queue before a later
loadmaster moved it to another protocol writes nothing and stops with an
error, never with a new file. 0.12 and 0.13 do not read them; what stops
those is the closing of the queue below. A
layout extended compatibly is brought up to date in place by the first newer
process to open it, while older writers go on: 0.14 added when each job
entered its state (see [Status as JSON](#status-as-json)), kept by triggers so
that 0.12 and 0.13 keep it too.

0.15 moved the queue to protocol 2. A job behind one that may start with fewer
cores protects that start beside sets that protocol 1 does not count, so the
two cannot share the queue; two protocols must never hold allocations at once.
The first 0.15 to open a queue of protocol 1 closes it to new jobs, by a
trigger that refuses inserts, which binaries that never read the protocol obey
too: runs of 0.12 to 0.14 fail and say to upgrade. The jobs in it finish,
park, resume and expire as their own loadmaster has them; runs of 0.15 wait
without a place in line (giving up as `--max-wait` says, saying that no place
was kept), and reap. The first 0.15 write transaction that finds the queue
empty moves it, in that transaction: `jobs.id` becomes `job_id`, which every
statement of 0.12 to 0.14 names but their insert (0.15 names rows by `rowid`,
the same before and after), a trigger refuses an insert without a
`min_weight`, which theirs has not, and the protocol becomes 2. 0.14 then
stops on its check of the protocol, with a message; 0.12 and 0.13 stop on the
missing column, as soon as they run a statement (a connection opened before
prepares it again then). Running jobs are never stopped: new runs pause until
the jobs in the queue are done, once.

A move that never paused was rejected (Codex): 0.15 admitting its jobs by
protocol 1 while those of 0.12 to 0.14 finished, and switching once none of
theirs was left. A job 0.15 admitted then had not protected the starts
protocol 2 allows: in a queue of 5 cores, a job that started beside a waiting
one by protocol 1 let that one start with fewer cores after the switch, 6 of
5. And a run of 0.12 to 0.14 could resume a parked place of 0.15, ignoring its
`min_weight`. The move is tested against real 0.12, 0.13 and 0.14 binaries:
their jobs finish, their new runs are refused, 0.15 waits and then runs, and
afterwards they cannot use the queue.

Rejected with Codex (GPT-6-Astra): counting the jobs of another file as if
they took the whole machine (both versions can see the other as idle and start
at once, the older one never counts the newer one's jobs, and an older agent
that keeps starting jobs starves the newer ones); keeping more columns the
same in every layout, so that versions could weigh each other's jobs (an older
rule still does not account for what a newer one lets start); and a rule
version per job (an older process cannot judge a job of a rule it does not
know).

The files of 0.11 and before stay separate. Every layout from 0.7 on keeps six
columns of `jobs` as they are (the task, its state, and the processes of its
loadmaster and of its task), and `status` and `run` read them from the other
`queue-v<N>.db` files beside their own, to say which tasks run, wait or keep a
parked place there (a parked place may resume: when it expires is not among
the six); a file that cannot be read may hold jobs, and is reported as such.
They are not counted: two files cannot be changed in one transaction, so jobs
in both would still race to start, and the other version would not count this
one's anyway. A file is opened as its own version opens it, so that the last
connection to close it folds its write-ahead log into it and removes it, as
that version's would (read-only, SQLite leaves the log behind); but it is
never created, and its rows are only read. The `state.db` of 0.5 and 0.6 is
not looked at. The schema is made once, in a write transaction, by the first
process to open the file.

## Job IDs are never reused

The `jobs` table uses `INTEGER PRIMARY KEY AUTOINCREMENT`. A plain
`INTEGER PRIMARY KEY` would also preserve arrival order, but it can hand out an
old ID again once the table empties, and then a leaked `LOADMASTER_HELD` could
validate against an unrelated new job. Bringing a layout up to date in place
keeps the highest ID ever given (`sqlite_sequence`), whatever jobs are left.

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

**Admission.** Jobs are judged in line order, on every dimension at once
(CPU, each machine-wide resource, each resource in each checkout). Every use
of a resource counts in its checkout's dimension, with or without a
per-checkout limit of its own, as a use without a machine-wide capacity
counts machine-wide; before 0.12 such a use was left out of the checkout, and
a run could take a checkout past the limit another task recorded.

A job decides from the jobs ahead of it alone (and every running
measure-only job, which started without the rule), so no job is ever
delayed by jobs that got in line after it: whatever they do, its decisions
see the same thing. What the rule has to protect is the limits. A job ahead
decides its own start without looking behind it, so whatever started behind
it meanwhile must still fit beside it then. A job may start now, with some
number of cores from the least it may start with (its `min_weight`, else its
weight) to its weight, if:

1. **it fits now**: beside the running jobs ahead of it, within the smallest
   limit it or any of them records, and on CPU within its own limit lowered
   by CPU busy outside the queue (never below its `min_weight`); and
2. **it fits beside every start ahead**: for every job X ahead of it that has
   not started (waiting or parked), X plus the most the other jobs ahead could
   use beside X, plus it, fit within the smallest limit any of them records.

It starts with the most cores both allow: each only gets easier with fewer.

"The most the other jobs ahead could use beside X" is over the sets of them
that could be running when X starts, which are those that fit beside X at the
least it may start with: whatever X starts with, what runs beside it then fits
beside that. A job not started counts with any amount it may start with (CPU
from its `min_weight` to its weight). X itself counts at its weight, the most
it takes. Working out the room beside X at its weight instead let a job start
that X then took past a limit (Codex): on 8 cores, one job running on 6, X
waiting for a resource another holds and able to start with 1 to 4, and a job
of 1 behind it that starts since X at its weight never fits beside the 6; once
the resource is free, X starts with 2 beside the 6, and 9 cores are in use.
Which sets fit is judged within X's own limit where it records one, since
everything running when X starts fits within it, and elsewhere within the
largest limit the jobs ahead record; what the job must keep, within the
smallest limit it or any of them records. A job that does not fit beside X
within the limits either of them records never runs with it, and is left out.
All of that comes from the jobs ahead of the job judged, never from the jobs
behind it.

When a job Z starts, every job W running behind it started while Z waited, and
checked that it fits beside Z's start next to any set of the jobs ahead of it
that could run beside Z, with Z at its weight; the jobs running at Z's start
are such a set (by induction over the jobs behind Z in line order: they fit
beside Z with what it starts with, so beside its least), so everything running
then fits within the smallest limit any of them records. A waiting job that
may start counts as running, with the cores it starts with, for the jobs
behind it: whichever loadmaster takes the write lock starts every such job, in
line order, in one transaction (see [Polling](#polling)), so what each gets
does not depend on who polls first. Measure-only jobs count where they run,
but start whatever the rule says, so the promises do not cover them: while one
runs, or after one started, a job ahead may start past a limit, since the jobs
behind it made room for its start beside what ran then, not beside a
measure-only job that came later (found in review). A queue that mixes them,
from processes with different machine configs, is a measurement setup, not one
to rely on.

**Fewer cores than the weight.** Before 0.15 a job started with fewer cores
than its weight only while CPU outside the queue left fewer, and only with
nothing running ahead of it; beside the queue's own jobs it waited for its
whole weight. A task that runs a pool of workers does as well with fewer, only
longer: in a simulation of a day of agents' checks on 8 cores, letting jobs
start with what is left cut the mean time from getting in line to the end by
30 to 45%, with 6 to 15% of the jobs finishing later. But only the task can
say whether it may: it must size its workers by `LOADMASTER_WEIGHT`, or it
runs all of them on fewer cores, and one that holds a scarce resource (a
browser, a database) holds it longer with fewer cores, which can keep others
waiting longer than it gains. clew's tasks read `LOADMASTER_WEIGHT` nowhere,
and its unit runs and UI captures hold an exclusive resource. So a task
consents with `min_weight`, the fewest cores it may start with; without it, it
starts with its weight. Exempting tasks that use a resource from the rule was
rejected (Codex): a generous resource and a scarce one differ, and that is the
task's to say by not setting `min_weight`.

Condition 2 asks no more than it must. Jobs may end and leave the line at any
time, so any set of the jobs ahead that fits within the limits all its members
record may be what runs when X starts: the others end or leave, the set's jobs
that have not started start in line order (each passes the rule, since the
whole set fits beside X), and X starts last. A job that condition 2 holds back
could therefore, in some future, be running when a job ahead of it takes the
machine past a limit; only knowing how long jobs run would rule that out.
Where the jobs record the same limits, the rule thus starts every job that
can start without that risk, with the most cores it safely can, as far as its
computation is exact (below); a test compares it with a search of every set
and every number of cores, on the lines the rule itself reaches. Where limits
disagree it is more careful than
that: the most the jobs beside X could use and the smallest limit among them
may come from different sets, and keeping the smallest limit of each set as
well would make the computation much larger. Codex (GPT-6-Astra) worked out
the same result independently. Before 0.13, two approximations held back jobs
that could have started: a dimension that could not bind on its own was
bounded by its sum even where the sets that fit on another one could not use
all of it (found against the search; 0.1% of the decisions in a random
simulation, 1.3 to 2.5% of the mean wait in a simulated day of agents'
checks), and a job that could not run beside X by its own smaller limit was
still counted beside it (Codex; 0.9% of the decisions with disagreeing limits,
0.08% now).

Before 0.12 a waiting job set aside its amount where it fit, less what it
was sure to get back by the time it could start (0.11), and held back from
everyone behind it what it was short of. That estimate was refined three
times and stayed too cautious wherever the order of the line mattered: units
waiting in turn for the browser a long UI capture held set aside cores that
only a later end would give them, and a second capture waiting behind the
first, short of CPU, held back every job behind it, though each could only
start once the run ahead of it ended and freed its cores (clew, 0.9 to
0.11). The rule now checks the condition the set-asides stood for. Run times
are still not used: a backfill that lets a job behind start when it is
expected to end in time breaks the guarantee whenever the estimate is wrong.
With CPU alone, a small job may now start ahead of a big one that waits, but
only where it cannot be running in the big one's way when that starts.

"The most beside X" is worked out by a small dynamic program over the usage of
the dimensions that the jobs could use more of than the room beside X, and
that have little room (at most 64 each, and at most 4,096 usage vectors in
all). Any other dimension is bounded by the sum of the jobs' largest amounts
and the room, which only makes the bound larger. Where that bound alone keeps
a job waiting, the most of that dimension over the sets that fit on the others
is worked out from the jobs beside X, and kept as more jobs are passed until
the states are worked out again, so jobs that cannot run together are not
counted together. Keeping that most in every usage vector instead, as first
tried, cost rebuilds whenever a job ahead recorded a larger limit or a second
job used a dimension, and a rebuild that left no usage vectors lost it,
letting a job past a limit (Codex). Two jobs with the same amounts and limits
have the same jobs beside their starts, but for the two swapped, so one start
stands for every later one of its kind: a pass costs about as many additions
as jobs times kinds of job, not pairs of jobs. Its cost does not grow with the
size of a capacity. The design was reviewed three times (Codex); each review
found counterexamples to the version before it (a measure-only job that may
end, a job that may start now and then starts later with more, a use without a
checkout limit, limits that disagree, poll order under a lowered CPU limit),
and each is now a unit test.

A unit test simulates random workloads with several resources (a browser of
capacity 1, one that some jobs declare with capacity 1 and others with 2, uses
with and without per-checkout limits), departures from the line, parks that
resume or expire, jobs that adapt to CPU outside the queue or not, capacities
of 6 and 8 cores (and ten times as many), with and without CPU outside the
queue that changes on its own, polling in line order, in reverse and at
random. It checks that each job starts at the same time whatever the order of
polls and whether or not the jobs that got in line after it exist, that every
job starts, and that the running jobs never use more than the smallest limit
any of them records. Leaving out any part of the rule (jobs not started beside
X, their range of cores, the smallest limit, a job that may start now in
either condition) fails it. Other tests check that one start per kind decides
and explains every job as a start per job does, on random lines of many kinds
and of few, and that no job waits that the search of every set would start.
The soak test adds resources and a second checkout and checks every limit from
inside the tasks.

A nested run takes no part in admission: it has to fit in its holder's
slot (see [Nested runs](#nested-runs)).

## Why a job waits

The first real user could not tell from `status` why jobs waited: a unit run
waiting for the browser that a long UI capture held set its cores aside, and
every check behind it stopped. That only showed in a separate experiment. So
the admission rule explains its own decision, rather than a second piece of
code guessing at it: for each dimension a job does not fit, either it does
not fit now (the running jobs ahead, and those about to start, use too much
of it: it names those that hold a resource, the CPU outside the queue if it
lowered the limit, and the job ahead whose record set a smaller limit), or it
would not fit beside a job ahead when that one starts (it names that job,
says how much could be in use then, and names the jobs beside it when the
rule found such a set; when the amount is only a bound, it says so rather
than name a set that may not be able to run). `status` shows this for every
waiting job and the waiting messages for their own, in the same words, and
`status --json` as data. The simulation checks, at
every step, that the explanation agrees with the decision each job gets and
that every waiting job has a reason.

A simulator that runs a made-up list of jobs (task, arrival, duration)
through the rule was asked for too, to try weights without touching the real
queue. Made-up durations and core use are exactly what goes wrong with
weights, though, so it would answer confidently from the wrong inputs. The
explanations above, the warnings of `loadmaster check` and the report's
measured cores cover most of the need; replaying the recorded history against
other settings would answer the rest from real inputs, and is the form to
build if it is needed.

`check --scenario` (0.14) is the part of that which needs no durations: it
judges the queue a file describes (a `status --json` captured from a real
queue, changed by hand) by the same function `run` and `status` use, with the
CPU busy outside the queue the file gives, and says what would start, with how
many cores, and why the rest wait. It answers a question about the rule, not
about run times. clew asked for it after checking admission in a state
directory of its own: with adapting on, the real queue's jobs and the rest of
the machine counted as outside, the limit fell to 2 cores and the check showed
nothing; with adapting off, it missed what only happens with adapting on
(0.11's count of a job ahead not started as one core). Pinning the measurement
with an environment variable was rejected (Codex): the measurement is a row
that every process of a queue shares and smooths, so one process's pinned
value would become the others' input, and a state directory of its own
isolates the accounting, not the machine.

## Status as JSON

Tools read the queue too. clew's list of the pull requests whose checks were
in line parsed the table `status` prints, and stopped working the day a new
state (`lingering`) appeared in it (0.11). The table is for people and keeps
changing with the explanations, so `status --json` prints the same information
in a format of its own, with a `format` number, documented in the README. Why
a job waits comes as data (the reason, the dimension, what is used and the
limit, the jobs by ID, and whether the jobs beside a start are a found set or
only a bound), with the sentence the table shows beside it.

Format 1 (0.12) said that any change to it was breaking, so adding a field
would have needed a new format each time. Format 2 (0.14) says what a tool may
rely on: fields may be added, and tools ignore the ones they do not know; a
field removed or renamed, a change of its meaning, type or unit, and a new
value of `state`, `need` or `reason` are a new format. The values of those
three are closed, so that a tool need not guess what an unknown state means;
texts may change in any release.

clew mapped each state to the field its time was in (`enqueued_at_ms` for a
waiting job, and so on), and had to follow each new state. `state_since_ms`
gives when the job entered the state shown, as the table's FOR column counts
it, and only a time that was seen: when it got in line or resumed its place,
parked, started, or was handed over to its task's process group. A job left
orphaned by a `loadmaster` that died got there at a time nobody saw, so its
time is `null`, as is that of a job that waited from before 0.14 (it may have
resumed). Before, a resumed job's FOR counted from when it first got in line,
and an orphaned one's from when it started. The database records the time by
triggers, so that the writers of 0.12 and 0.13, which share the queue and know
nothing of it, keep it as well.

`status --json` is also the input of `check --scenario`, which judges the
queue a file describes by the admission rule (see [Why a job
waits](#why-a-job-waits)), so it carries the rule's inputs as they are: the
CPU busy outside the queue unrounded, and whenever it was measured, not only
when the machine config of the process that prints it adapts to it.

## Canceling jobs

Agents on one machine run the same commands, so stopping one's own checks by
matching command lines stopped other agents' checks too. `loadmaster cancel`
stops jobs by ID, or every job of the caller's checkout (`--here`), the unit
that one agent works in.

- It does not signal the job. Agents' sandboxes (Codex's on macOS, measured)
  let a command signal only processes it started itself, and each command
  gets a sandbox of its own, so an agent could not cancel the checks it had
  started a command earlier. Instead `cancel` leaves a request in the queue
  database, and the job's own `loadmaster` acts on it: it looks every 100 ms
  while waiting and every 200 ms while its task runs. A waiting job then
  leaves the line, and a running one sends its task its stop signal, or
  SIGKILL with `--kill`, as for any signal it forwards, and frees the slot once the task
  has exited. Its task is its own child, so that is allowed in any sandbox.
  A parked place is deleted.
- A task gets its stop signal at most once, from a cancel or for a SIGTERM
  from outside alike: a second one would cut short the clean-up the first
  started (as in a handler registered with `process.once`), and with several
  sources (a cancel, a tool's timeout) it would otherwise come up. Its
  `loadmaster` sends it, so that one rule covers every path through it.
  When it hands the slot over to the processes its task left behind, it
  passes on the signals that arrived meanwhile and records whether the task
  had its stop signal, in the same write transaction as the handover, so a
  `cancel`, which signals those processes itself, sends them none either.
  Signals that arrive after the handover are not passed on: the processes
  left behind are the job from then on, and `cancel` reaches them; passing
  them on unrecorded would let a cancel send a second stop signal. SIGKILL
  is there to force a task: `--kill`, as often as it takes.
- A job has one request row, kept until the job is gone, with the strongest
  stop asked for (several `cancel`s are that one row) and the strongest one
  done, by its `loadmaster` and by a `cancel` signalling the task of a job
  whose `loadmaster` is gone. Its `loadmaster` acts on what it has not done
  yet; a `cancel` that asks for no more than was done does nothing, so a
  second plain cancel never makes a job already stopping stop again. Each
  step is one write transaction, so each stop is done once.
- A `loadmaster` stopped with Ctrl-Z acts on nothing, not even on its task
  exiting, so `cancel` continues it (SIGCONT, checked by PID and start time),
  and it then acts on the request itself. Only a stopped one is continued: one
  that is merely slow to look (behind a locked database, say) needs nothing,
  so a `cancel` in a sandbox, which may not signal it, does not fail for it.
  If continuing it is refused, `cancel` says why and how to stop the job
  instead; the request stays, so the job stops once its `loadmaster` is
  continued.
- A task whose `loadmaster` is gone is signalled from `cancel`: its stop
  signal (which the job records) if nobody has sent it one (its
  `loadmaster` may have, before it handed the slot over to the processes the
  task left behind), SIGKILL whenever asked.
  It decides, signals and records the signal in one write transaction, so
  no other `cancel` can send it in between; if the signal is refused, the
  transaction is rolled back, so a `cancel` run outside the sandbox can
  still send it.
  So a waiting job whose `loadmaster` was killed after its task had started
  has that task killed too. Two gaps are left, both needing a `loadmaster`
  killed with SIGKILL at a particular moment. Killed between taking a stop
  request and passing it on, it leaves its task unsignalled, and `cancel`
  waits; its note after 10 seconds points to `--kill`. Killed after passing
  the stop signal on but before handing its slot over, it leaves no record
  of it, and a later `cancel` sends one more. Closing the first would open
  the second wider, and the other way round.
- It waits until the jobs have left the queue, so the caller knows the slot
  is free, and says after 10 seconds which ones are still stopping. It never
  escalates to SIGKILL by itself: a task that takes long to stop is usually
  cleaning up (an e2e environment, say), and cutting that short leaves the
  next run a mess.
- A job can change while it is canceled. One that gave up waiting at that
  moment parks: its place is dropped. If a re-run resumes the place first,
  its new `loadmaster` finds the request and acts on it.
- A PID is checked by its start time right before the signal, as when a
  signal is forwarded to a task's group: a PID the OS hands out again within
  that instant would get it. macOS has no pidfd to close that gap, and the
  OS would have to go through its whole PID space in between.
- `--here` leaves out the job it runs in (`LOADMASTER_HELD`), so a script
  inside a task can clear the rest of its checkout without ending itself.
  Agents sharing one checkout cancel by ID instead.
- The command logs a `canceled` record for each job besides the run's own
  record, as the reaper does for `lost` jobs, and the report leaves those
  checks out: a cancel says nothing about the machine. The report matches the
  record to the run by job ID, task and directory, and by when the job got in
  line, within two minutes. IDs start over in a new database (one deleted,
  or the file of a new layout), so a run is taken for a canceled one only if
  two databases gave them the same ID for the same task and directory within
  those two minutes: two queues in use from one directory at once.

## Load outside the queue

Work that cannot go through the queue (a game, a video call, other people's
builds on a shared machine) takes CPU the capacity counts on, and heavy
checks then run into timeouts. Two answers were asked for: to adjust after a
check times out or fails, and to keep checks from crowding a busy machine.

Adjusting after a failure is not done. loadmaster cannot tell a failure from
the load from one in the code, so retrying would hide real failures and run
every failing check twice, and raising a weight would change, on one
machine's bad hour, a setting committed for every machine; a timeout of the
agent's own also ends loadmaster with its task. What loadmaster can tell is
whether the machine was saturated: it measures the machine's CPU busy while
each task runs, records it in the history log, notes on stderr when a task
fails or is killed while the CPUs were 95% busy or more, and `report` counts
such checks. That leaves the agent or person to re-run, with the reason.

Keeping checks off a busy machine is done, at the start of each job. An
earlier proposal, to admit jobs only while the load average is below a
limit, was not taken, for three reasons this design answers:

- The load average lags by a minute and counts more than CPU (on Linux,
  processes waiting for disks). Instead the machine's CPU time comes from the
  kernel's tick counters (each CPU's from `host_processor_info` on macOS, as
  the machine's own total is added up only once a second; `/proc/stat` on
  Linux), measured over a quarter of a second and averaged with the last
  measurement if that is at most 10 seconds old: on a Mac running other
  agents' checks, single measurements 3 seconds apart ranged from 3 to 8
  cores. Measuring since an earlier reading instead would count what a job
  that started and ended in between used as outside. A reading in which a count went back (a counter wrapped,
  or Linux's iowait did) measures nothing. Waiting processes take turns
  measuring, through a row in the queue database that stands for 3 seconds. What the running jobs were granted counts as the
  queue's own; the rest is outside, including what a job uses beyond its
  grant, since those cores are busy too. `status` therefore calls it the CPU
  busy beyond what running jobs got. Telling the two apart was asked for
  (clew), to see whether to raise a weight or bring the outside work into
  the queue; that would take each job's process tree measured over the same
  quarter second, with what its finished processes used (the children's
  times in `/proc` on Linux, from `proc_pid_rusage` on macOS), for a number
  that is only shown. The live processes' times alone would miss the
  compilers and test workers that come and go, and count what they used as
  outside. `report` shows each task's cores busy against its weight, which
  answers the question.
- A limit on it lets work outside the line delay every job indefinitely.
  Instead the outside load beyond the cores the capacity leaves free lowers
  the CPU limit of the jobs that start, down to their `min_weight` (a job
  without one keeps its weight as its limit, so it starts once nothing runs
  ahead of it). The first job in line therefore still starts once the jobs
  ahead of it are done: outside work can slow the line, but not stop it.
- It breaks the guarantee the line gives, in an order nobody can predict.
  Instead the lowered limit only throttles starts: a job that adapts starts
  only if it and the jobs ahead of it fit within it, while what the rule
  protects is the capacity (see [Resources](#resources)); jobs that started
  before may keep the machine above the lowered limit, never above the
  capacity. A job is still judged by the jobs ahead of it alone, so jobs
  behind never delay it, however the CPU outside the queue changes; a
  simulation with outside load that changes on its own checks that. That
  holds for the measurements as they come: the measurement itself counts
  what a job uses beyond its grant, and what a job that started and ended
  between two readings used, as outside, so a job behind that uses more than
  it got can lower the limit a job ahead starts with (Codex). The OVER column
  of `report` shows the runs that do.

A job keeps what it started with: a task cannot be made to use fewer cores
once it runs, and counting its grant anew would make jobs behind wait for
cores it does not take. The task learns what it got from `LOADMASTER_WEIGHT`,
so one that sizes its workers by it runs fewer of them, which is what keeps
a single heavy check from timing out next to a game. Before 0.15 every job
could get as few as one core this way, whether its task read
`LOADMASTER_WEIGHT` or not: clew's ran all their workers on the fewer cores
anyway (the report's OVER column shows that). Now a task without `min_weight`
is never given fewer than its weight. `adapt_to_outside_cpu = false` in the
machine config turns the throttle off; each job records the setting it got in
line with, as it does its capacity. GPU and memory are not measured:
there is no portable, unprivileged way to read either per machine, and CPU is
what checks contend for most.

## Nested runs

A `loadmaster run` inside a task (one that sees `LOADMASTER_HELD`) runs
directly in the holder's slot instead of queueing. Queueing could deadlock:
the inner job could wait for capacity, or for a resource, that only the
holder's own job will free, or behind a job that waits for that.

So the inner run must fit where it runs, and anything else is an error (exit
code 125) that says what to change:

- The holder must be a running job started from the same checkout, the check
  `loadmaster held` makes. A stale or made-up `LOADMASTER_HELD`, or one that
  leaked into another worktree, would otherwise let a run skip the queue. A
  repository with more than one `.loadmaster.toml` has one checkout per file,
  so a task cannot nest the tasks of another file.
- The inner task's weight, capped at the capacity the holder got in line
  with, must not exceed the cores the holder got (its weight, unless it
  started with fewer; before 0.15 its weight, so a holder started with 2 of 8
  let a fixed inner task of 4 run, Codex); an inner task with `min_weight`
  fits if that does, and runs with the holder's `LOADMASTER_WEIGHT`. Every
  resource it uses must be in the holder's `uses` with at least the same
  amount. Otherwise its load would
  run outside the accounting: a heavy check inside a light one would overload
  the machine, and an e2e run inside a task that holds no database would break
  the per-checkout limit that keeps two runs off one database. Amounts are
  compared as declared.
- Several nested runs at once inside one holder are not added up: like a task
  that starts more workers than its weight, that is the task's own business.

## Checking that a script runs through the queue

`loadmaster held` lets a script refuse to run outside the queue, whatever
started it. Permission rules and hooks only see an agent's own commands, and
not every agent has them.

- It makes the check a nested run makes: `LOADMASTER_HELD` must name a
  running job started from the same checkout, in the sense of per-checkout
  limits (the resolved directory of the `.loadmaster.toml` found from the
  current directory). Otherwise a value that leaked into another worktree
  would pass. The file is only located, not parsed, so a config edited while
  the job runs makes no difference.
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
- The search for `.loadmaster.toml` goes up from the current directory and
  stops at the root of its repository: the nearest directory with a `.git`
  entry, which linked worktrees and submodules have as a file. A file further
  up belongs to another repository. The case that mattered: a worktree inside
  the main checkout (under `.claude/worktrees/`, say) on a branch without the
  file used to find the main checkout's, and quietly ran its tasks against the
  main checkout's code. Now it fails and names the file it did not use.
  Outside any repository the search goes up to `/`.
- Config and state follow the XDG layout on macOS as well
  (`~/.config/loadmaster/config.toml`, `~/.local/state/loadmaster/`), because
  that is where a CLI user expects them and what the documentation shows.
  `LOADMASTER_CONFIG` and `LOADMASTER_STATE_DIR` override them (the tests use
  this for isolation). Relative overrides are resolved against the current
  directory, and the resolved state directory (and `LOADMASTER_CONFIG`, if
  set) are passed on to the task, so a loadmaster it runs finds the same
  queue and the same machine config even though the task runs elsewhere.
- A file named by `LOADMASTER_CONFIG` must exist, while a missing file in the
  default place means all defaults. A typo in the variable would otherwise
  give one process the default capacity while the others use the configured
  one. For the same reason the variable is passed on to tasks only when it
  was set: the default place is the same from anywhere. A task that changes
  `XDG_CONFIG_HOME` or `HOME` therefore changes it for the loadmaster
  commands it runs, which is what it asked for; forcing the parent's file on
  them would undo that. No accounting depends on it: a nested run is judged
  by the terms its holder recorded, not by the machine config.
- The state directory is created with mode 0700: it holds command lines and
  paths.

## Agent sandboxes

Many agents run each command in a sandbox. In Codex's workspace-write
sandbox on macOS (measured with `codex sandbox` and real `codex exec`
sessions):

- Writes outside the workspace and the temp directories are denied, so the
  state directory has to be added as a writable root. Until it is, the
  commands that use the queue fail with exit code 125, and the error names
  the state directory and the sandbox rather than SQLite's "unable to open
  database file": the person who reads it has to change the agent's
  settings, not loadmaster's. SQLite's errors don't say whether the
  directory is to blame, and it opens a database it may not write for
  reading without saying so, so each command first makes (and removes) a
  file in the directory, and says so if it cannot.
- Reading other processes' start times and listing process groups work, so
  a sandboxed `loadmaster` sees jobs started outside its sandbox, waits
  behind them and reclaims them once they are dead.
- Signals reach only processes in the same sandbox, and each command gets a
  sandbox of its own, which is why `cancel` goes through requests (see
  [Canceling jobs](#canceling-jobs)). Forwarding a signal to a job's own task
  stays within one sandbox.

All agents must share one state directory, so an agent in a container or VM
with its own home is only coordinated with the others if the directory is
shared with it.

## Polling

Waiting jobs poll the queue every 100 ms; that bounds the extra latency of each
admission. A poll first reads the queue without the write lock and only takes
it (`BEGIN IMMEDIATE`) when something can change: the job is at the head and
fits, or a job that can affect its decision (a running one, or one ahead of
it) is dead and must be reaped. Otherwise every waiter would take the write
lock ten times a second and compete with the writes that matter, such as
recording a task that is held at its gate. The decision is always made again
under the lock before admitting. `Queue::open` likewise checks that the
schema has been made without the lock. The CPU busy outside the queue is
measured again only once its row is 3 seconds old, by whichever waiter finds
it so: a quarter of a second of one waiter's time and one short write every
3 seconds while jobs wait, and a quarter of a second more for a job that
starts after 3 quiet seconds. A running
task is polled for exit every 20 ms. The waiting message is printed when a
job starts waiting and then every 10 seconds: often enough to show progress,
rarely enough not to flood an agent's context.

Since 0.15 the loadmaster that takes the write lock to start its own job
starts every waiting job that may, in line order, each with its cores and
counting as running for the jobs behind it, in one transaction. A job may
start with fewer cores than its weight, so what it gets must not depend on
whether a job ahead of it, which may also start, polled first; and before,
such a job ahead stayed a start in condition 2, with its whole range, since it
might start later with more, while once started it is a fixed grant. A job
another loadmaster started is running from then on, holding its slot as one
whose task runs, and its own loadmaster finds it so on its next poll, reads
the cores it got and starts its task. Measure-only jobs start by themselves,
and parked ones when resumed. A job that is started meanwhile as its
`--max-wait` runs out runs instead of parking: parking and that check are one
transaction, and the run writes its `gave_up` line only once it has parked, so
a run that went on to start never says it gave up (killed between the two, the
run leaves no line; killed before parking, it is reaped and logged as lost).

Each waiting job's loadmaster notices on its next poll that it may start, or
that it was started. That is negligible for tasks that run for seconds or
minutes, but visible with tiny ones: 200 simultaneous one-core 50 ms jobs took
2.8 s at capacity 8 under 0.14, where starting each the moment cores were free
would take 1.25 s (under 0.11's rule, 16 s on a 14-core Mac; zero capacity
violations).

The rule depends on nothing but the jobs and the CPU outside the queue, so a
poll that finds the database as the last one left it (`PRAGMA data_version`
counts other connections' commits, `total_changes` this one's) keeps the last
decision. Whether the jobs are alive and how old the CPU measurement is change
without a write, so those are looked at on every poll. Judging a job takes a
pass over the line in which, for each kind of job ahead that has not started,
what could run beside its start is built up job by job: about 30 microseconds
for a line of 12 jobs and 0.25 ms for 160 (four kinds of job, two resources, 8
cores; release build, Apple M-series). With 160 jobs waiting behind one that
held the machine, the waiting processes used 0.1 cores in all, and 4 under
0.12, which judged every job ahead on every poll and worked out a start per
job.

## Configuration details

- `cmd` must be an array. A string `cmd` is rejected with a hint showing the
  array form and the `["sh", "-c", ...]` escape hatch.
- Unknown keys are errors (`deny_unknown_fields`), so typos such as `wieght`
  fail loudly instead of being ignored.
- Each process reads the capacity when it starts; a changed capacity applies to
  new runs. Weights are summed as 64-bit integers, so even `capacity =
  4294967295` cannot overflow the admission check.
- Nested runs skip the capacity check and inherit the holder's
  `LOADMASTER_WEIGHT`: they run inside the holder's slot, which they must fit
  (see [Nested runs](#nested-runs)).
- `min_version = "X.Y.Z"` names the oldest loadmaster that can read a file.
  Branches and machines run different versions, and strict keys make an older
  version reject any key added later with only "unknown field". The version
  is checked before the strict parse, so an older version says what the file
  needs instead; an unknown key's error also names the running version and
  the two ways it comes about. (Versions before 0.5 do not know `min_version`
  either.)
- `loadmaster check` loads both configs as a run would, without opening the
  queue, so CI can validate them, and warns about what the rule will make of
  them on this machine: a weight at or above the capacity (it takes the
  machine, and while it waits everyone behind it waits), a resource with no
  limit anywhere, and a task whose run, waiting for a resource held by a
  long run, keeps other tasks out: a task that fits beside the long run but
  not beside the waiting run's start, and would if the waiting run took one
  core. The rule itself decides that, on a made-up queue of the three runs,
  so the warning cannot drift from the rule. Warnings depend
  on the machine's capacity and do not fail the check. Under measure-only,
  where nothing waits, there are none, and a capacity of 1 (a small CI
  runner) does not get one per task.
- A weight above the capacity counts as the whole capacity, and the task runs
  alone. Weights are committed with the repository while the capacity belongs
  to each machine, so a repository's weights have to work on smaller machines
  too (a CI runner, a laptop); rejecting them, as the first version did, made
  the shared config unusable there. A note on stderr still points out a typo
  such as `weight = 80`. The capped weight is what the job records and what
  the history log shows as its weight; `LOADMASTER_WEIGHT` says the cores it
  started with, which are fewer for a task with `min_weight` when that is
  what fits (the log then has them as `granted`). A `min_weight` above the
  capacity is an error, not capped: such a task would never start.

## History log

`history.jsonl` in the state directory gets one JSON object per queued run:
job ID, task, weight (and `min_weight` if it is less), resources used,
capacity, whether the machine was in
measure-only mode,
caller directory, arguments, outcome (`exited`, `gave_up`, `interrupted`,
`spawn_failed`, `error`, `lost`, or `canceled` for the extra record
`loadmaster cancel` writes), queue time, wait and run durations, CPU
time, the 1-minute load average when the task ended, the signal that killed
the task, if any, the exit code `loadmaster run` returned, whether that code
is one of the task's `ok_exit_codes`, and for a `lost` run whether its task
had started. The repository's config is not at hand when the log is read (one
log covers every repository), so what it says is recorded when the run ends.
Every run that
got in line writes exactly one line, whatever happened afterwards: a
`loadmaster` killed with SIGKILL cannot write its own, so the process that
reaps its job writes a `lost` line for it, after the reaping is committed.
That needs the capacity and mode of the job, which are therefore kept in the
queue database. A run that gives up writes its line before it parks its
job: a parked job has no owner whose loss could be logged, so a `loadmaster`
killed after parking (while its message to a full stderr blocks, say) would
otherwise leave no line. Killed before parking, its job is reaped and logged
as `lost`, and the report joins that line to the `gave_up` one as a single
check. JSON Lines is easy to analyze with `jq`. It is not rotated.
`loadmaster report` reads only records of the format this version writes, and
says how many lines it skipped: a record of an older format may lack what the
report judges by (whether a task passed, or had started), and guessing would
misreport it.

Known gaps, each needing an unusual combination of events, accepted rather
than paid for with per-attempt IDs and logging inside the queue's
transactions:

- A `loadmaster` killed right after removing or handing over its job but
  before writing its line leaves no line.
- If parking a job that gave up fails (the database stayed locked for the
  whole 60-second busy timeout), its `gave_up` line is already written and
  its job is later logged as `lost` before it started: `report` counts a
  check that left the line, though `loadmaster` exited with 125.
- If releasing or handing over a finished job fails (the database stayed
  locked for the whole 60-second busy timeout), `loadmaster` warns, exits with
  the task's code and logs its result, and the job is later also logged as
  `lost`. The slot stays held until the task's processes are gone either way.

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
  part of what admission costs. Job IDs restart in a new database (one
  deleted, or the file of a new layout), so a record only joins a kept place if it has the queue's resume key (caller
  directory, task, arguments) and starts within the place's lifetime of giving
  up (120 seconds, plus two minutes for a busy database and clocks). A `lost`
  record carries the time the job first got in line, so it may start earlier
  than the attempts it joins.
- **Results**: an exit code in the task's `ok_exit_codes` (`[0]` unless set)
  is passed and any other code failed, so a task that exits with 143 on its
  own is a failure. A check that reports "differences found" with a non-zero
  code would otherwise bury load-induced failures among its findings. A task
  killed by a signal (recorded from its exit status), or a `lost` run whose
  task had started, is killed: the harm admission is meant to remove. A signal while waiting, a
  `lost` run that was still waiting, and a place given up and never resumed
  left the line: the cost of waiting. A `lost` run counts as started once
  its task's process group was recorded, since the task is let through right
  after that; a job admitted but killed before then never ran. The two used
  to share one column, and the first real user had to tell them apart by hand
  (run time 0); a period of admission would otherwise look worse for its
  timeouts in line than a baseline that never waits. `lost` records say
  whether the task had started (`started`). A failed spawn and a loadmaster
  error are not run. A wait that never ended in a start still counts, up to
  when it ended, so waits that ran into a timeout are not hidden.
- **Cores**: CORES MED and CORES P90 are the median and 90th percentile of
  each passed run's CPU time divided by its run time. A median alone hid a
  task that sometimes used far more than its weight (10.7 cores against 8 in
  the first real use, while the load average peaked); a P90 above 1.2 times
  the weight is marked with `!`. That compares with the weight, not with
  what each run got: a run that got fewer cores while CPU was busy outside
  the queue and used its whole weight anyway passed unseen (Codex), and clew's
  tasks, which none of them read `LOADMASTER_WEIGHT`, did just that. OVER
  counts the passed runs that kept more than 1.2 times the cores they got
  busy, each against its own grant.
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
  worktrees of one project have unrelated paths, and within a name by weight
  and resources used: during a switch-over, old branches run the same task
  with other settings, and one row with a range of weights hid which was
  which. `--dir TEXT` keeps only checks run from directories whose path
  contains TEXT, which usually picks out one project with all its worktrees.
  Arguments do not make rows of their own: they are free text, rows would
  split into samples too small to read, and a run's setting is what it
  declared. `--args TEXT` keeps only checks with an extra argument that
  contains TEXT, to look at the runs that tried a flag (clew ran `unit` with
  `--maxWorkers=4` at weight 2, and its row rightly got `!`).
- **Periods**: `--since` and `--until` take a time ago or a local time, and
  keep the checks that got in line in between, so the time around a change of
  settings can be compared directly. A day that does not exist (February 30)
  is an error rather than whatever `mktime` turns it into.
- **CPU time** is the task's `RUSAGE_CHILDREN` delta, which covers the task and
  every descendant that was waited for. loadmaster has one child, so nothing
  else is mixed in. Processes a task leaves behind are not counted.
- **Load average** is the system-wide 1-minute average: it lags and includes
  everything else on the machine, so it is only a rough signal. It is recorded
  because "the machine is sluggish" is part of the problem, and nothing else
  measures it.

## Messages are in English

loadmaster is meant to be published as open source.
