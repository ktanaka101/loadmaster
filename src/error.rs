use std::io;
use std::path::PathBuf;

use crate::config::{CapacitySource, REPO_CONFIG_FILE};
use crate::queue::JobId;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no {REPO_CONFIG_FILE} found in {} or any parent directory", start.display())]
    RepoConfigNotFound { start: PathBuf },

    #[error("cannot read {}: {source}", path.display())]
    ReadConfig { path: PathBuf, source: io::Error },

    #[error("invalid config {}: {message}", path.display())]
    InvalidConfig { path: PathBuf, message: String },

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
        "task `{task}` has weight {weight}, but this machine's capacity is {capacity} ({capacity_source}), so it could never start\n\
         lower the weight in {}, or raise `capacity` in the machine config",
        path.display()
    )]
    WeightExceedsCapacity {
        task: String,
        weight: u32,
        capacity: u32,
        capacity_source: CapacitySource,
        path: PathBuf,
    },

    #[error(
        "LOADMASTER_HELD={value} does not refer to a running loadmaster job\n\
         `loadmaster run` was started with LOADMASTER_HELD set, but that job is gone or never existed; \
         unset LOADMASTER_HELD if it leaked into this environment"
    )]
    InvalidHeld { value: String },

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

    #[error("job #{0} disappeared from the queue")]
    JobVanished(JobId),

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
