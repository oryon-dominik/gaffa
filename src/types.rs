use std::time::{Duration, Instant};

use colored::Colorize;

pub type Result<T> = std::result::Result<T, ProcessError>;

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("Failed to read Procfile '{path}': {source}")]
    ProcfileRead {
        path: String,
        source: std::io::Error,
    },

    #[error("Invalid Procfile line format: '{line}'")]
    InvalidFormat { line: String },

    #[error("No valid processes found in Procfile")]
    NoProcesses,

    #[error("Process '{name}' not found")]
    ProcessNotFound { name: String },

    #[error("Process '{name}' is already running")]
    ProcessAlreadyRunning { name: String },

    #[error("Process '{name}' is not running")]
    ProcessNotRunning { name: String },

    #[error("Failed to parse command '{command}': {source}")]
    CommandParse {
        command: String,
        source: shell_words::ParseError,
    },

    #[error("Empty command for process '{name}'")]
    EmptyCommand { name: String },

    #[error("Failed to spawn process '{name}': {source}")]
    ProcessSpawn {
        name: String,
        source: std::io::Error,
    },

    #[error("Error reading input: {0}")]
    InputRead(std::io::Error),
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub command: String,
    pub status: ProcessStatus,
    pub restart_count: u32,
    pub last_restart: Option<Instant>,
    pub stopped_at: Option<Instant>,
    pub cumulative_runtime: Duration,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessStatus {
    Running,
    Stopped,
    Restarting,
}

impl std::fmt::Display for ProcessStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status_str = match self {
            Self::Running => "RUNNING".green(),
            Self::Stopped => "STOPPED".yellow(),
            Self::Restarting => "RESTARTING".blue(),
        };
        write!(f, "{status_str}")
    }
}
