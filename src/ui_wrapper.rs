use std::sync::Arc;
use crate::process_manager::{ProcessError, ProcessManager};
use crate::ui::{AppState, run_terminal_ui};

/// Simple wrapper to run the terminal UI with minimal configuration
pub async fn run_interactive_ui(manager: ProcessManager) -> Result<(), ProcessError> {
    let manager = Arc::new(manager);
    let state = Arc::new(AppState::new());
    
    // Get all process names to start
    let processes_to_start = manager.process_names().await;
    
    // We don't have the procfile path and log file path in this context,
    // so we'll use defaults
    let procfile_path = "Procfile";
    let log_file_path = None;
    
    run_terminal_ui(
        manager,
        state,
        processes_to_start,
        procfile_path,
        log_file_path,
    ).await
}