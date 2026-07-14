use std::path::PathBuf;

/// Errors returned by dvup operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot determine the dvup state directory")]
    StateDirectoryUnavailable,

    #[error("configuration file not found at {0}; run `dvup init` first")]
    ConfigNotFound(PathBuf),

    #[error("tool `{0}` is not defined in the configuration")]
    ToolNotFound(String),

    #[error("configuration is invalid: {0}")]
    InvalidConfig(String),

    #[error("job `{0}` was not found")]
    JobNotFound(String),

    #[error("job is invalid: {0}")]
    InvalidJob(String),

    #[error("refusing to overwrite existing file {0}; pass --force to replace it")]
    FileExists(PathBuf),

    #[error("settings file {path} is locked or not writable: {source}")]
    SettingsWrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("configuration file {path} is locked or not writable: {source}")]
    ConfigWrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("command is empty")]
    EmptyCommand,

    #[error("failed to start `{program}`: {source}")]
    CommandStart {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{0}")]
    Message(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    TomlDeserialize(#[from] toml::de::Error),

    #[error(transparent)]
    TomlSerialize(#[from] toml::ser::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
