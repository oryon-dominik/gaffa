pub mod constants;
pub mod platform;
mod process_manager;
pub mod ui;
mod ui_wrapper;

#[cfg(test)]
mod tests;

// Re-export main types and functions
pub use process_manager::{
    parse_env_file, ProcessError, ProcessInfo, ProcessManager, ProcessStatus, Result,
};