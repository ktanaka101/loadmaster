//! Configuration, split in two layers:
//!
//! - **Repository** (`.loadmaster.toml`, committed): tasks and their weights.
//! - **Machine** (`~/.config/loadmaster/config.toml`): the capacity shared by
//!   every repository and worktree on this machine.
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
    pub weight: u32,
    /// Program and arguments. Never empty.
    pub cmd: Vec<String>,
}

/// A parsed `.loadmaster.toml`.
#[derive(Debug)]
pub struct RepoConfig {
    path: PathBuf,
    tasks: BTreeMap<String, Task>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoFile {
    #[serde(default)]
    tasks: BTreeMap<String, TaskFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    weight: u32,
    cmd: toml::Value,
}

impl RepoConfig {
    /// Finds and loads `.loadmaster.toml` in `start` or the nearest parent
    /// directory.
    pub fn discover(start: &Path) -> Result<RepoConfig> {
        Self::find(start)
            .ok_or_else(|| Error::RepoConfigNotFound {
                start: start.to_owned(),
            })
            .and_then(|path| Self::load(&path))
    }

    /// The path of `.loadmaster.toml` in `start` or the nearest parent
    /// directory, without reading it.
    pub fn find(start: &Path) -> Option<PathBuf> {
        start
            .ancestors()
            .map(|dir| dir.join(REPO_CONFIG_FILE))
            .find(|path| path.is_file())
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
        let file: RepoFile = toml::from_str(text).map_err(|e| invalid(e.to_string()))?;
        let tasks = file
            .tasks
            .into_iter()
            .map(|(name, def)| {
                let task = Task::validate(name, def).map_err(invalid)?;
                Ok((task.name.clone(), task))
            })
            .collect::<Result<_>>()?;
        Ok(RepoConfig {
            path: path.to_owned(),
            tasks,
        })
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
                .map(|t| (t.name.clone(), t.weight))
                .collect(),
        })
    }
}

impl Task {
    fn validate(name: String, def: TaskFile) -> std::result::Result<Task, String> {
        if def.weight == 0 {
            return Err(format!("task `{name}`: weight must be at least 1"));
        }
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
            weight: def.weight,
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineFile {
    capacity: Option<u32>,
    #[serde(default)]
    measure_only: bool,
}

impl MachineConfig {
    /// Loads the machine config from its standard location.
    pub fn load() -> Result<MachineConfig> {
        Self::load_from(&machine_config_path()?)
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
        let file: MachineFile = toml::from_str(text).map_err(|e| invalid(e.to_string()))?;
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
        })
    }
}

/// `$LOADMASTER_CONFIG`, else `$XDG_CONFIG_HOME/loadmaster/config.toml`,
/// else `~/.config/loadmaster/config.toml`. Always absolute.
pub fn machine_config_path() -> Result<PathBuf> {
    if let Some(path) = env_path(CONFIG_ENV) {
        return absolute(path);
    }
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("loadmaster/config.toml"))
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
                name: "test".into(),
                weight: 4,
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
        ] {
            let msg = message(MachineConfig::parse(Path::new("/m.toml"), text).unwrap_err());
            assert!(msg.contains(expected), "{text:?}: {msg}");
        }
    }
}
