pub mod constants;
pub mod output;
pub mod platform;
mod process_manager;
pub mod procfile;
pub mod shell;
pub mod types;
pub mod ui;
mod ui_wrapper;

#[cfg(test)]
mod tests;

// Re-export main types and functions
pub use process_manager::{LifecycleOptions, ProcessManager};
pub use procfile::parse_env_file;
pub use shell::Shell;
pub use types::{ProcessError, ProcessInfo, ProcessStatus, Result};
