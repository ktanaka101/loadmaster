//! Configuration, split in two layers:
//!
//! - **Repository** (`.loadmaster.toml`, committed): tasks, their weights and
//!   the resources they use.
//! - **Machine** (`~/.config/loadmaster/config.toml`): the capacity shared by
//!   every repository and worktree on this machine, and resource capacities.
//!
//! Weights are measured in "roughly how many CPU cores the task keeps busy",
//! so that tasks from different repositories draw from the same budget.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

pub const REPO_CONFIG_FILE: &str = ".loadmaster.toml";

/// Overrides the machine config file path.
pub const CONFIG_ENV: &str = "LOADMASTER_CONFIG";
/// Overrides the directory holding the queue database and history log.
pub const STATE_DIR_ENV: &str = "LOADMASTER_STATE_DIR";

/// Cores left out of the default capacity for everything loadmaster does not
/// see: editors, dev servers, Docker, the agents themselves.
const RESERVED_CORES: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub name: String,
    /// What it checks, and how to run part of it, in one line: what
    /// `loadmaster guide` tells an agent choosing a task.
    pub description: Option<String>,
    pub weight: u32,
    /// The least cores it may start with, if it says (`min_weight`): its
    /// consent to fewer than its weight, which it sizes itself by through
    /// `LOADMASTER_WEIGHT`. Without it, it starts with its weight.
    pub min_weight: Option<u32>,
    /// Resources the task uses, by name, all declared in the same file.
    pub uses: BTreeMap<String, u32>,
    /// Exit codes that mean the check passed (`[0]` unless set), for the
    /// history log and `loadmaster report`.
    pub ok_exit_codes: Vec<i32>,
    /// The signal that asks the task to stop and clean up (SIGTERM unless
    /// set): what a cancel, or a SIGTERM to loadmaster, delivers.
    pub stop_signal: i32,
    /// Program and arguments. Never empty.
    pub cmd: Vec<String>,
}

/// A resource declared under `[resources.NAME]` in `.loadmaster.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    /// The machine-wide capacity, unless the machine config sets one.
    pub capacity: Option<u32>,
    /// At most this much is used at once by jobs from one checkout.
    pub per_checkout: Option<u32>,
}

/// A parsed `.loadmaster.toml`.
#[derive(Debug)]
pub struct RepoConfig {
    path: PathBuf,
    tasks: BTreeMap<String, Task>,
    resources: BTreeMap<String, Resource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoFile {
    /// Checked before the file is read strictly; see [`check_min_version`].
    #[serde(default, rename = "min_version")]
    _min_version: Option<String>,
    #[serde(default)]
    tasks: BTreeMap<String, TaskFile>,
    #[serde(default)]
    resources: BTreeMap<String, Resource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    description: Option<String>,
    weight: u32,
    min_weight: Option<u32>,
    #[serde(default)]
    uses: BTreeMap<String, u32>,
    ok_exit_codes: Option<Vec<i64>>,
    stop_signal: Option<String>,
    cmd: toml::Value,
}

impl RepoConfig {
    /// Finds and loads the repository's `.loadmaster.toml` (see
    /// [`RepoConfig::find`]).
    pub fn discover(start: &Path) -> Result<RepoConfig> {
        Self::locate(start).and_then(|path| Self::load(&path))
    }

    /// Like [`RepoConfig::find`], but not finding it is an error that says
    /// where the search looked.
    pub fn locate(start: &Path) -> Result<PathBuf> {
        match search(start) {
            Search::Found(path) => Ok(path),
            Search::NotFound { repo_root, beyond } => Err(Error::RepoConfigNotFound {
                start: start.to_owned(),
                repo_root,
                beyond,
            }),
        }
    }

    /// The path of `.loadmaster.toml` in `start` or the nearest parent
    /// directory, without reading it. The search stops at the root of the
    /// repository `start` is in, the nearest directory with a `.git` entry
    /// (a linked worktree or a submodule has one too): a file further up
    /// belongs to another repository, such as the main checkout a worktree
    /// lives in, and its tasks would run against that repository's code.
    pub fn find(start: &Path) -> Option<PathBuf> {
        match search(start) {
            Search::Found(path) => Some(path),
            Search::NotFound { .. } => None,
        }
    }

    pub fn load(path: &Path) -> Result<RepoConfig> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::ReadConfig {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(path, &text)
    }

    fn parse(path: &Path, text: &str) -> Result<RepoConfig> {
        let invalid = |message: String| Error::InvalidConfig {
            path: path.to_owned(),
            message,
        };
        check_min_version(path, text)?;
        let file: RepoFile = toml::from_str(text).map_err(|e| invalid(explain_toml_error(e)))?;
        for (name, resource) in &file.resources {
            for (key, limit) in [
                ("capacity", resource.capacity),
                ("per_checkout", resource.per_checkout),
            ] {
                if limit == Some(0) {
                    return Err(invalid(format!(
                        "resource `{name}`: {key} must be at least 1"
                    )));
                }
            }
        }
        let tasks = file
            .tasks
            .into_iter()
            .map(|(name, def)| {
                let task = Task::validate(name, def, &file.resources).map_err(invalid)?;
                Ok((task.name.clone(), task))
            })
            .collect::<Result<_>>()?;
        Ok(RepoConfig {
            path: path.to_owned(),
            tasks,
            resources: file.resources,
        })
    }

    /// The resource declared as `name`, if any.
    pub fn resource(&self, name: &str) -> Option<&Resource> {
        self.resources.get(name)
    }

    /// The declared resources, by name.
    pub fn resources(&self) -> &BTreeMap<String, Resource> {
        &self.resources
    }

    /// The tasks, by name.
    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.values()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory containing `.loadmaster.toml`; tasks run here.
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("/"))
    }

    pub fn task(&self, name: &str) -> Result<&Task> {
        self.tasks.get(name).ok_or_else(|| Error::UnknownTask {
            name: name.to_owned(),
            path: self.path.clone(),
            available: self
                .tasks
                .values()
                .map(|t| (t.name.clone(), t.weight, t.description.clone()))
                .collect(),
        })
    }
}

enum Search {
    Found(PathBuf),
    NotFound {
        /// Where the search stopped, if `start` is in a repository.
        repo_root: Option<PathBuf>,
        /// A config further up, outside the repository, for the message.
        beyond: Option<PathBuf>,
    },
}

fn search(start: &Path) -> Search {
    let mut ancestors = start.ancestors();
    for dir in ancestors.by_ref() {
        let path = dir.join(REPO_CONFIG_FILE);
        if path.is_file() {
            return Search::Found(path);
        }
        if dir.join(".git").exists() {
            return Search::NotFound {
                repo_root: Some(dir.to_owned()),
                beyond: ancestors
                    .map(|dir| dir.join(REPO_CONFIG_FILE))
                    .find(|path| path.is_file()),
            };
        }
    }
    Search::NotFound {
        repo_root: None,
        beyond: None,
    }
}

impl Task {
    fn validate(
        name: String,
        def: TaskFile,
        resources: &BTreeMap<String, Resource>,
    ) -> std::result::Result<Task, String> {
        if def.weight == 0 {
            return Err(format!("task `{name}`: weight must be at least 1"));
        }
        let description = match def.description.as_deref().map(str::trim) {
            Some("") => {
                return Err(format!(
                    "task `{name}`: description must not be empty; leave it out instead"
                ));
            }
            Some(text) if text.contains('\n') => {
                return Err(format!("task `{name}`: description must be one line"));
            }
            text => text.map(str::to_owned),
        };
        if let Some(min) = def.min_weight
            && !(1..=def.weight).contains(&min)
        {
            return Err(format!(
                "task `{name}`: min_weight must be from 1 to its weight, {}",
                def.weight
            ));
        }
        if let Some(undeclared) = def.uses.keys().find(|r| !resources.contains_key(*r)) {
            let declared = if resources.is_empty() {
                "none".to_owned()
            } else {
                resources.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            return Err(format!(
                "task `{name}` uses `{undeclared}`, which is not declared under [resources] \
                 (declared: {declared})"
            ));
        }
        if def.uses.values().any(|&amount| amount == 0) {
            return Err(format!("task `{name}`: `uses` amounts must be at least 1"));
        }
        let ok_exit_codes = match def.ok_exit_codes {
            None => vec![0],
            Some(codes) if codes.is_empty() => {
                return Err(format!(
                    "task `{name}`: `ok_exit_codes` must list at least one exit code"
                ));
            }
            Some(codes) => codes
                .into_iter()
                .map(|code| match i32::try_from(code) {
                    Ok(code @ 0..=255) => Ok(code),
                    _ => Err(format!(
                        "task `{name}`: exit codes are 0 to 255, found {code}"
                    )),
                })
                .collect::<std::result::Result<_, _>>()?,
        };
        let stop_signal = match def.stop_signal {
            None => libc::SIGTERM,
            Some(given) => crate::runner::STOP_SIGNALS
                .iter()
                .find(|(name, _)| *name == given)
                .map(|&(_, signal)| signal)
                .ok_or_else(|| {
                    let names: Vec<&str> = crate::runner::STOP_SIGNALS
                        .iter()
                        .map(|(name, _)| *name)
                        .collect();
                    format!(
                        "task `{name}`: `stop_signal` is one of {}, not `{given}` (SIGKILL is \
                         `loadmaster cancel --kill`)",
                        names.join(", ")
                    )
                })?,
        };
        let cmd_hint = "e.g. cmd = [\"npx\", \"vitest\", \"run\"]; \
                        use [\"sh\", \"-c\", \"...\"] if you need shell syntax";
        let cmd = match def.cmd {
            toml::Value::Array(items) => items
                .into_iter()
                .map(|item| match item {
                    toml::Value::String(s) => Ok(s),
                    other => Err(format!(
                        "task `{name}`: every element of `cmd` must be a string, found {}",
                        other.type_str()
                    )),
                })
                .collect::<std::result::Result<Vec<_>, _>>()?,
            toml::Value::String(_) => {
                return Err(format!(
                    "task `{name}`: `cmd` must be an array of strings, not a single string ({cmd_hint})"
                ));
            }
            other => {
                return Err(format!(
                    "task `{name}`: `cmd` must be an array of strings, found {} ({cmd_hint})",
                    other.type_str()
                ));
            }
        };
        if cmd.first().is_none_or(|program| program.is_empty()) {
            return Err(format!(
                "task `{name}`: `cmd` must start with a program name ({cmd_hint})"
            ));
        }
        Ok(Task {
            name,
            description,
            weight: def.weight,
            min_weight: def.min_weight,
            uses: def.uses,
            ok_exit_codes,
            stop_signal,
            cmd,
        })
    }
}

/// Where the capacity came from, for messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacitySource {
    File(PathBuf),
    Default { cores: u32 },
}

impl fmt::Display for CapacitySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapacitySource::File(path) => write!(f, "set in {}", path.display()),
            CapacitySource::Default { cores } => {
                write!(f, "default: {cores} cores minus {RESERVED_CORES}")
            }
        }
    }
}

/// The machine-wide settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineConfig {
    pub capacity: u32,
    pub capacity_source: CapacitySource,
    /// Admit every job at once and only record it: a baseline to compare
    /// against (see `loadmaster report`).
    pub measure_only: bool,
    /// Start jobs with fewer cores while CPU is busy outside the queue
    /// beyond the cores the capacity leaves free.
    pub adapt_to_outside_cpu: bool,
    /// Resource capacities, overriding the repositories' defaults.
    pub resources: BTreeMap<String, u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineFile {
    /// Checked before the file is read strictly; see [`check_min_version`].
    #[serde(default, rename = "min_version")]
    _min_version: Option<String>,
    capacity: Option<u32>,
    #[serde(default)]
    measure_only: bool,
    #[serde(default = "yes")]
    adapt_to_outside_cpu: bool,
    #[serde(default)]
    resources: BTreeMap<String, u32>,
}

fn yes() -> bool {
    true
}

impl MachineConfig {
    /// Loads the machine config from its standard location. A file named by
    /// LOADMASTER_CONFIG must exist: a typo there would otherwise give this
    /// process the defaults while others use the file. Judged by reading
    /// it, so a file removed in between is not mistaken for no file.
    pub fn load() -> Result<MachineConfig> {
        let Some(path) = machine_config_override()? else {
            return Self::load_from(&machine_config_path()?);
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&path, &text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Err(Error::MissingMachineConfig { path })
            }
            Err(source) => Err(Error::ReadConfig { path, source }),
        }
    }

    /// Loads the machine config; a missing file means all defaults.
    pub fn load_from(path: &Path) -> Result<MachineConfig> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(path, &text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::parse(path, ""),
            Err(source) => Err(Error::ReadConfig {
                path: path.to_owned(),
                source,
            }),
        }
    }

    fn parse(path: &Path, text: &str) -> Result<MachineConfig> {
        let invalid = |message: String| Error::InvalidConfig {
            path: path.to_owned(),
            message,
        };
        check_min_version(path, text)?;
        let file: MachineFile = toml::from_str(text).map_err(|e| invalid(explain_toml_error(e)))?;
        if let Some((name, _)) = file.resources.iter().find(|(_, c)| **c == 0) {
            return Err(invalid(format!(
                "resource `{name}`: capacity must be at least 1"
            )));
        }
        let (capacity, capacity_source) = match file.capacity {
            Some(0) => return Err(invalid("capacity must be at least 1".to_owned())),
            Some(capacity) => (capacity, CapacitySource::File(path.to_owned())),
            None => {
                let cores = std::thread::available_parallelism()
                    .map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX));
                (
                    cores.saturating_sub(RESERVED_CORES).max(1),
                    CapacitySource::Default { cores },
                )
            }
        };
        Ok(MachineConfig {
            capacity,
            capacity_source,
            measure_only: file.measure_only,
            adapt_to_outside_cpu: file.adapt_to_outside_cpu,
            resources: file.resources,
        })
    }
}

/// This loadmaster's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A config file may name the oldest loadmaster that can read it, as
/// `min_version = "0.5.0"`. It is checked before the file is read strictly,
/// so an older version says what it needs instead of rejecting the first
/// key it does not know.
fn check_min_version(path: &Path, text: &str) -> Result<()> {
    let invalid = |message: String| Error::InvalidConfig {
        path: path.to_owned(),
        message,
    };
    let table: toml::Table = toml::from_str(text).map_err(|e| invalid(e.to_string()))?;
    let Some(value) = table.get("min_version") else {
        return Ok(());
    };
    let expected = "a version such as \"0.5.0\"";
    let required = value
        .as_str()
        .ok_or_else(|| invalid(format!("`min_version` must be {expected}")))?;
    let needed = version_parts(required)
        .ok_or_else(|| invalid(format!("`min_version` is `{required}`, not {expected}")))?;
    // This build's own version may carry a pre-release or build suffix,
    // which does not count here.
    let release = VERSION.split(['-', '+']).next().unwrap_or(VERSION);
    let Some(current) = version_parts(release) else {
        return Ok(());
    };
    if needed > current {
        return Err(Error::VersionTooOld {
            path: path.to_owned(),
            required: required.to_owned(),
        });
    }
    Ok(())
}

/// "0.5.0", "0.5" or "1" as (major, minor, patch). Pre-release and build
/// suffixes are not accepted: a minimum is a release.
fn version_parts(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.').map(|part| {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse::<u64>().ok())
            .flatten()
    });
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// A TOML error, with a note on unknown keys: they may be typos, or keys of
/// a newer version.
fn explain_toml_error(e: toml::de::Error) -> String {
    let message = e.to_string();
    if !message.contains("unknown field") {
        return message;
    }
    format!(
        "{}\nthis is loadmaster {VERSION}: an unknown key is a typo, or one that a newer version \
         added; a file that needs a newer version can say so with `min_version = \"X.Y.Z\"`",
        message.trim_end()
    )
}

/// `$LOADMASTER_CONFIG`, else `$XDG_CONFIG_HOME/loadmaster/config.toml`,
/// else `~/.config/loadmaster/config.toml`. Always absolute.
pub fn machine_config_path() -> Result<PathBuf> {
    match machine_config_override()? {
        Some(path) => Ok(path),
        None => Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("loadmaster/config.toml")),
    }
}

/// `$LOADMASTER_CONFIG`, resolved, if set.
pub fn machine_config_override() -> Result<Option<PathBuf>> {
    env_path(CONFIG_ENV).map(absolute).transpose()
}

/// `$LOADMASTER_STATE_DIR`, else `$XDG_STATE_HOME/loadmaster`,
/// else `~/.local/state/loadmaster`. Always absolute, so it names the same
/// queue no matter which directory a (nested) run starts in.
pub fn state_dir() -> Result<PathBuf> {
    if let Some(path) = env_path(STATE_DIR_ENV) {
        return absolute(path);
    }
    Ok(xdg_dir("XDG_STATE_HOME", ".local/state")?.join("loadmaster"))
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    std::path::absolute(&path)
        .map_err(|e| Error::io(format!("cannot resolve {}", path.display()), e))
}

fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    // The XDG spec says relative paths are invalid and must be ignored.
    if let Some(dir) = env_path(var).filter(|p| p.is_absolute()) {
        return Ok(dir);
    }
    let home = env_path("HOME").ok_or(Error::NoHome)?;
    Ok(home.join(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<RepoConfig> {
        RepoConfig::parse(Path::new("/repo/.loadmaster.toml"), text)
    }

    fn message(err: Error) -> String {
        err.to_string()
    }

    #[test]
    fn parses_tasks() {
        let config = parse(
            r#"
            [tasks.lint]
            weight = 1
            cmd = ["npx", "biome", "check", "."]

            [tasks.test]
            weight = 4
            cmd = ["npx", "vitest", "run"]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.task("test").unwrap(),
            &Task {
                description: None,
                name: "test".into(),
                weight: 4,
                min_weight: None,
                uses: BTreeMap::new(),
                ok_exit_codes: vec![0],
                stop_signal: libc::SIGTERM,
                cmd: vec!["npx".into(), "vitest".into(), "run".into()],
            }
        );
        assert_eq!(config.dir(), Path::new("/repo"));
    }

    #[test]
    fn unknown_task_lists_available_tasks() {
        let config = parse(
            "[tasks.lint]\nweight = 1\ncmd = [\"x\"]\n[tasks.e2e]\nweight = 8\ncmd = [\"y\"]\n",
        )
        .unwrap();
        let msg = message(config.task("tset").unwrap_err());
        assert!(msg.contains("unknown task `tset`"), "{msg}");
        assert!(msg.contains("e2e (weight 8), lint (weight 1)"), "{msg}");
    }

    #[test]
    fn rejects_string_cmd_with_hint() {
        let msg = message(parse("[tasks.t]\nweight = 1\ncmd = \"npm test\"\n").unwrap_err());
        assert!(msg.contains("must be an array of strings"), "{msg}");
        assert!(msg.contains("[\"sh\", \"-c\""), "{msg}");
    }

    #[test]
    fn rejects_bad_tasks() {
        for (text, expected) in [
            (
                "[tasks.t]\nweight = 0\ncmd = [\"x\"]\n",
                "weight must be at least 1",
            ),
            ("[tasks.t]\nweight = -1\ncmd = [\"x\"]\n", "weight"),
            (
                "[tasks.t]\nweight = 1\ncmd = []\n",
                "must start with a program",
            ),
            (
                "[tasks.t]\nweight = 1\ncmd = [\"\"]\n",
                "must start with a program",
            ),
            (
                "[tasks.t]\nweight = 1\ncmd = [\"x\", 3]\n",
                "must be a string",
            ),
            ("[tasks.t]\ncmd = [\"x\"]\n", "missing field `weight`"),
            ("[tasks.t]\nweight = 1\n", "missing field `cmd`"),
            (
                "[tasks.t]\nwieght = 1\ncmd = [\"x\"]\n",
                "unknown field `wieght`",
            ),
            ("[task.t]\nweight = 1\n", "unknown field `task`"),
            ("[tasks.t\n", "TOML parse error"),
        ] {
            let msg = message(parse(text).unwrap_err());
            assert!(msg.contains(expected), "{text:?}: {msg}");
            assert!(msg.contains("/repo/.loadmaster.toml"), "{text:?}: {msg}");
        }
    }

    #[test]
    fn min_version_names_the_oldest_loadmaster_that_reads_the_file() {
        let task = "[tasks.t]\nweight = 1\ncmd = [\"x\"]\n";
        for ok in ["0.1.0", "0.1", "0", VERSION] {
            let text = format!("min_version = \"{ok}\"\n{task}");
            assert!(parse(&text).is_ok(), "{ok}");
        }
        // Checked first: an older version must not stop at a key it does
        // not know.
        let msg = message(
            parse("min_version = \"99.0.0\"\n[tasks.t]\nweight = 1\nfuture = 1\ncmd = [\"x\"]\n")
                .unwrap_err(),
        );
        assert_eq!(
            msg,
            format!(
                "/repo/.loadmaster.toml needs loadmaster 99.0.0 or newer (its `min_version`), \
                 but this is {VERSION}: upgrade loadmaster"
            )
        );
        for (text, expected) in [
            (
                "min_version = 5\n",
                "`min_version` must be a version such as",
            ),
            (
                "min_version = \"latest\"\n",
                "`min_version` is `latest`, not a version",
            ),
            ("min_version = \"0.5.0.1\"\n", "`min_version` is `0.5.0.1`"),
            ("min_version = \"0..1\"\n", "`min_version` is `0..1`"),
            (
                "min_version = \"0.4.0-???\"\n",
                "`min_version` is `0.4.0-???`",
            ),
            (
                "min_version = \"0.4.0+local\"\n",
                "`min_version` is `0.4.0+local`",
            ),
        ] {
            let msg = message(parse(text).unwrap_err());
            assert!(msg.contains(expected), "{text:?}: {msg}");
        }
        let machine = MachineConfig::parse(Path::new("/m.toml"), "min_version = \"99\"\n");
        assert!(matches!(machine, Err(Error::VersionTooOld { .. })));
    }

    #[test]
    fn an_unknown_key_says_which_version_does_not_know_it() {
        let msg =
            message(parse("[tasks.t]\nweight = 1\nretries = 2\ncmd = [\"x\"]\n").unwrap_err());
        assert!(msg.contains("unknown field `retries`"), "{msg}");
        assert!(
            msg.contains(&format!(
                "this is loadmaster {VERSION}: an unknown key is a typo"
            )),
            "{msg}"
        );
        let msg = message(parse("[tasks.t]\nweight = 1\ncmd = 3\n").unwrap_err());
        assert!(!msg.contains("this is loadmaster"), "{msg}");
    }

    #[test]
    fn discovers_config_in_parent_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(REPO_CONFIG_FILE),
            "[tasks.t]\nweight = 1\ncmd = [\"true\"]\n",
        )
        .unwrap();
        let nested = root.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let config = RepoConfig::discover(&nested).unwrap();
        assert_eq!(config.dir(), root.path());
    }

    #[test]
    fn missing_repo_config_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let msg = message(RepoConfig::discover(root.path()).unwrap_err());
        assert!(msg.contains("no .loadmaster.toml found"), "{msg}");
    }

    #[test]
    fn the_search_stops_at_the_root_of_the_repository() {
        // The main checkout has a config; a worktree inside it, on a branch
        // without one, must not run the main checkout's tasks.
        let main = tempfile::tempdir().unwrap();
        std::fs::write(
            main.path().join(REPO_CONFIG_FILE),
            "[tasks.t]\nweight = 1\ncmd = [\"true\"]\n",
        )
        .unwrap();
        std::fs::create_dir(main.path().join(".git")).unwrap();
        let worktree = main.path().join(".claude/worktrees/feature");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}/.git/worktrees/feature\n", main.path().display()),
        )
        .unwrap();

        assert_eq!(RepoConfig::find(&worktree.join("src")), None);
        let msg = message(RepoConfig::discover(&worktree.join("src")).unwrap_err());
        assert!(
            msg.contains(&format!(
                "up to the root of its repository, {}",
                worktree.display()
            )),
            "{msg}"
        );
        assert!(
            msg.contains(&format!(
                "{} is outside this repository",
                main.path().join(REPO_CONFIG_FILE).display()
            )),
            "{msg}"
        );
        // Within the main checkout, its own config is found.
        let sub = main.path().join("src");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(
            RepoConfig::find(&sub),
            Some(main.path().join(REPO_CONFIG_FILE))
        );
    }

    #[test]
    fn machine_capacity_from_file() {
        let config = MachineConfig::parse(Path::new("/m.toml"), "capacity = 8\n").unwrap();
        assert_eq!(config.capacity, 8);
        assert_eq!(
            config.capacity_source,
            CapacitySource::File("/m.toml".into())
        );
    }

    #[test]
    fn measure_only_is_off_unless_set() {
        let path = Path::new("/m.toml");
        assert!(
            !MachineConfig::parse(path, "capacity = 8\n")
                .unwrap()
                .measure_only
        );
        let config = MachineConfig::parse(path, "measure_only = true\n").unwrap();
        assert!(config.measure_only);
    }

    #[test]
    fn machine_capacity_defaults_below_core_count() {
        let config = MachineConfig::parse(Path::new("/m.toml"), "").unwrap();
        let CapacitySource::Default { cores } = config.capacity_source else {
            panic!("expected default capacity");
        };
        assert_eq!(config.capacity, cores.saturating_sub(2).max(1));
    }

    #[test]
    fn missing_machine_config_means_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = MachineConfig::load_from(&dir.path().join("none.toml")).unwrap();
        assert!(matches!(
            config.capacity_source,
            CapacitySource::Default { .. }
        ));
    }

    #[test]
    fn rejects_bad_machine_config() {
        for (text, expected) in [
            ("capacity = 0\n", "capacity must be at least 1"),
            ("capacity = \"8\"\n", "invalid type"),
            ("capasity = 8\n", "unknown field `capasity`"),
            ("measure_only = \"yes\"\n", "invalid type"),
            (
                "[resources]\nmemory = 0\n",
                "resource `memory`: capacity must be at least 1",
            ),
            ("[resources]\nmemory = \"40\"\n", "invalid type"),
        ] {
            let msg = message(MachineConfig::parse(Path::new("/m.toml"), text).unwrap_err());
            assert!(msg.contains(expected), "{text:?}: {msg}");
        }
    }

    #[test]
    fn parses_resources_and_what_tasks_use() {
        let config = parse(
            r#"
            [resources.db]
            capacity = 2
            per_checkout = 1

            [resources.memory]

            [tasks.e2e]
            weight = 2
            uses = { db = 1, memory = 4 }
            cmd = ["npx", "playwright", "test"]

            [tasks.lint]
            weight = 1
            cmd = ["npx", "biome", "check"]
            "#,
        )
        .unwrap();
        let e2e = config.task("e2e").unwrap();
        assert_eq!(
            e2e.uses,
            BTreeMap::from([("db".to_owned(), 1), ("memory".to_owned(), 4)])
        );
        assert!(config.task("lint").unwrap().uses.is_empty());
        assert_eq!(
            config.resource("db"),
            Some(&Resource {
                capacity: Some(2),
                per_checkout: Some(1),
            })
        );
        assert_eq!(config.resource("memory"), Some(&Resource::default()));
        assert_eq!(config.resource("gpu"), None);
    }

    #[test]
    fn rejects_bad_resources() {
        let task = |uses: &str| format!("[tasks.t]\nweight = 1\nuses = {uses}\ncmd = [\"x\"]\n");
        for (text, expected) in [
            (
                "[resources.db]\n".to_owned() + &task("{ dbb = 1 }"),
                "task `t` uses `dbb`, which is not declared under [resources] (declared: db)",
            ),
            (
                task("{ db = 1 }"),
                "task `t` uses `db`, which is not declared under [resources] (declared: none)",
            ),
            (
                "[resources.db]\n".to_owned() + &task("{ db = 0 }"),
                "task `t`: `uses` amounts must be at least 1",
            ),
            (
                "[resources.db]\ncapacity = 0\n".to_owned(),
                "resource `db`: capacity must be at least 1",
            ),
            (
                "[resources.db]\nper_checkout = 0\n".to_owned(),
                "resource `db`: per_checkout must be at least 1",
            ),
            (
                "[resources.db]\ncapacty = 2\n".to_owned(),
                "unknown field `capacty`",
            ),
        ] {
            let msg = message(parse(&text).unwrap_err());
            assert!(msg.contains(expected), "{text:?}: {msg}");
        }
    }

    #[test]
    fn ok_exit_codes_default_to_zero() {
        let config = parse(
            "[tasks.lint]\nweight = 1\ncmd = [\"x\"]\n\
             [tasks.ui]\nweight = 1\nok_exit_codes = [0, 2]\ncmd = [\"x\"]\n",
        )
        .unwrap();
        assert_eq!(config.task("lint").unwrap().ok_exit_codes, [0]);
        assert_eq!(config.task("ui").unwrap().ok_exit_codes, [0, 2]);
        for (codes, expected) in [
            (
                "[]",
                "task `t`: `ok_exit_codes` must list at least one exit code",
            ),
            ("[256]", "task `t`: exit codes are 0 to 255, found 256"),
            ("[-1]", "task `t`: exit codes are 0 to 255, found -1"),
        ] {
            let text = format!("[tasks.t]\nweight = 1\nok_exit_codes = {codes}\ncmd = [\"x\"]\n");
            let msg = message(parse(&text).unwrap_err());
            assert!(msg.contains(expected), "{codes}: {msg}");
        }
    }

    #[test]
    fn a_task_names_the_signal_that_stops_it() {
        let config = parse(
            "[tasks.lint]\nweight = 1\ncmd = [\"x\"]\n\
             [tasks.e2e]\nweight = 1\nstop_signal = \"SIGINT\"\ncmd = [\"x\"]\n",
        )
        .unwrap();
        assert_eq!(config.task("lint").unwrap().stop_signal, libc::SIGTERM);
        assert_eq!(config.task("e2e").unwrap().stop_signal, libc::SIGINT);
        for given in ["SIGKILL", "INT", "sigint"] {
            let text = format!("[tasks.t]\nweight = 1\nstop_signal = \"{given}\"\ncmd = [\"x\"]\n");
            let msg = message(parse(&text).unwrap_err());
            assert!(
                msg.contains(&format!(
                    "task `t`: `stop_signal` is one of SIGTERM, SIGINT, SIGHUP, SIGQUIT, SIGUSR1, \
                     SIGUSR2, not `{given}`"
                )),
                "{msg}"
            );
        }
    }

    #[test]
    fn machine_resources() {
        let path = Path::new("/m.toml");
        let config = MachineConfig::parse(path, "[resources]\nmemory = 40\ndb = 3\n").unwrap();
        assert_eq!(
            config.resources,
            BTreeMap::from([("db".to_owned(), 3), ("memory".to_owned(), 40)])
        );
        assert!(MachineConfig::parse(path, "").unwrap().resources.is_empty());
    }
}
