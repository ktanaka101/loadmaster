# loadmaster

**Weighted job admission for parallel AI agents.**

Run five AI coding agents in parallel (say, Claude Code in five git worktrees)
and each one runs lint, tests, builds and e2e on its own. They fight over CPU
and memory, tests time out for no real reason, and agents start "fixing" bugs
that are really just an overloaded machine.

loadmaster puts those commands behind one machine-wide queue. Unlike queues and
semaphores that cap the *number* of concurrent jobs, it knows that an e2e run
is much heavier than a lint: every task has a **weight**, the machine has a
**capacity**, and a job starts only when its weight fits.

With capacity 8, lint = 1, test = 4 and e2e = 8:

- one test and four lints run together (4 + 4 = 8)
- two tests run together
- e2e runs alone

A count-based limit can't express this: set it low and the machine idles while
lints wait; set it high and three e2e runs at once bring the machine down.

The line is first come, first served, so a big job is never starved by a
stream of small ones. CPU is not the only limit: tasks can also use named
resources, such as a few local database stacks, memory, or the one dev server
port a checkout has.

## At a glance

| | |
| --- | --- |
| Problem | Several AI agents on one machine run heavy checks at the same time. CPU and memory contention causes timeouts that look like test failures, and a sluggish machine. |
| Approach | Every task has a weight (≈ CPU cores it keeps busy), the machine has a capacity, and the line is first come, first served. Tasks can also use named resources (database stacks, memory), limited per machine or per checkout; a job waiting for one doesn't hold up jobs that don't need it. |
| Interface | A command wrapper: `loadmaster run <task> [-- args]`. No daemon or server; processes coordinate through a SQLite file. |
| Crash safety | Places and slots of killed processes are reclaimed automatically (PID plus start time, so reused PIDs don't fool it). A task keeps its slot while it runs, even if `loadmaster` itself is killed. |
| Agent support | Exit code 75 with a kept place in line when a wait exceeds `--max-wait`, so a tool timeout never sends an agent to the back of the line. `loadmaster held` lets scripts refuse to run outside the queue, whatever the agent. `loadmaster cancel --here` stops an agent's own checks without touching other agents' runs of the same commands. `loadmaster status` says what each job waits for, and `loadmaster check` what the config means on this machine. Ready-made Claude Code settings: instructions, timeouts, permissions, a hook that blocks direct runs. |
| Measuring | `loadmaster report` shows failures, timeouts, run and wait times and the cores each task really used; a measure-only mode records a baseline to compare with. See [Checking that it helps](#checking-that-it-helps). |
| Platforms | macOS and Linux. On Windows, WSL2 should work (it runs the Linux binary) but is not tested yet; native Windows is not supported. |
| Integration cost | One binary, one `.loadmaster.toml` per repository, a few lines of agent instructions. Removing it means deleting those, `~/.local/state/loadmaster` and `~/.config/loadmaster`. |
| Status | 0.5, new. Verified with tests, soak tests and real Claude Code sessions with biome, vitest and Playwright; see [Status](#status). |
| License | MIT or Apache-2.0 |

## Is it for you?

Use loadmaster when:

- several agents (or agents and you) run lint, tests, builds or e2e on the
  same machine at the same time;
- those checks differ a lot in cost, so "at most N at a time" either wastes
  the machine or overloads it;
- the agents can be told, or made, to run checks through a command (agent
  instructions, permission rules or hooks).

Look elsewhere when:

- only one agent runs at a time, or the checks are light: there is nothing to
  coordinate;
- the checks run on several machines or in CI: loadmaster coordinates a single
  machine;
- you need priorities, preemption, or limits based on measured CPU or memory:
  loadmaster uses static weights;
- you need native Windows (WSL2 should work, but is not tested yet; see
  [Install](#install)).

Not sure? Record a baseline first and compare; see
[Checking that it helps](#checking-that-it-helps).

## Compared with alternatives

| Option | What it does | Why it falls short here |
| --- | --- | --- |
| Count-based queues and semaphores (Block's agent-task-queue, `flock`, GNU `sem`) | Run at most N jobs at once. | N can't tell a lint from an e2e suite: low N idles the machine, high N overloads it. |
| `nice`, CPU quotas, containers | Change how CPU time is shared. | Everything still runs at once; tests get slower and time out instead of waiting their turn. |
| Lower test parallelism in each agent | Smaller footprint per run. | Five agents still add up, and nothing coordinates them. |
| Run checks only in CI | Moves the load off the machine. | Agents iterate locally; a round trip through CI is slow. |

## Status

loadmaster is new (0.5). What has been verified:

- Integration tests that start real `loadmaster` processes check that the
  capacity is never exceeded (from the tasks' own records), that big jobs are
  not overtaken, that `kill -9` of waiting and running jobs is recovered from,
  nested runs, exit codes and config errors. They run on macOS and Linux, also
  in a container without an init process.
- A soak test simulates five agents with Claude Code-style timeouts (SIGTERM,
  then SIGKILL of the process group), re-runs after exit code 75, and crashes,
  in two checkouts with two resources besides CPU, while jobs are canceled at
  random. 1656 runs (503 of them canceled) in 10 minutes on macOS and 503 in
  3 minutes on Linux exceeded no limit, every task got through, and nothing
  was left behind.
- A simulation checks, on random workloads with several resources, that no
  job is ever delayed by jobs that got in line after it; rules broken on
  purpose are caught.
- Five real Claude Code sessions (Sonnet, headless) in five git worktrees of a
  demo repository, set up as described in
  [Using it with Claude Code](#using-it-with-claude-code), ran lint, test and
  e2e three times each on a 14-core Mac (capacity 8, weights 1/4/8,
  `LOADMASTER_MAX_WAIT=60`). The checks were stand-in scripts that sleep for 2,
  10 and 20 seconds. All 45 checks went through loadmaster, the 12 waits that
  hit `--max-wait` were resumed by re-running, the hook blocked the one direct
  run, and the capacity was never exceeded. Checks waited 26 seconds on
  average and 60 at most: with fifteen 20-second e2e runs that each need the
  whole machine, that time has to go somewhere, and waiting replaces the
  timeouts it would otherwise cause.

- With real tools (biome, vitest, Playwright with a `webServer`) on the same
  Mac, five agents verifying at once (`test`, then `e2e`, each in its own
  worktree):

  | Setup | Wall time | vitest | Playwright |
  | --- | --- | --- | --- |
  | Run directly, default parallelism | 39–45 s | all 5 runs failed: 120 tests timed out | passed |
  | Run directly, 4 workers each | 36 s | passed | passed |
  | loadmaster, measured weights (capacity 12, test 4, e2e 2) | 39–40 s | passed | passed |
  | loadmaster, e2e weighted 8 (capacity 8) | 126 s | passed | passed |

  The same tests pass alone in 3 seconds. With weights that match what the
  tasks really use, loadmaster removed the false failures at no cost in time;
  an overestimated weight made everyone wait.
- Five real Claude Code sessions then ran lint, test and e2e three times each
  on that project with the measured weights: all 45 checks passed through
  loadmaster (longest wait 8.6 s), and the hook blocked a direct
  `npx vitest run`.

- In real use, in a project with 39 worktrees on a 14-core Mac: with weights
  that had been set to the whole machine, the checks waited up to 70 minutes,
  and a timing-sensitive test, weighted as the whole machine to run it alone,
  held back every job behind it while it waited. With weights taken from
  `loadmaster report` and that test's exclusion moved to a resource of
  capacity 1, the 90th-percentile wait of most tasks fell from 13 to 16
  minutes to 2.5 to 8 over the next 158 checks (90 minutes), and none failed
  under load. The
  load average when jobs ended rose (median 7.5 to 15), as more ran side by
  side; its peaks came with a task that used more cores than its weight.

Not verified yet: weeks of real use. loadmaster only starts commands, so other
tools should work too, but see [Limitations](#limitations) for tools that use
the terminal, leave daemons behind or start their own process groups.

## Install

Prebuilt binaries for macOS (Apple Silicon and Intel) and Linux (x86_64 and
arm64, statically linked) are on the
[releases page](https://github.com/ktanaka101/loadmaster/releases). With
[mise](https://mise.jdx.dev):

```bash
mise use -g github:ktanaka101/loadmaster
```

Append `@0.5.0` to pin a version. Without mise, download the archive for your
platform, such as `loadmaster-aarch64-apple-darwin.tar.gz`, and put the
`loadmaster` binary it contains on your `PATH`.

On Windows, use the Linux binary inside WSL2, and run the agents and their
checks in the same WSL2 distribution: loadmaster only coordinates runs that
share its state directory, so checks started on the Windows side, or in
another distribution, are not seen. This has not been tested yet.

To build from source instead (Rust 1.88 or later):

```bash
cargo install --locked --git https://github.com/ktanaka101/loadmaster --tag v0.5.0
```

## Quick start

Add `.loadmaster.toml` to the repository root and commit it, so every worktree
shares it:

```toml
# weight ≈ how many CPU cores the task keeps busy; measure yours
# (see "Choosing weights" below)
[tasks.lint]
weight = 1
cmd = ["npx", "biome", "check", "."]

[tasks.test]
weight = 4
cmd = ["npx", "vitest", "run", "--maxWorkers=4"]

[tasks.e2e]
weight = 2
cmd = ["npx", "playwright", "test", "--workers=4"]
```

Then:

```bash
loadmaster run test
```

```bash
loadmaster run test -- src/parser.test.ts
```

```bash
loadmaster status
```

While a job waits, loadmaster prints a line to stderr every 10 seconds that
says what it waits for, such as
``loadmaster: waiting to run `test` (weight 4, 2 jobs ahead) for 4 cores (6 of 8 in use)``.
Once it starts, the task's stdout and stderr pass straight through and
loadmaster exits with the task's exit code.

```bash
loadmaster check
```

checks `.loadmaster.toml` and the machine config without touching the queue,
and warns about weights that will make jobs wait on this machine; it also
works in CI.

## Rules for agents running checks

If you are an AI agent working in a repository that already uses loadmaster
(it has a `.loadmaster.toml`), this is all you need:

- **Run checks through loadmaster**: `loadmaster run <task>`. The tasks are
  the `[tasks.<name>]` tables in `.loadmaster.toml`; an unknown name prints
  the list.
- **Never run the underlying commands directly** (`npx vitest`, `npm test`, a
  script from `cmd`, ...). Other agents share this machine; running outside the
  queue overloads it and makes everyone's tests time out for no real reason.
- **Run each `loadmaster` command on its own.** Don't chain anything to it
  (`; echo $?`, `&& ...`): its exit code already is the check's exit code, and
  a chained command may need approval.
- **Pass extra arguments after `--`**: `loadmaster run test -- src/a.test.ts`.
- **Waiting is normal.** Lines like `loadmaster: waiting to run ...` on stderr
  mean other jobs are using the machine. Don't kill the command, don't give it
  a shorter timeout than your default, and don't treat the wait as a failure.
- **Act on the exit code:**

  | Exit code | Meaning | What to do |
  | --- | --- | --- |
  | 0 | The check passed. | Continue. |
  | 75 with `loadmaster: gave up waiting` on stderr | Gave up waiting; your place in line is kept for 120 s. Not a failure. | Run exactly the same command again, from the same directory, right away. |
  | 125 | loadmaster itself failed: unknown task, bad config, and so on. | Read the `loadmaster: error:` message; don't retry blindly. |
  | 126, 127 | The task's program could not be started. | A setup problem, e.g. dependencies not installed. |
  | 128 + n | The check was killed by signal n (143 = SIGTERM). | It was interrupted, not failed; run it again if needed. |
  | anything else | The check's own exit code. | Treat it as the check's result. |

- `loadmaster status` shows what is running and waiting, and what each
  waiting job waits for.
- **To stop your own checks, use `loadmaster cancel`**: `loadmaster cancel
  --here` stops every job started from your checkout, `loadmaster cancel 43`
  one job (IDs are in `loadmaster status`); if other agents work in the same
  checkout, cancel by ID. Never stop checks by matching command lines
  (`pkill -f vitest`): other agents run the same commands.

## Configuration

Configuration has two layers: weights belong to the repository, capacity
belongs to the machine. Agents in different repositories then share one
budget, and no single call can bring its own idea of the capacity.

### Repository: `.loadmaster.toml`

loadmaster looks for `.loadmaster.toml` in the current directory and then in
each parent directory, up to the root of the repository it is in (the nearest
directory with a `.git`, which linked worktrees and submodules have too), and
no further. Tasks run in the directory that contains it.

Commit it on every branch your worktrees use. A worktree inside the main
checkout (for example under `.claude/worktrees/`) whose branch has no
`.loadmaster.toml` does not pick up the main checkout's file, whose tasks would
run against the main checkout's code: loadmaster says so and fails.

| Key | Meaning |
| --- | --- |
| `weight` | Integer ≥ 1. Use "roughly how many CPU cores this task keeps busy" as the unit, in every repository, because all repositories on the machine share one capacity. |
| `cmd` | Array: the program and its arguments. No shell is involved. Arguments after `--` are appended to the end exactly as given. |
| `uses` | Optional: the resources the task uses, such as `{ e2e-db = 1 }`; see [Resources besides CPU](#resources-besides-cpu). |
| `ok_exit_codes` | Optional: the exit codes that mean the check passed, for the history log and `loadmaster report`; `[0]` if not set. A check that reports "differences found" with exit code 2 would set `[0, 2]`. loadmaster itself still exits with the task's code. |

Unknown keys are errors, so a typo fails instead of being ignored. That also
means an older loadmaster rejects a key added in a newer version, and branches
and machines often run different versions. Name the oldest version your file
needs at its top, and older versions say so instead of rejecting the key:

```toml
min_version = "0.5.0"
```

(loadmaster 0.4 and older do not know `min_version` either, and reject it as
an unknown key.) In CI, `loadmaster check` validates the file.

The weight does not change with the arguments: `loadmaster run test -- one.test.ts`
still counts as 4. Extra arguments come after the fixed ones, so leave out of
`cmd` whatever callers should be able to pass (a path, for example), and note
that relative paths are resolved from the directory containing
`.loadmaster.toml`. When the cost depends a lot on the arguments, as for a
whole test suite and a single test file, define a task for each:

```toml
[tasks.test]
weight = 8
cmd = ["npx", "vitest", "run"]

[tasks.test-file]
weight = 1
cmd = ["npx", "vitest", "run", "--maxWorkers=1"]
```

Choosing weights: measure, don't guess. Run the task alone and divide its CPU
time by its wall time:

```bash
/usr/bin/time -p npx playwright test --workers=4
```

`(user + sys) / real` is the number of cores it keeps busy. Guesses are easily
far off: in a demo project, vitest with 4 workers kept 3.8 cores busy, but
Playwright with 4 workers only 1.2, because browser tests mostly wait. A
weight that is too high costs real time: with e2e weighted 8 instead of 2, five
agents verifying at once took 126 seconds instead of 40, all of it spent
waiting. If memory or something else rather than CPU runs out, declare it as
a [resource](#resources-besides-cpu) instead of inflating the weight. Once
tasks run through loadmaster, `loadmaster report` shows the cores each one
really kept busy (`CORES`), measured the same way. It counts only the task's
own processes: load the task causes elsewhere, such as in Docker containers
or a database server it talks to, is not in it, so size such tasks by hand
(`docker stats` and the load average help).

### Resources besides CPU

When something other than CPU runs out, declare it as a resource and list
what each task uses:

```toml
[resources.e2e-db]
capacity = 2      # at most 2 at once on this machine
per_checkout = 1  # and 1 per checkout: they share its ports and database

[resources.memory]  # GiB; the machine config sets the capacity

[tasks.e2e]
weight = 2
uses = { e2e-db = 1, memory = 4 }
cmd = ["npx", "playwright", "test"]

[tasks.lint]
weight = 1
uses = { memory = 11 }
cmd = ["npx", "eslint", "."]
```

- `capacity` limits a resource on the whole machine. The machine config can
  set or override it; with no capacity anywhere, the resource is not limited
  machine-wide.
- `per_checkout` limits what the jobs of one checkout use together, for what a
  checkout has only one of: its dev server port, its local database, its build
  output. A checkout is a directory with `.loadmaster.toml`, so each worktree
  is one. A port that every worktree shares is a resource with `capacity = 1`
  instead.
- Every name in `uses` must be declared under `[resources]`. Names are shared
  by all repositories on the machine, like the capacity, so give
  project-specific resources project-specific names.
- An amount above a limit counts as the whole limit, like a weight above the
  capacity.
- To keep runs from overlapping, such as a timing-sensitive test that must run
  without another of its kind, give them a resource with `capacity = 1`. Don't
  give the task a weight at or above the capacity for that: it would also keep
  every other check off the machine, and while it waits, every job behind it
  waits too.

A job waiting for a resource holds back only that resource: a lint that needs
no database can start while an e2e run waits for one, as long as it leaves the
e2e run the CPU it will need. A job waiting for CPU still holds back everyone
behind it. See [How it works](#how-it-works).

That share is set aside for as long as the job waits, so keep the weights of
tasks that share a resource with a long-running task low: while one of them
waits, both its weight and the long-running task's are out of the capacity,
and only the rest is left for everything else. A weight above the capacity
minus the long-running task's is worse still: the waiting job is then short
of CPU too and holds back everyone behind it. `loadmaster check` points out
the tasks whose weights do that on this machine, and `loadmaster status`
shows what each waiting job sets aside.

Make manual operations on a resource tasks too, such as resetting the e2e
database by hand: `uses = { e2e-db = 1 }` then makes them wait for the runs
using it, and keeps new runs off it until they are done.

The running task gets two environment variables:

- `LOADMASTER_WEIGHT`: the task's weight, so the task can size its own
  parallelism to match the slot it holds.
- `LOADMASTER_HELD`: the ID of the job holding the slot (see
  [Nested runs](#nested-runs) and
  [Guarding a script](#guarding-a-script)).

It also gets `LOADMASTER_STATE_DIR`, and `LOADMASTER_CONFIG` if that was set,
as absolute paths, so a `loadmaster` it runs finds the same queue and machine
config, though the task runs in the directory of `.loadmaster.toml`.

If you need shell features, for example to use `LOADMASTER_WEIGHT`, call a
shell explicitly. The trailing `"sh"` becomes `$0`, so the extra arguments
arrive as `"$@"`:

```toml
[tasks.test]
weight = 4
cmd = ["sh", "-c", "exec npx vitest run --maxWorkers=$LOADMASTER_WEIGHT \"$@\"", "sh"]
```

### One check per task

Make each check a task of its own, and run a series of checks, such as a
"verify everything" script, as one `loadmaster run` per check rather than as
one task. A job holds its weight for as long as it runs: a series defined as
one task needs a weight that covers its heaviest step and holds it through
every step, and while several such series wait in line, every check behind
them waits for all of them. Run one by one, each check holds only its own
weight, and checks that other agents started in the meantime run between the
steps:

```bash
#!/bin/sh
set -e
export LOADMASTER_MAX_WAIT=  # no limit for the steps; see below
loadmaster run lint
loadmaster run test
loadmaster run e2e
```

This is a trade-off, made on purpose: a series gets back in line for each
step, so it finishes later than if it held the machine throughout, and in
exchange the short checks that agents run while they iterate don't wait
behind it.

The steps of a script should not give up waiting: exit code 75 would stop the
script like any other failure, and running it again would repeat the steps
that already passed. Set `LOADMASTER_MAX_WAIT` to an empty string for them, as
above, and give the script itself a timeout that covers the waits. An agent
that needs to keep its place across tool timeouts can run the checks one
command at a time instead.

Not every check has to be a task. A job waiting for CPU holds back every job
behind it, so a quick check queued behind a heavy one waits for it, even with
cores free; that is what keeps heavy jobs from starving. A check that keeps a
core busy for a few seconds, such as a single test file, can instead run
outside the queue and never wait. Leave room for such checks in the capacity
(the default keeps 2 cores out of it).

### Machine: `~/.config/loadmaster/config.toml`

```toml
capacity = 8

[resources]
memory = 48  # GiB
e2e-db = 3   # overrides the repository's default
```

If `capacity` is not set, it defaults to the number of CPU cores minus 2
(at least 1). The capacity only counts work that goes through loadmaster;
editors, dev servers, Docker and the agents themselves still need room. On
Apple Silicon the core count includes efficiency cores; the number of
performance cores (`sysctl hw.perflevel0.physicalcpu`) is often a better
starting point.

`measure_only = true` makes every job start at once, whatever the capacity
and its place in line, and only records it, to get a baseline; see
[Checking that it helps](#checking-that-it-helps).

`$XDG_CONFIG_HOME` is honored, and `LOADMASTER_CONFIG` overrides the path; a
file it names must exist, while a missing file in the default place means all
defaults. Like `.loadmaster.toml`, the file can name the oldest loadmaster
that reads it with `min_version`, for a file shared across machines.

The queue lives in `~/.local/state/loadmaster/state.db` (`$XDG_STATE_HOME`,
`LOADMASTER_STATE_DIR`). Next to it, `history.jsonl` gets one line per job
(task, weight, resources, capacity, wait and run time, CPU time, load average,
signal, exit code and whether it counts as passed), which `loadmaster report`
summarizes. If `loadmaster` is killed
before it can write its line, the process that later reclaims the job writes
one with outcome `lost`.

## How it works

- **Admission**: jobs are considered in line order. A job starts when its
  weight and each resource it uses fit in what running jobs leave free and in
  what the jobs ahead of it set aside: a job waiting in line sets aside what
  it needs wherever that fits, and holds back entirely what it is short of.
  No job is ever delayed by jobs that got in line after it. Every task uses
  CPU, so a small job never overtakes a big job waiting for CPU, and the big
  job starts as soon as enough running jobs finish; the price is that a quick
  lint can wait behind an e2e run even while some cores are free. A job
  waiting only for a resource, such as a database stack, lets jobs that don't
  need it pass, as long as they leave it the CPU it needs.
- **Crashes**: state is a per-user SQLite database. Every update first checks
  that the processes holding places and slots are still alive (comparing PID
  and process start time, so a reused PID can't fool it):
  - a waiting job disappears from the line once its `loadmaster` process dies;
  - a running job keeps its slot while its `loadmaster` process **or** any
    process in the task's process group is alive. If only `loadmaster` is
    killed, or the task's main process exits but leaves other processes
    running, the slot stays taken until they have all exited.
  - a task is recorded before it starts, so there is no moment in which it
    runs untracked.
- **Signals**: SIGINT, SIGTERM or SIGHUP while waiting leaves the line. While
  the task runs, they are forwarded to the task's process group, and
  loadmaster waits for the task to exit before freeing the slot. Signals that
  were ignored when loadmaster started (as under `nohup`) stay ignored. Every
  process in the group gets the signal, as with Ctrl-C in a terminal, so a
  wrapper script that forwards signals to its child makes the child get each
  one twice. Have the wrapper `exec` the child instead, or write handlers that
  survive a second signal (a Node.js handler added with `process.once` does
  not: the second signal ends the process).
- **Unknown tasks and broken config files** fail immediately instead of
  waiting.
- **A weight above the capacity** counts as the whole capacity: the task runs
  alone, with a note on stderr. Weights are committed with the repository, so
  this keeps them usable on a smaller machine, such as a CI runner.
- **Why a job waits** is worked out by the admission rule itself, which
  `loadmaster status` and the waiting messages report: a dimension in use by
  running jobs, one set aside for jobs ahead, or one a job ahead waits for
  too, with the jobs that cause it.

### Nested runs

If a task itself calls `loadmaster run`, the inner call must not queue: it
would wait forever for the slot its own parent holds. The inner call sees
`LOADMASTER_HELD` and runs directly in that job's slot, without taking
another one, so it has to fit there:

- the job must be running and must have been started from the same checkout,
  so a stale, made-up or leaked `LOADMASTER_HELD` is not a free pass;
- the inner task's weight must not exceed the outer task's, and every resource
  it uses must be in the outer task's `uses`, with at least the same amount.

Otherwise the inner call fails with exit code 125 and says what to change.
Give the outer task a weight and `uses` that cover everything it runs. Nested
runs keep a task that happens to call loadmaster, such as a script that is
also run on its own, from deadlocking; to run several checks in a row, prefer
[one `loadmaster run` per check](#one-check-per-task).

### Giving up without losing your place

```bash
loadmaster run --max-wait 600 test
```

If the job has not started after 600 seconds, loadmaster exits with code 75
but keeps the job's place in line for 120 seconds. Running the same command
again from the same directory resumes that place instead of joining the back of
the line. "The same command" means the same caller directory, task name and
extra arguments; if the task's weight or resources changed in the meantime,
the re-run gets in line again instead. A kept place still holds back jobs
behind it, as if it were waiting, until it is resumed or expires. The 120
seconds are fixed, and each resumed run may again wait up to `--max-wait`.

`LOADMASTER_MAX_WAIT` sets the default for `--max-wait`; set to an empty
string, it means no limit. You can tell loadmaster's 75 from a task that
happens to exit with 75 by the `loadmaster: gave up waiting` line on stderr.

## Commands

```text
loadmaster run [--max-wait SECONDS] <TASK> [-- ARGS...]
loadmaster status
loadmaster cancel [--kill] (<ID>... | --here)
loadmaster check
loadmaster clean
loadmaster report [--since TIME] [--until TIME] [--dir TEXT]
loadmaster held [--uses RESOURCE]
```

- `run`: run a task once its weight and resources fit.
- `status`: show used/total capacity, resources in use, the load average,
  every running, waiting or parked job, and what each waiting job waits for.
- `cancel`: stop jobs, by ID or (`--here`) every job started from this
  checkout except the one `cancel` itself runs in, and wait until they have
  left the queue. It sends SIGTERM to the job's `loadmaster`, so a waiting job
  leaves the line and a running job's task gets the signal as described under
  [Signals](#how-it-works), with the slot freed once it exits; a kept place
  (exit code 75) is dropped. `--kill` sends SIGKILL to a running task
  instead, for one that does not stop; it gets no chance to clean up.
  `loadmaster report` leaves canceled checks out. Exits with 0 once the jobs
  are gone.
- `check`: validate `.loadmaster.toml` and the machine config, list the tasks
  and resources with their limits on this machine, and warn about weights that
  take the whole machine or crowd out other tasks while waiting for a
  resource. It does not touch the queue. Exits with 0 if the configs are
  valid, warnings or not, and with 125 if not.
- `clean`: reclaim places and slots whose processes are gone. Every update
  already does this, so you should rarely need it.
- `report`: summarize the history log per task. `--since` and `--until`
  limit it to checks that got in line in a period, given as a time ago
  (`12h`, `7d`, `2w`) or a local time (`2026-10-12T16:44`, `2026-10-12`), for
  example around a change of settings; `--dir myapp` keeps checks run from
  directories whose path contains `myapp`. See
  [Checking that it helps](#checking-that-it-helps).
- `held`: exit with 0 if this process runs inside a loadmaster job started
  from this checkout (and, with `--uses`, using that resource), and with 1
  (saying why on stderr) if not; see [Guarding a script](#guarding-a-script).
  Its own failures exit with 125.

```text
$ loadmaster status
6/8 in use (capacity set in /Users/you/.config/loadmaster/config.toml)
e2e-db: 1/2 in use
load average 22.9 over the last minute, on 14 cores: more than the cores, often from work outside the queue or tasks that use more cores than their weight (see `loadmaster report`); on Linux, processes waiting for disks count too

ID  STATE    TASK  WEIGHT  USES      PID    PGID   FOR   DIR
41  running  test  4       -         10687  10689  3.0s  /Users/you/src/app-wt1
42  running  e2e   2       e2e-db=1  10700  10702  2.5s  /Users/you/src/app-wt2
43  waiting  e2e   2       e2e-db=1  10753  -      2.0s  /Users/you/src/app-wt2
44  waiting  lint  1       -         10772  -      1.4s  /Users/you/src/app-wt3

why jobs wait:
  #43 `e2e` waits for `e2e-db` in its checkout (1 of 1 in use by #42 `e2e`); sets aside 2 cores and `e2e-db` meanwhile
  #44 `lint` waits for 1 core (2 free, but 2 set aside for #43 `e2e`)
```

Job 43 waits because `app-wt2` already has its one `e2e-db`. Job 44 waits
although 2 cores are free: they are set aside for job 43, which needs them as
soon as `app-wt2`'s database is free. `FOR` is the time spent in the current
state. The load average counts everything on the machine; well above the
capacity in use, it points to work outside the queue, here a build run
directly.

### Exit codes

| Code | Meaning |
| --- | --- |
| task's code | The task ran and exited normally. |
| 128 + n | The task was killed by signal n, or loadmaster was interrupted by signal n while waiting. |
| 75 | `--max-wait` elapsed. The job keeps its place in line; run the same command again to resume. |
| 125 | loadmaster itself failed: unknown task, invalid config, a config that needs a newer loadmaster (`min_version`), no `.loadmaster.toml` in this repository, invalid `LOADMASTER_HELD` or `LOADMASTER_MAX_WAIT`, a nested task that does not fit in its holder's slot, bad arguments. |
| 126 | The task's program could not be executed. |
| 127 | The task's program was not found. |

A task can exit with 75, 125, 126 or 127 itself; loadmaster's own messages on
stderr start with `loadmaster:`.

## Routing existing commands through loadmaster

Agents and people run whatever the repository offers: `npm test`, a Makefile
target, a script under `scripts/`. Point those entry points at loadmaster, and
every run goes through the queue, whoever starts it and whatever the agent,
without relying on permission rules or hooks:

```json
{
  "scripts": {
    "lint": "loadmaster run lint --",
    "test": "loadmaster run test --",
    "test:e2e": "loadmaster run e2e --"
  }
}
```

The trailing `--` lets `npm test -- src/a.test.ts` pass its arguments on to
the task. The tasks' `cmd` must call the tools themselves
(`["npx", "vitest", "run"]`), never these scripts: a task whose `cmd` is
`["npm", "test"]` calls itself forever, because the inner `loadmaster run test`
runs directly inside the outer one's slot (see [Nested runs](#nested-runs)).

### Guarding a script

A script that must only run through the queue, such as one that starts an
e2e database, can check that it does:

```bash
loadmaster held || exit 1
```

`loadmaster held` succeeds when `LOADMASTER_HELD`, which every task gets,
names a running job that was started from the same checkout (the directory
with `.loadmaster.toml`); nested runs count. Otherwise it exits with 1 and
says why on stderr, including how to run the task through loadmaster. This
works whatever starts the script, unlike permission rules and hooks, which
only see an agent's own commands. A stale or made-up `LOADMASTER_HELD`, or one
from another checkout, fails; one copied into another shell of the same
checkout still passes while its job runs, so this too is a guardrail against
accidents, not a security boundary. `loadmaster held --uses e2e-db` also
requires the job to use `e2e-db`, for a script that needs that resource
held.

Tools you don't control, such as `npx vitest` run directly, still bypass the
queue; in Claude Code, [blocking direct execution](#3-block-direct-execution)
catches those.

### Where loadmaster must be installed

Scripts that go through loadmaster need it wherever they run: on every
developer's machine, in CI, in containers and in agent sandboxes. Either route
only the scripts that agents use and let CI call the tools directly, or install
loadmaster in CI too (a prebuilt binary or mise). On a small CI runner, a
weight above the capacity simply runs alone. An agent that sandboxes its
commands must also be allowed to write to the state directory, or loadmaster
fails with exit code 125 (see the sandbox setting for Claude Code below).

### Switching over with existing worktrees

Each worktree runs the scripts of its own branch. Until the loadmaster setup
has reached every branch you work on, worktrees on older branches keep running
checks the old way, and loadmaster cannot see that load. Merge the setup into
those branches (or rebase them) before relying on it. If they used another
coordination mechanism, such as a lock file, keep taking that lock inside the
new tasks' `cmd` while old worktrees remain, so that both sides still see each
other; remove it once the last old worktree is gone.

## Using it with Claude Code

### 1. Tell the agents

In `CLAUDE.md` (or `AGENTS.md` for other agents), or point them at
[Rules for agents running checks](#rules-for-agents-running-checks):

```markdown
Run lint, tests and e2e only through loadmaster: `loadmaster run lint`,
`loadmaster run test`, `loadmaster run e2e`. To run part of the suite, pass
arguments after `--`, e.g. `loadmaster run test -- src/parser.test.ts`.
Run each `loadmaster` command on its own, without chaining other commands to
it; its exit code is the check's exit code.
If it exits with code 75, run exactly the same command again: your place in
line is kept.
```

The "on its own" line matters: agents like to append `; echo "exit=$?"`, and
a chained command no longer matches an allow rule such as
`Bash(loadmaster run *)`, so it stops for approval.

### 2. Make room for the wait

The Bash tool's timeout includes the time spent waiting in line. Raise it in
`~/.claude/settings.json`, and set `LOADMASTER_MAX_WAIT` below it, so that
loadmaster gives up first (keeping the place) instead of being killed:

```json
{
  "env": {
    "BASH_DEFAULT_TIMEOUT_MS": "1800000",
    "BASH_MAX_TIMEOUT_MS": "3600000",
    "LOADMASTER_MAX_WAIT": "600"
  }
}
```

Here a call can wait up to 10 minutes and still leave 20 minutes for the task
itself under the default 30-minute timeout.

### 3. Block direct execution

Deny the direct commands and allow loadmaster, in `.claude/settings.json` or
`~/.claude/settings.json`:

```json
{
  "permissions": {
    "allow": [
      "Bash(loadmaster run *)",
      "Bash(loadmaster status)",
      "Bash(loadmaster cancel *)"
    ],
    "deny": ["Bash(npx vitest *)", "Bash(npx playwright *)", "Bash(npx biome *)"]
  }
}
```

If Claude Code runs Bash commands in its sandbox, `loadmaster` fails with exit
code 125 ("cannot create ...: Operation not permitted") until the sandbox may
write to the state directory. Allow it in `~/.claude/settings.json`:

```json
{
  "sandbox": {
    "filesystem": {
      "allowWrite": ["~/.local/state/loadmaster"]
    }
  }
}
```

Claude Code ignores `permissions.allow` in a project's `.claude/settings.json`
until that workspace is trusted, and every git worktree is a workspace of its
own: open each worktree interactively once and accept the trust dialog (for
headless `claude -p` runs, `--allowedTools` works regardless).

Deny rules also match commands inside `&&` chains and subshells, but they match
text: `npm test`, `npx -y vitest` or `./node_modules/.bin/vitest` still get
through. A PreToolUse hook catches those too. Save this as
`.claude/hooks/require-loadmaster.sh` in the repository and make it executable
(it needs `jq`):

```bash
#!/usr/bin/env bash
# Claude Code PreToolUse hook: blocks lint/test/e2e commands that bypass loadmaster.
cmd=$(jq -r '.tool_input.command // empty')

# Commands that must go through loadmaster. Adjust to your project.
block='(^|[^[:alnum:]_.-])(vitest|playwright|biome|jest)([^[:alnum:]_./-]|$)'
block+='|(^|[^[:alnum:]_.-])(npm|pnpm|yarn|bun)( run)? (test|lint|e2e)([^[:alnum:]_:-]|$)'
# A single loadmaster command, with nothing chained to it.
allow='^loadmaster (run|status|clean)( |$)'
chain='[;&|`]|[$][(]|[[:cntrl:]]'

if [[ $cmd =~ $block ]] && ! { [[ $cmd =~ $allow ]] && ! [[ $cmd =~ $chain ]]; }; then
  echo "Run this through loadmaster instead (e.g. \`loadmaster run test\`); see .loadmaster.toml for the tasks." >&2
  exit 2
fi
exit 0
```

and register it in `.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "\"$CLAUDE_PROJECT_DIR\"/.claude/hooks/require-loadmaster.sh" }
        ]
      }
    ]
  }
}
```

Exit code 2 blocks the call and shows the message to Claude. The hook blocks
`npm test`, `cd x && npx vitest` and `loadmaster run test && npx vitest`, and
allows a plain `loadmaster run test`. Its patterns are plain text matches, so
adjust them to your scripts: as written, they miss `npm run test:unit`, and
they also block harmless commands such as `grep vitest`. If your package
scripts already go through loadmaster, drop the second `block` line. Without
`jq` the hook lets everything through. Like permission rules, it is a
guardrail against accidents, not a security boundary.

## Checking that it helps

loadmaster trades waiting for fewer overloaded runs. Whether that pays off
depends on your checks and on how often agents run them at once, so measure it
on your machine:

1. Record a baseline: add `measure_only = true` to
   `~/.config/loadmaster/config.toml` and work as usual for a few days. Every
   run still goes through loadmaster and is logged, but none waits, so the
   machine is as loaded as it would be without loadmaster.
2. Remove the line and work as usual for about as long.
3. Run `loadmaster report`.

The report has one section per setting (measure-only or admission, and the
capacity), and one row per task and setting: a task run with another weight
or other resources, as by an old branch during a switch-over, gets a row of
its own. Tasks are grouped by name across all repositories; if two projects
both have a `test` task, look at one at a time with `--dir`. To compare the
time before and after a change, use `--until` and `--since` with the time of
the change. Checks stopped with `loadmaster cancel` are left out and only
counted: they were stopped on purpose. This report comes
from a short demo, five agents each running `test` and then `e2e` at once:

```text
$ loadmaster report
history: /Users/you/.local/state/loadmaster/history.jsonl

== measure-only (capacity 12): 10 check(s), 2026-10-11 11:40 to 2026-10-11 11:40
weight in use: above capacity 35% of the time checks ran, peak 20
load average when tasks ended: median 10.7, p90 16.3 (14 CPUs)

TASK  WEIGHT  USES  CORES MED  CORES P90  CHECKS  PASSED  FAILED  KILLED  LEFT LINE  NOT RUN  RUN MED  RUN P90  WAIT MED  WAIT P90  WAIT MAX
e2e        2     -        1.3        1.4       5       5       0       0          0        0      24s      25s      0.0s      0.0s      0.0s
test       4     -        2.2        2.2       5       5       0       0          0        0      13s      13s      0.1s      0.2s      0.2s

== admission (capacity 12): 10 check(s), 2026-10-11 11:41 to 2026-10-11 11:42
demand: above capacity 23% of the time checks ran, peak 20 (held back by loadmaster; in use peaked at 12)
load average when tasks ended: median 13.0, p90 17.0 (14 CPUs)

TASK  WEIGHT  USES  CORES MED  CORES P90  CHECKS  PASSED  FAILED  KILLED  LEFT LINE  NOT RUN  RUN MED  RUN P90  WAIT MED  WAIT P90  WAIT MAX
e2e        2     -        1.3        1.3       5       5       0       0          0        0      24s      25s      0.2s       10s       10s
test       4     -        2.9        2.9       5       5       0       0          0        0      10s      10s      0.1s       10s       10s
```

What to compare between the two periods:

- **FAILED and KILLED**: the false failures and timeouts loadmaster is meant
  to remove (along with real failures, which both periods have). FAILED
  counts exit codes not in the task's `ok_exit_codes`. KILLED means ended by a
  signal while running, usually a tool timeout, including runs whose
  `loadmaster` was killed while the task ran.
- **LEFT LINE**: checks that stopped before they started: a tool timeout or
  an interrupt while waiting, or a place given up (exit code 75) and not
  re-run. This is what waiting cost; it should be near 0 in the measure-only
  period. NOT RUN counts checks that could not start at all, such as a
  missing program.
- **RUN MED / RUN P90** (passed checks): contention makes runs slower; under
  admission they should come close to the time a task takes alone.
- **WAIT** (from first getting in line to starting, re-runs after exit code 75
  included; for checks that never started, until they ended): what admission
  costs. If it nears your Bash tool timeout, raise the capacity or lower
  weights.
- **CORES MED** and **CORES P90** against **WEIGHT**: the cores a task really
  kept busy (CPU time divided by run time), for the median run and the
  busiest tenth of runs. A weight far above them makes jobs wait for nothing.
  A `!` after CORES P90 marks a task whose busiest tenth kept more than 1.2
  times its weight busy: those runs use more than their slot accounts for, so
  the machine can run above its capacity; raise the weight. Read them from the admission
  period; under contention they read low.
- **above capacity ... of the time checks ran**: how often the weights of the
  checks running at once (measure-only) or asking to run (admission) added up
  to more than the capacity. It is only as meaningful as the weights are; the
  load average and the run times show whether the machine really struggled.
  Near 0% means the agents rarely overlap and loadmaster has little to do.
- **load average**: the whole machine's 1-minute load average when tasks
  ended, including everything outside loadmaster. A rough sign of how sluggish
  the machine was.

In the demo, admission made `test` runs faster (13 s to 10 s) at the price of
waits of up to 10 s, with no failures either way, and showed that `test` uses
about 3 cores rather than 4. Periods with different kinds of work are not
comparable, and a few dozen checks are noisy; compare similar weeks.

## Limitations

loadmaster is deliberately small:

- One machine. Weights are static; there are no priorities and no measurement
  of actual CPU or memory use.
- The capacity only accounts for work that goes through loadmaster. Work
  outside it shows in the load average that `loadmaster status` prints, but
  does not hold jobs back: the load average lags by a minute and counts more
  than CPU, and admission based on it would let work outside the line delay
  jobs indefinitely and in no predictable order. Route that work through the
  queue instead.
- Tasks run without terminal input (stdin is `/dev/null` when it would be a
  terminal). A task that opens the terminal itself, for a password prompt or a
  pager, is stopped by the OS; Ctrl-C still ends it.
- A task that leaves a daemon running in its own process group holds its slot
  until the daemon exits (loadmaster warns about it).
- Processes a task starts in a process group or session of their own are not
  tracked after the task's own group is gone, and do not always exit with it:
  when a Playwright run is stopped with SIGTERM (as a Claude Code timeout
  does), its `webServer` keeps running and holding its port, so the next run
  fails with "already used". This happens without loadmaster too; SIGINT
  shuts it down cleanly.
- `history.jsonl` is never rotated.

## Development

```bash
cargo test
```

Besides unit tests, this runs integration tests that start real `loadmaster`
processes: many jobs at once, a big job among a stream of small ones, `kill -9`
of waiting and running jobs, nested runs, resources, `loadmaster held`,
`cancel` and `check`, exit codes and config errors. Design decisions and their reasons are in
[docs/DECISIONS.md](docs/DECISIONS.md).

A soak test simulates five agents calling `loadmaster run` the way Claude
Code's Bash tool does (timeouts that SIGTERM and then SIGKILL the process
group, re-runs after exit code 75, occasional crashes), in two checkouts with
two resources besides CPU. It checks from inside the tasks that no limit is
ever exceeded, that every task gets through and that nothing is left behind:

```bash
LOADMASTER_SOAK_SECS=600 cargo test --release --test soak -- --ignored --nocapture
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
