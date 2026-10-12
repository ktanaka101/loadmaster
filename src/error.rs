use std::io;
use std::path::{Path, PathBuf};

use crate::config::REPO_CONFIG_FILE;
use crate::queue::JobId;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}", not_found(start, repo_root.as_deref(), beyond.as_deref()))]
    RepoConfigNotFound {
        start: PathBuf,
        repo_root: Option<PathBuf>,
        beyond: Option<PathBuf>,
    },

    #[error("cannot read {}: {source}", path.display())]
    ReadConfig { path: PathBuf, source: io::Error },

    #[error("LOADMASTER_CONFIG names {}, which does not exist", path.display())]
    MissingMachineConfig { path: PathBuf },

    #[error("invalid config {}: {message}", path.display())]
    InvalidConfig { path: PathBuf, message: String },

    #[error(
        "{} needs loadmaster {required} or newer (its `min_version`), but this is {}: \
         upgrade loadmaster",
        path.display(),
        crate::config::VERSION
    )]
    VersionTooOld { path: PathBuf, required: String },

    #[error(
        "unknown task `{name}` in {}\navailable tasks: {}",
        path.display(),
        format_tasks(available)
    )]
    UnknownTask {
        name: String,
        path: PathBuf,
        available: Vec<(String, u32)>,
    },

    #[error(
        "{reason}\n\
         `loadmaster run` was started with LOADMASTER_HELD set, so it would run in that job's slot; \
         unset LOADMASTER_HELD if it leaked into this environment"
    )]
    InvalidHeld { reason: String },

    #[error(
        "{reason}\n\
         a nested `loadmaster run` uses the slot of the job that holds it, and waiting for more \
         from inside that slot could deadlock"
    )]
    NestedDoesNotFit { reason: String },

    #[error("{var}={value} is invalid: expected {expected}")]
    InvalidEnv {
        var: &'static str,
        value: String,
        expected: &'static str,
    },

    #[error("{0}")]
    Usage(String),

    #[error("cannot determine the home directory: HOME is not set")]
    NoHome,

    #[error(
        "state database {} has schema version {found}, but this loadmaster expects {expected}\n\
         delete it once no loadmaster jobs are running",
        path.display()
    )]
    SchemaVersion {
        path: PathBuf,
        found: i64,
        expected: i64,
    },

    #[error(
        "cannot write to the state directory {} ({detail})\n\
         loadmaster keeps the queue that every agent on this machine shares there; in an \
         agent's sandbox, allow writing to it (see \"Let it write the state directory\" in \
         loadmaster's README). LOADMASTER_STATE_DIR moves it, but every agent must then use \
         the same one",
        dir.display()
    )]
    StateDirNotWritable { dir: PathBuf, detail: String },

    #[error("job #{0} disappeared from the queue")]
    JobVanished(JobId),

    #[error("cannot stop job #{id}: {why}: {source}{}", sandbox_hint(source))]
    CannotSignal {
        id: JobId,
        why: String,
        source: io::Error,
    },

    #[error("state database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
}

impl Error {
    pub fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

fn not_found(start: &Path, repo_root: Option<&Path>, beyond: Option<&Path>) -> String {
    let mut message = match repo_root {
        Some(root) if root == start => format!(
            "no {REPO_CONFIG_FILE} found in {}, the root of its repository",
            start.display()
        ),
        Some(root) => format!(
            "no {REPO_CONFIG_FILE} found in {} or its parents up to the root of its repository, {}",
            start.display(),
            root.display()
        ),
        None => format!(
            "no {REPO_CONFIG_FILE} found in {} or any parent directory",
            start.display()
        ),
    };
    if let Some(beyond) = beyond {
        message += &format!(
            "\n{} is outside this repository (in the main checkout a worktree lives in, \
             for example) and is not used: commit {REPO_CONFIG_FILE} on this branch",
            beyond.display()
        );
    }
    message
}

fn sandbox_hint(source: &io::Error) -> &'static str {
    if source.kind() == io::ErrorKind::PermissionDenied {
        "\nin an agent's sandbox, a command may not signal processes that another command \
         started: run `loadmaster cancel` outside the sandbox, or from where the job was started"
    } else {
        ""
    }
}

fn format_tasks(tasks: &[(String, u32)]) -> String {
    if tasks.is_empty() {
        return "(none defined)".to_owned();
    }
    tasks
        .iter()
        .map(|(name, weight)| format!("{name} (weight {weight})"))
        .collect::<Vec<_>>()
        .join(", ")
}
