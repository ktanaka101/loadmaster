//! `loadmaster guide`: how an agent runs checks here, from the binary
//! itself, so it always matches the version installed. It is what an
//! instruction file points agents at, and it says the rules even where
//! there is no config, or a broken one: a guide that needed a working
//! setup could not say how to fix one.

use crate::*;
use loadmaster::usual::{Setting, Usual};

pub(crate) fn guide() -> Result<i32> {
    let cwd = env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
    let mut out = format!(
        "loadmaster {}: how AI agents run checks on this machine. Read it once per session, and \
         again after `.loadmaster.toml` changes.\n\n",
        env!("CARGO_PKG_VERSION")
    );
    let machine = MachineConfig::load();
    let state_dir = config::state_dir().ok();
    match &machine {
        Ok(machine) => {
            out += &format!(
                "Machine: capacity {} cores ({}){}",
                machine.capacity,
                machine.capacity_source,
                if machine.measure_only {
                    "; measure-only: checks start at once and are only recorded"
                } else {
                    ""
                }
            );
        }
        Err(e) => out += &format!("Machine config: invalid ({e})"),
    }
    if let Some(dir) = &state_dir {
        out += &format!("; the queue every agent here shares is in {}", tilde(dir));
    }
    out += ".\n";
    out += &match RepoConfig::locate(&cwd) {
        Err(e) => setup(&e),
        Ok(path) => match RepoConfig::load(&path) {
            Err(e) => format!(
                "\n{}: invalid ({e}). `loadmaster check` says what to fix.\n",
                tilde(&path)
            ),
            Ok(repo) => tasks(&repo, machine.as_ref().ok(), state_dir.as_deref()),
        },
    };
    out += RULES;
    print(&out);
    Ok(0)
}

/// The tasks of `repo`, one line each: what an agent needs to choose one
/// and run it.
fn tasks(repo: &RepoConfig, machine: Option<&MachineConfig>, state_dir: Option<&Path>) -> String {
    let usual = state_dir.map_or_else(Usual::default, Usual::load);
    let checkout = fs::canonicalize(repo.dir()).unwrap_or_else(|_| repo.dir().to_owned());
    let mut out = format!(
        "\nTasks of {} (run one with `loadmaster run <task>`; it runs in {}; give extra \
         arguments after `--`, e.g. `loadmaster run <task> -- path/to/one.test.ts`):\n",
        tilde(repo.path()),
        tilde(repo.dir()),
    );
    if repo.tasks().next().is_none() {
        out += "- none defined\n";
    }
    for task in repo.tasks() {
        let capacity = machine.map_or(task.weight, |m| m.capacity);
        let weight = task.weight.min(capacity);
        let mut facts = vec![format!("weight {weight}")];
        if let Some(least) = task
            .min_weight
            .map(|l| l.min(weight))
            .filter(|&l| l < weight)
        {
            facts.push(format!("may start with {least}"));
        }
        if !task.uses.is_empty() {
            let uses: Vec<String> = task.uses.keys().map(|name| format!("`{name}`")).collect();
            facts.push(format!("uses {}", uses.join(", ")));
        }
        if task.ok_exit_codes != [0] {
            let codes: Vec<String> = task.ok_exit_codes.iter().map(i32::to_string).collect();
            facts.push(format!("passes on exit {}", codes.join(" or ")));
        }
        if let Some(run) = usual.run_time(&Setting::of_task(&checkout, task, capacity)) {
            facts.push(format!("usually {}", format_duration(run.median)));
        }
        out += &format!("- {} ({})", task.name, facts.join("; "));
        if let Some(description) = &task.description {
            out += &format!(": {description}");
        }
        out += "\n";
    }
    out
}

/// What to say where there is no `.loadmaster.toml` to be found.
fn setup(error: &Error) -> String {
    format!(
        "\nNo checks here go through loadmaster: {error}.\n\
         To set it up, add `.loadmaster.toml` at the repository root and commit it, one table \
         per check:\n\
         \n\
         [tasks.test]\n\
         description = \"Unit tests; pass test files after --\"  # optional, one line\n\
         weight = 4  # the CPU cores it keeps busy; `loadmaster report` measures them\n\
         cmd = [\"npx\", \"vitest\", \"run\", \"--maxWorkers=4\"]\n\
         \n\
         Then `loadmaster check` validates it, and `loadmaster doctor` checks that your tool \
         lets loadmaster work. Until then, run this checkout's checks as its own docs say.\n"
    )
}

/// The rules, the same everywhere.
const RULES: &str = "
Rules:
1. Run checks only through loadmaster, never the commands they run (`npx vitest`, `npm test`, a \
script from `cmd`): other agents share this machine, and a check run outside the line overloads \
it and makes everyone's checks time out.
2. One `loadmaster` command per tool call, with nothing chained to it (`; echo $?`, `&& ...`): \
its exit code is the check's.
3. Waiting is normal: `loadmaster: #42 `test` is waiting ...` means other jobs use the machine; \
the lines say what it waits for and behind whom. Do not stop it or start it again. If your tool \
hands back control or runs it in the background, check on that session; do not start a second \
one.
4. Leave room for the wait: a tool timeout must cover the wait, the run and its clean-up. Set \
LOADMASTER_MAX_WAIT (seconds) below it, so loadmaster gives up first and keeps your place.
5. Read how it ended from its last `loadmaster:` line, not only the exit code (a task may exit \
75 or 125 itself):
   - `passed` or `failed (exit N)`: the check's result.
   - `gave up ... (exit 75)`: the `loadmaster: next:` line after it says what to run, word for \
word, and from where; do it right away. The place is kept 120 s for the same task, arguments \
and settings.
   - `loadmaster: error:` (exit 125): loadmaster itself failed; do what its `loadmaster: next:` \
line says, if it has one; do not retry blindly.
   - `could not start` (exit 126 or 127): the task's program is missing or not executable.
   - `was killed by SIG...` (exit 128+n): interrupted, not failed; run it again if needed.
   - `stopped as asked`: it got its stop signal (a cancel, or your tool's timeout).
6. To stop your own checks: `loadmaster cancel <ID>` (the ID is on its first line, `#42`, and in \
`loadmaster status`). `loadmaster cancel --here` stops every job of this checkout, other agents' \
working in it too. Never stop checks by command line (`pkill -f vitest`): other agents run the \
same commands.
7. Weights are CPU cores. A task with a min_weight may start with fewer and learns how many from \
LOADMASTER_WEIGHT, so it must size its workers by it. Change `.loadmaster.toml` only if that is \
your task: every agent shares its weights and resources.

Where to look:
- `loadmaster status`: what runs and waits, and why; what became of this checkout's latest \
checks (`--json` for tools).
- `loadmaster check`: whether `.loadmaster.toml` and the machine config are valid, and what \
they mean here.
- `loadmaster doctor`: whether your tool's settings let loadmaster work (its sandbox, its \
timeouts).
- `loadmaster report`: how checks fared over time; `loadmaster tune`: what other weights or \
min_weights would have done, before you change them.
";
