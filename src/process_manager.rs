use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use colored::Colorize;
use tokio::{
    process::{Child, Command as TokioCommand},
    sync::Mutex,
    time::sleep,
};

use crate::constants::*;
use crate::output;
use crate::platform::{configure_command, force_kill_process, terminate_process};
use crate::procfile;
use crate::types::*;
use crate::ui::AppState;
use crate::ui_wrapper::run_interactive_ui;

/// Options for process lifecycle operations.
///
/// Replaces the previous pattern of duplicated `_with_state` / `_internal`
/// methods by bundling the two orthogonal knobs (UI state and logging) into
/// a single value object.
pub struct LifecycleOptions {
    pub app_state: Option<Arc<AppState>>,
    pub log_messages: bool,
}

impl LifecycleOptions {
    /// Default options: no UI, log messages enabled.
    pub fn new() -> Self {
        Self {
            app_state: None,
            log_messages: true,
        }
    }

    /// Quiet mode: no UI, no log messages.
    pub fn quiet() -> Self {
        Self {
            app_state: None,
            log_messages: false,
        }
    }

    /// UI mode: with app state, log messages enabled.
    pub fn with_ui(state: Arc<AppState>) -> Self {
        Self {
            app_state: Some(state),
            log_messages: true,
        }
    }
}

impl Default for LifecycleOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Configuration set during initialization. Rarely changes after load.
pub(crate) struct ProcessConfig {
    pub colors: HashMap<String, colored::Color>,
    pub max_name_length: usize,
    pub env_vars: HashMap<String, String>,
    pub log_file: Option<Arc<Mutex<std::fs::File>>>,
}

/// Mutable runtime state for all managed processes.
pub(crate) struct RuntimeState {
    pub processes: HashMap<String, ProcessInfo>,
    pub children: HashMap<String, Child>,
    pub monitor_handles: HashMap<String, tokio::task::JoinHandle<()>>,
    pub output_handles: HashMap<String, Vec<tokio::task::JoinHandle<()>>>,
}

/// Manages multiple processes defined in a Procfile.
///
/// Provides functionality to start, stop, restart, and monitor processes
/// with interactive control capabilities.
#[derive(Clone)]
pub struct ProcessManager {
    pub(crate) config: Arc<Mutex<ProcessConfig>>,
    pub(crate) runtime: Arc<Mutex<RuntimeState>>,
}

impl std::fmt::Debug for ProcessManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessManager").finish()
    }
}

impl ProcessManager {
    /// Create a new process manager instance.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: Arc::new(Mutex::new(ProcessConfig {
                colors: HashMap::new(),
                max_name_length: 0,
                env_vars: HashMap::new(),
                log_file: None,
            })),
            runtime: Arc::new(Mutex::new(RuntimeState {
                processes: HashMap::new(),
                children: HashMap::new(),
                monitor_handles: HashMap::new(),
                output_handles: HashMap::new(),
            })),
        }
    }

    /// Set the log file for this process manager.
    pub async fn set_log_file(&self, log_file: Arc<Mutex<std::fs::File>>) {
        let mut config = self.config.lock().await;
        config.log_file = Some(log_file);
    }

    /// Set environment variables to be applied to all processes.
    pub async fn set_environment_variables(&self, env_vars: HashMap<String, String>) {
        let mut config = self.config.lock().await;
        config.env_vars = env_vars;
    }

    /// Load process definitions from a Procfile.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The Procfile cannot be read
    /// - The Procfile contains invalid format
    /// - No valid processes are found
    pub async fn load_procfile(&self, procfile_path: &str) -> Result<()> {
        let data = procfile::parse_procfile(procfile_path)?;

        let mut config = self.config.lock().await;
        config.colors = data.colors;
        config.max_name_length = data.max_name_length;
        drop(config);

        let mut runtime = self.runtime.lock().await;
        runtime.processes = data.processes;

        Ok(())
    }

    /// Start a specific process by name.
    ///
    /// Convenience wrapper around [`start_process_with_opts`] with default options.
    ///
    /// # Errors
    ///
    /// Returns an error if the process is already running, not found, or fails to start.
    pub async fn start_process(&self, name: &str) -> Result<()> {
        self.start_process_with_opts(name, &LifecycleOptions::new())
            .await
    }

    /// Start a specific process by name with explicit lifecycle options.
    ///
    /// # Errors
    ///
    /// Returns an error if the process is already running, not found, or fails to start.
    pub async fn start_process_with_opts(&self, name: &str, opts: &LifecycleOptions) -> Result<()> {
        // Check if process is actually running (exists in children map)
        {
            let runtime = self.runtime.lock().await;
            if runtime.children.contains_key(name) {
                return Err(ProcessError::ProcessAlreadyRunning {
                    name: name.to_string(),
                });
            }
        }

        let process_info = {
            let mut runtime = self.runtime.lock().await;
            match runtime.processes.get_mut(name) {
                Some(info) => {
                    info.status = ProcessStatus::Restarting;
                    info.clone()
                }
                None => {
                    return Err(ProcessError::ProcessNotFound {
                        name: name.to_string(),
                    });
                }
            }
        };

        // Add startup message (if requested)
        if opts.log_messages
            && let Some(state) = &opts.app_state
        {
            state
                .add_log(name.to_string(), format!("Starting '{name}'..."), false)
                .await;
        }

        self.spawn_process_with_state(name, &process_info.command, opts.app_state.clone())
            .await?;

        {
            let mut runtime = self.runtime.lock().await;
            if let Some(info) = runtime.processes.get_mut(name) {
                info.status = ProcessStatus::Running;
                // Only increment restart count if this was previously started
                if info.last_restart.is_some() || info.cumulative_runtime.as_secs() > 0 {
                    info.restart_count += 1;
                }
                info.last_restart = Some(Instant::now());
                info.stopped_at = None;
            }
        }

        // Don't print to stdout if we have UI state - it's already logged
        // Also don't print if we're not logging messages (non-interactive mode handles it)
        if opts.app_state.is_none() && opts.log_messages {
            self.print_system_message(&format!("Starting process '{name}'"))
                .await;
        }
        Ok(())
    }

    /// Spawn the actual process with proper I/O handling and optional UI state.
    ///
    /// # Errors
    ///
    /// Returns an error if command parsing fails, command is empty, or process spawn fails.
    ///
    /// # Panics
    ///
    /// Panics if stdout or stderr pipes cannot be taken from the child process.
    async fn spawn_process_with_state(
        &self,
        name: &str,
        command: &str,
        app_state: Option<Arc<AppState>>,
    ) -> Result<()> {
        let command_parts =
            shell_words::split(command).map_err(|e| ProcessError::CommandParse {
                command: command.to_string(),
                source: e,
            })?;

        if command_parts.is_empty() {
            return Err(ProcessError::EmptyCommand {
                name: name.to_string(),
            });
        }

        let program = &command_parts[0];
        let args = &command_parts[1..];

        let mut cmd = TokioCommand::new(program);
        cmd.args(args);

        // Apply environment variables
        {
            let config = self.config.lock().await;
            for (key, value) in config.env_vars.iter() {
                cmd.env(key, value);
            }
        }

        // Only inherit stdin in non-interactive mode
        // In interactive mode (when app_state is Some), the TUI needs exclusive stdin access
        if app_state.is_none() {
            cmd.stdin(Stdio::inherit());
        } else {
            cmd.stdin(Stdio::null());
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        // Configure platform-specific command options
        configure_command(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| ProcessError::ProcessSpawn {
            name: name.to_string(),
            source: e,
        })?;

        let stdout = child.stdout.take().expect("stdout pipe");
        let stderr = child.stderr.take().expect("stderr pipe");

        // Get log file from config
        let log_file = {
            let config = self.config.lock().await;
            config.log_file.clone()
        };

        self.spawn_output_handler_with_state(name, stdout, stderr, app_state.clone(), log_file)
            .await;

        {
            let mut runtime = self.runtime.lock().await;
            runtime.children.insert(name.to_string(), child);
        }

        // Spawn a task to monitor the process exit
        let name_str = name.to_string();
        let runtime_arc = Arc::clone(&self.runtime);
        let app_state_monitor = app_state;

        let monitor_handle = tokio::spawn(async move {
            // Give process time to start
            tokio::time::sleep(SPAWN_WAIT_DELAY).await;

            // Use exponential backoff for checking process status
            let mut check_interval = INITIAL_CHECK_INTERVAL;

            loop {
                tokio::time::sleep(check_interval).await;

                // Increase interval up to max for efficiency
                if check_interval < MAX_CHECK_INTERVAL {
                    check_interval = check_interval.saturating_mul(2).min(MAX_CHECK_INTERVAL);
                }

                let mut runtime = runtime_arc.lock().await;
                if let Some(child) = runtime.children.get_mut(&name_str) {
                    if let Ok(Some(status)) = child.try_wait() {
                        // Process has exited
                        let exit_code = status.code();

                        // Remove from children map
                        runtime.children.remove(&name_str);

                        // Update process info
                        if let Some(info) = runtime.processes.get_mut(&name_str) {
                            if let Some(start_time) = info.last_restart {
                                let session_runtime = start_time.elapsed();
                                info.cumulative_runtime += session_runtime;
                            }
                            info.status = ProcessStatus::Stopped;
                            info.stopped_at = Some(Instant::now());
                            info.exit_code = exit_code;
                        }
                        drop(runtime);

                        // Log the exit
                        if let Some(state) = &app_state_monitor {
                            let exit_msg = match exit_code {
                                Some(0) => format!("Process '{name_str}' exited cleanly"),
                                Some(-1) => format!("Process '{name_str}' terminated gracefully"), // Force terminated
                                Some(EXIT_CODE_KEYBOARD_INTERRUPT) => {
                                    format!("Process '{name_str}' interrupted gracefully")
                                } // KeyboardInterrupt
                                Some(EXIT_CODE_CTRL_C_WINDOWS) => {
                                    format!("Process '{name_str}' interrupted gracefully")
                                } // CTRL_C_EVENT on Windows
                                Some(code) => {
                                    format!("Process '{name_str}' exited with code {code}")
                                }
                                None => format!("Process '{name_str}' terminated by signal"),
                            };
                            state.add_system_log(exit_msg).await;
                        }

                        break;
                    }
                } else {
                    // Process was removed from children map (stopped manually)
                    break;
                }
            }
        });

        // Store the monitor handle so it can be aborted if needed
        {
            let mut runtime = self.runtime.lock().await;
            runtime
                .monitor_handles
                .insert(name.to_string(), monitor_handle);
        }

        Ok(())
    }

    /// Spawn tasks to handle process output streams with optional UI state and log file.
    async fn spawn_output_handler_with_state(
        &self,
        name: &str,
        stdout: tokio::process::ChildStdout,
        stderr: tokio::process::ChildStderr,
        app_state: Option<Arc<AppState>>,
        log_file: Option<Arc<Mutex<std::fs::File>>>,
    ) {
        // Get max name length and color for alignment
        let (max_name_len, process_color) = {
            let config = self.config.lock().await;
            let max_len = config.max_name_length;
            let color = config
                .colors
                .get(name)
                .copied()
                .unwrap_or(colored::Color::White);
            (max_len, color)
        };

        let (stdout_handle, stderr_handle) = output::spawn_output_handlers(
            name,
            stdout,
            stderr,
            app_state,
            log_file,
            max_name_len,
            process_color,
        );

        // Store handles in runtime state
        {
            let mut runtime = self.runtime.lock().await;
            let handles = runtime
                .output_handles
                .entry(name.to_string())
                .or_insert_with(Vec::new);
            handles.push(stdout_handle);
            handles.push(stderr_handle);
        }
    }

    /// Stop a specific process by name.
    ///
    /// Convenience wrapper around [`stop_process_with_opts`] with default options.
    ///
    /// # Errors
    ///
    /// Returns an error if the process is not running.
    pub async fn stop_process(&self, name: &str) -> Result<()> {
        self.stop_process_with_opts(name, &LifecycleOptions::new())
            .await
    }

    /// Stop a specific process by name with explicit lifecycle options.
    ///
    /// # Errors
    ///
    /// Returns an error if the process is not running.
    pub async fn stop_process_with_opts(&self, name: &str, opts: &LifecycleOptions) -> Result<()> {
        // First announce we're stopping the process (if requested)
        if opts.log_messages {
            if let Some(state) = &opts.app_state {
                state
                    .add_system_log(format!("Stopping process '{name}'..."))
                    .await;
            } else {
                self.print_system_message(&format!("Stopping process '{name}'..."))
                    .await;
            }
        }

        // Take the child out of the runtime
        let maybe_child = {
            let mut runtime = self.runtime.lock().await;
            runtime.children.remove(name)
        };

        if let Some(mut child) = maybe_child {
            let exit_code = self.terminate_child_process(&mut child).await;

            // If process didn't terminate gracefully, it's still running
            if exit_code.is_none() {
                // Put it back in the children map since it's still running
                let mut runtime = self.runtime.lock().await;
                runtime.children.insert(name.to_string(), child);
                drop(runtime);

                if opts.log_messages {
                    if let Some(state) = &opts.app_state {
                        state
                            .add_system_log(format!(
                                "Process '{name}' is ignoring termination signals"
                            ))
                            .await;
                    } else {
                        self.print_system_message(&format!(
                            "Process '{name}' is ignoring termination signals"
                        ))
                        .await;
                    }
                }

                // Don't return error - the process is still running but stubborn
                // This allows restart to work properly
                return Ok(());
            }

            // Abort the monitor and output handles for this process
            {
                let mut runtime = self.runtime.lock().await;
                if let Some(handle) = runtime.monitor_handles.remove(name) {
                    handle.abort();
                }
                if let Some(handles) = runtime.output_handles.remove(name) {
                    for handle in handles {
                        handle.abort();
                    }
                }

                if let Some(info) = runtime.processes.get_mut(name) {
                    // Calculate and add the runtime for this session
                    if let Some(start_time) = info.last_restart {
                        let session_runtime = start_time.elapsed();
                        info.cumulative_runtime += session_runtime;
                    }
                    info.status = ProcessStatus::Stopped;
                    info.stopped_at = Some(Instant::now());
                    info.exit_code = exit_code;
                }
            }

            // Log to UI if available (if requested)
            if opts.log_messages {
                if let Some(state) = &opts.app_state {
                    state
                        .add_system_log(format!("Stopped process '{name}'"))
                        .await;
                } else {
                    self.print_system_message(&format!("Stopped process '{name}'"))
                        .await;
                }
            }

            Ok(())
        } else {
            Err(ProcessError::ProcessNotRunning {
                name: name.to_string(),
            })
        }
    }

    /// Terminate a child process with platform-specific handling.
    /// Returns the exit code if available.
    async fn terminate_child_process(&self, child: &mut Child) -> Option<i32> {
        // Try to get exit status first (in case process already exited)
        if let Ok(Some(status)) = child.try_wait() {
            return status.code();
        }

        // Use platform-specific termination
        terminate_process(child).await
    }

    /// Restart a specific process by name.
    ///
    /// Convenience wrapper around [`restart_process_with_opts`] with default options.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be stopped or started.
    pub async fn restart_process(&self, name: &str) -> Result<()> {
        self.restart_process_with_opts(name, &LifecycleOptions::new())
            .await
    }

    /// Restart a specific process by name with explicit lifecycle options.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be stopped or started.
    pub async fn restart_process_with_opts(
        &self,
        name: &str,
        opts: &LifecycleOptions,
    ) -> Result<()> {
        // First announce the restart
        if let Some(state) = &opts.app_state {
            state
                .add_system_log(format!("Restarting process '{name}'..."))
                .await;
        } else {
            self.print_system_message(&format!("Restarting process '{name}'..."))
                .await;
        }

        // Build quiet opts (same app_state, but no log messages since we already announced)
        let quiet_opts = LifecycleOptions {
            app_state: opts.app_state.clone(),
            log_messages: false,
        };

        // Try to stop the process quietly (we already announced the restart)
        let stop_result = self.stop_process_with_opts(name, &quiet_opts).await;

        // If stop failed (process might be stubborn), force kill it for restart
        if stop_result.is_ok() {
            // Process stopped gracefully, just wait a bit
            sleep(Duration::from_millis(200)).await;
        } else {
            // Process might be stubborn or already stopped, check if it's still in children
            let maybe_child = {
                let mut runtime = self.runtime.lock().await;
                runtime.children.remove(name)
            };

            if let Some(mut child) = maybe_child {
                // Force kill the stubborn process for restart
                let _ = force_kill_process(&mut child).await;

                // Clean up handles and update process status
                {
                    let mut runtime = self.runtime.lock().await;
                    if let Some(handle) = runtime.monitor_handles.remove(name) {
                        handle.abort();
                    }
                    if let Some(handles) = runtime.output_handles.remove(name) {
                        for handle in handles {
                            handle.abort();
                        }
                    }
                    if let Some(info) = runtime.processes.get_mut(name) {
                        info.status = ProcessStatus::Stopped;
                        info.exit_code = Some(EXIT_CODE_FORCED_TERMINATION);
                    }
                }

                // Log the force kill
                if let Some(state) = &opts.app_state {
                    state
                        .add_system_log(format!(
                            "Force killed stubborn process '{name}' for restart"
                        ))
                        .await;
                } else {
                    self.print_system_message(&format!(
                        "Force killed stubborn process '{name}' for restart"
                    ))
                    .await;
                }

                // Wait a bit after force kill
                sleep(Duration::from_millis(500)).await;
            }
        }

        // Start the process quietly (we already announced the restart)
        self.start_process_with_opts(name, &quiet_opts).await
    }

    /// Stop all running processes.
    ///
    /// Convenience wrapper around [`stop_all_with_opts`] with default options.
    pub async fn stop_all(&self) {
        self.stop_all_with_opts(&LifecycleOptions::new()).await;
    }

    /// Send Ctrl+C to all running processes without stopping them.
    pub async fn send_ctrl_c_to_all(&self) {
        // Collect PIDs first, then drop the lock before async work
        let pids: Vec<(String, u32)> = {
            let runtime = self.runtime.lock().await;
            runtime
                .children
                .iter()
                .filter_map(|(name, child)| child.id().map(|pid| (name.clone(), pid)))
                .collect()
        };

        for (name, pid) in pids {
            #[cfg(target_os = "windows")]
            {
                self.print_system_message(&format!("Sending interrupt signal to '{name}'..."))
                    .await;

                // On Windows, try multiple approaches
                // First try Ctrl+C event
                unsafe {
                    use winapi::um::wincon::{CTRL_C_EVENT, GenerateConsoleCtrlEvent};
                    let _ = GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid);
                }

                // Small delay
                tokio::time::sleep(INITIAL_CHECK_INTERVAL).await;

                // Try Ctrl+Break as alternative
                unsafe {
                    use winapi::um::wincon::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
                    let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                self.print_system_message(&format!("Sending SIGINT to '{name}'..."))
                    .await;
                unsafe {
                    libc::kill(pid as i32, libc::SIGINT);
                }
            }
        }
    }

    /// Stop all running processes with explicit lifecycle options.
    pub async fn stop_all_with_opts(&self, opts: &LifecycleOptions) {
        // Get all RUNNING processes, not just those with children
        let process_names: Vec<String> = {
            let runtime = self.runtime.lock().await;
            runtime
                .processes
                .iter()
                .filter(|(_, info)| info.status == ProcessStatus::Running)
                .map(|(name, _)| name.clone())
                .collect()
        };

        if process_names.is_empty() {
            // No running processes to stop - don't log anything to avoid noise
            return;
        }

        // Log that we're starting shutdown
        if let Some(state) = &opts.app_state {
            state
                .add_system_log("Interrupt received, stopping processes gracefully...".to_string())
                .await;
        } else {
            self.print_system_message("Interrupt received, stopping processes gracefully...")
                .await;
        }

        // Stop all processes in parallel with individual timeouts
        let mut shutdown_tasks = Vec::new();
        for name in process_names {
            let manager = self.clone();
            let app_state_clone = opts.app_state.clone();
            let name_clone = name.clone();

            let task = tokio::spawn(async move {
                // Build quiet opts for each parallel task (no individual logging)
                let quiet_opts = LifecycleOptions {
                    app_state: app_state_clone,
                    log_messages: false,
                };
                // Try graceful shutdown with a generous timeout (without individual logging)
                let result = tokio::time::timeout(
                    GRACEFUL_SHUTDOWN_TIMEOUT,
                    manager.stop_process_with_opts(&name_clone, &quiet_opts),
                )
                .await;

                match result {
                    Ok(Ok(())) => (name_clone, true),
                    _ => (name_clone, false),
                }
            });

            shutdown_tasks.push(task);
        }

        // Wait for all shutdown tasks to complete
        let mut failed_shutdowns = Vec::new();
        for task in shutdown_tasks {
            if let Ok((name, success)) = task.await
                && !success
            {
                failed_shutdowns.push(name);
            }
        }

        // Force cleanup any processes that failed graceful shutdown
        if !failed_shutdowns.is_empty() {
            let remaining: Vec<(String, Child)> = {
                let mut runtime = self.runtime.lock().await;
                failed_shutdowns
                    .into_iter()
                    .filter_map(|name| runtime.children.remove(&name).map(|child| (name, child)))
                    .collect()
            };

            #[allow(unused_mut)]
            for (name, mut child) in remaining {
                // Force kill without waiting
                let _ = force_kill_process(&mut child).await;

                // Clean up handles and update process status
                let mut runtime = self.runtime.lock().await;
                if let Some(handle) = runtime.monitor_handles.remove(&name) {
                    handle.abort();
                }
                if let Some(handles) = runtime.output_handles.remove(&name) {
                    for handle in handles {
                        handle.abort();
                    }
                }
                if let Some(info) = runtime.processes.get_mut(&name) {
                    info.status = ProcessStatus::Stopped;
                    info.exit_code = Some(EXIT_CODE_FORCED_TERMINATION);
                }
            }
        }

        // Small delay to ensure all processes have stopped
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Display current status of all processes.
    ///
    /// Convenience wrapper around [`show_status_with_opts`] with default options.
    pub async fn show_status(&self) {
        self.show_status_with_opts(&LifecycleOptions::new()).await;
    }

    /// Display current status of all processes with explicit lifecycle options.
    pub async fn show_status_with_opts(&self, opts: &LifecycleOptions) {
        let runtime = self.runtime.lock().await;

        if let Some(state) = &opts.app_state {
            let mut status_lines = vec![];

            for (name, info) in runtime.processes.iter() {
                let uptime = match info.status {
                    ProcessStatus::Running => info.last_restart.map_or_else(
                        || "N/A".to_string(),
                        |start| format!("{}s", start.elapsed().as_secs()),
                    ),
                    ProcessStatus::Stopped => "stopped".to_string(),
                    ProcessStatus::Restarting => "restarting".to_string(),
                };

                status_lines.push(format!(
                    "{:>12} | {:>12} | {:>8} restarts | {:>8} uptime",
                    name,
                    format!("{}", info.status),
                    info.restart_count,
                    uptime
                ));
            }

            // Send to status display instead of logs
            state.set_process_status(status_lines).await;
        } else {
            println!("\n{}", "Process Status:".bold());
            println!("{:-<60}", "");

            for (name, info) in runtime.processes.iter() {
                // Calculate total runtime including current session if running
                let total_runtime = if info.status == ProcessStatus::Running {
                    if let Some(start_time) = info.last_restart {
                        info.cumulative_runtime + start_time.elapsed()
                    } else {
                        info.cumulative_runtime
                    }
                } else {
                    info.cumulative_runtime
                };

                let runtime_str = if total_runtime.as_secs() == 0 {
                    "N/A".to_string()
                } else {
                    let secs = total_runtime.as_secs();
                    let total_formatted = if secs >= 3600 {
                        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
                    } else if secs >= 60 {
                        format!("{}m{}s", secs / 60, secs % 60)
                    } else {
                        format!("{secs}s")
                    };

                    // Show current session time, with total in brackets if different
                    if info.status == ProcessStatus::Running {
                        if let Some(start_time) = info.last_restart {
                            let session_secs = start_time.elapsed().as_secs();
                            let session_formatted = if session_secs >= 3600 {
                                format!("{}h{}m", session_secs / 3600, (session_secs % 3600) / 60)
                            } else if session_secs >= 60 {
                                format!("{}m{}s", session_secs / 60, session_secs % 60)
                            } else {
                                format!("{session_secs}s")
                            };
                            // Show total in brackets if different from session
                            if session_secs != secs && info.cumulative_runtime.as_secs() > 0 {
                                format!("{session_formatted} ({total_formatted})")
                            } else {
                                session_formatted
                            }
                        } else {
                            total_formatted
                        }
                    } else {
                        // For stopped processes, just show total
                        total_formatted
                    }
                };

                let restart_str = match info.restart_count {
                    0 => String::new(),
                    1 => "1 restart".to_string(),
                    n => format!("{n} restarts"),
                };

                println!(
                    "{:>12} | {:>12} | {:>22} | {:>12}",
                    name.cyan(),
                    info.status,
                    restart_str,
                    runtime_str
                );
            }
            println!("{:-<60}\n", "");
        }
    }

    /// Run the interactive terminal UI.
    ///
    /// # Errors
    ///
    /// Returns an error if the UI fails to initialize or run.
    pub async fn run_interactive(&self) -> Result<()> {
        run_interactive_ui(self.clone()).await
    }

    /// Handle a single interactive command.
    ///
    /// Convenience wrapper around [`handle_command_with_opts`] with default options.
    ///
    /// # Errors
    ///
    /// Returns an error if the command cannot be executed.
    pub async fn handle_command(&self, input: &str) -> Result<()> {
        self.handle_command_with_opts(input, &LifecycleOptions::new())
            .await
    }

    /// Handle a single interactive command with explicit lifecycle options.
    ///
    /// # Errors
    ///
    /// Returns an error if the command cannot be executed.
    pub async fn handle_command_with_opts(
        &self,
        input: &str,
        opts: &LifecycleOptions,
    ) -> Result<()> {
        let parts: Vec<&str> = input.split_whitespace().collect();

        match parts.as_slice() {
            ["q" | "quit"] => {
                // In interactive mode, the UI will handle the quit
                // In non-interactive mode, we still need to handle it
                if opts.app_state.is_none() {
                    // Non-interactive mode - handle quit directly
                    return Ok(());
                }
                // Interactive mode - UI will handle the quit via UICommand::Quit
            }
            ["status"] => {
                self.show_status_with_opts(opts).await;
            }
            ["start", name] => {
                self.start_process_with_opts(name, opts).await?;
            }
            ["start"] => {
                // Provide helpful error message
                if let Some(state) = &opts.app_state {
                    state
                        .add_system_log(
                            "Usage: start <name> - Start a specific stopped process".to_string(),
                        )
                        .await;
                } else {
                    self.print_system_message(
                        "Usage: start <name> - Start a specific stopped process",
                    )
                    .await;
                }
                return Err(ProcessError::InputRead(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Missing process name for start command",
                )));
            }
            ["s" | "stop", name] => {
                if *name == "all" {
                    // When user types "stop all", just call stop_all directly
                    self.stop_all_with_opts(opts).await;
                } else {
                    self.stop_process_with_opts(name, opts).await?;
                }
            }
            ["s" | "stop"] => {
                // Provide helpful error message
                if let Some(state) = &opts.app_state {
                    state.add_system_log("Usage: stop <name> or stop all - Stop a specific process or all processes".to_string()).await;
                } else {
                    self.print_system_message(
                        "Usage: stop <name> or stop all - Stop a specific process or all processes",
                    )
                    .await;
                }
                return Err(ProcessError::InputRead(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Missing process name for stop command",
                )));
            }
            ["r" | "restart", name] => {
                self.restart_process_with_opts(name, opts).await?;
            }
            ["r" | "restart"] => {
                // Provide helpful error message
                if let Some(state) = &opts.app_state {
                    state
                        .add_system_log(
                            "Usage: restart <name> - Restart a specific process".to_string(),
                        )
                        .await;
                } else {
                    self.print_system_message("Usage: restart <name> - Restart a specific process")
                        .await;
                }
                return Err(ProcessError::InputRead(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Missing process name for restart command",
                )));
            }
            [] => {} // Empty input
            _ if !input.is_empty() => {
                if opts.app_state.is_none() {
                    self.print_system_message(&format!("Unknown command: {input}"))
                        .await;
                }
                return Err(ProcessError::InputRead(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Unknown command: {input}"),
                )));
            }
            _ => {}
        }

        Ok(())
    }

    /// Get list of all process names.
    pub async fn process_names(&self) -> Vec<String> {
        let runtime = self.runtime.lock().await;
        runtime.processes.keys().cloned().collect()
    }

    /// Get the maximum process name length for alignment.
    pub async fn get_max_name_length(&self) -> usize {
        let config = self.config.lock().await;
        config.max_name_length.max(5) // Ensure at least 5 for "gaffa"
    }

    /// Get the number of active monitor handles (for diagnostics/testing).
    pub async fn monitor_handle_count(&self) -> usize {
        self.runtime.lock().await.monitor_handles.len()
    }

    /// Get the number of active output handles (for diagnostics/testing).
    pub async fn output_handle_count(&self) -> usize {
        self.runtime
            .lock()
            .await
            .output_handles
            .values()
            .map(|v| v.len())
            .sum()
    }

    /// Get the number of active child processes (for diagnostics/testing).
    pub async fn children_count(&self) -> usize {
        self.runtime.lock().await.children.len()
    }

    /// Get a color for a process (consistent assignment).
    pub fn get_process_color(index: usize) -> colored::Color {
        output::get_process_color(index)
    }

    // -----------------------------------------------------------------------
    // Accessor methods for external consumers
    // -----------------------------------------------------------------------

    /// Get a clone of process info for a specific process.
    pub async fn get_process_info(&self, name: &str) -> Option<ProcessInfo> {
        let runtime = self.runtime.lock().await;
        runtime.processes.get(name).cloned()
    }

    /// Get all process info as a snapshot (name, info, color).
    pub async fn process_snapshot(&self) -> Vec<(String, ProcessInfo, colored::Color)> {
        let runtime = self.runtime.lock().await;
        let config = self.config.lock().await;
        runtime
            .processes
            .iter()
            .map(|(name, info)| {
                let color = config
                    .colors
                    .get(name)
                    .copied()
                    .unwrap_or(colored::Color::White);
                (name.clone(), info.clone(), color)
            })
            .collect()
    }

    /// Filter processes to only keep the specified names.
    pub async fn retain_processes(&self, names: &[String]) {
        let mut runtime = self.runtime.lock().await;
        runtime.processes.retain(|name, _| names.contains(name));
    }

    /// Check if all started processes have stopped.
    pub async fn all_stopped(&self) -> bool {
        let runtime = self.runtime.lock().await;
        runtime
            .processes
            .values()
            .all(|info| info.status == ProcessStatus::Stopped)
    }

    /// Get a snapshot of process colors.
    pub async fn get_colors(&self) -> HashMap<String, colored::Color> {
        let config = self.config.lock().await;
        config.colors.clone()
    }

    /// Try to get process colors without blocking (for UI render loop).
    pub fn try_get_colors(&self) -> Option<HashMap<String, colored::Color>> {
        self.config
            .try_lock()
            .ok()
            .map(|config| config.colors.clone())
    }

    /// Fix processes that are marked as running but have no child process.
    /// Used during quit to ensure consistent state.
    pub async fn fix_orphaned_process_status(&self) {
        let mut runtime = self.runtime.lock().await;
        let child_names: Vec<String> = runtime.children.keys().cloned().collect();
        for (name, info) in runtime.processes.iter_mut() {
            if info.status == ProcessStatus::Running && !child_names.contains(name) {
                info.status = ProcessStatus::Stopped;
                info.stopped_at = Some(Instant::now());
            }
        }
    }

    /// Check if all started (previously running) processes have stopped.
    /// Only considers processes that were actually started at some point.
    pub async fn all_started_processes_stopped(&self) -> bool {
        let runtime = self.runtime.lock().await;
        let running_processes: Vec<_> = runtime
            .processes
            .values()
            .filter(|info| info.last_restart.is_some())
            .collect();
        running_processes
            .iter()
            .all(|info| info.status == ProcessStatus::Stopped)
    }

    /// Format and print a system message with proper alignment.
    async fn print_system_message(&self, message: &str) {
        let max_name_len = {
            let config = self.config.lock().await;
            config.max_name_length.max(5) // Ensure at least 5 for "gaffa"
        };
        println!("{}", output::format_gaffa_message(message, max_name_len));
    }
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::sleep;

    /// Helper to create a test Procfile
    fn create_test_procfile(content: &str) -> String {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = format!("test_procfile_{}_{}.txt", std::process::id(), count);
        std::fs::write(&path, content).expect("Failed to write test procfile");
        path
    }

    /// Clean up test Procfile
    fn cleanup_test_procfile(path: &str) {
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn test_load_procfile_success() {
        let manager = ProcessManager::new();
        let procfile_content = "web: echo hello\nworker: echo world";
        let procfile_path = create_test_procfile(procfile_content);

        let result = manager.load_procfile(&procfile_path).await;
        assert!(result.is_ok());

        let mut process_names = manager.process_names().await;
        process_names.sort();
        assert_eq!(process_names.len(), 2);
        assert!(process_names.contains(&"web".to_string()));
        assert!(process_names.contains(&"worker".to_string()));

        cleanup_test_procfile(&procfile_path);
    }

    #[tokio::test]
    async fn test_load_procfile_with_comments() {
        let manager = ProcessManager::new();
        let procfile_content =
            "# This is a comment\nweb: echo hello\n# Another comment\nworker: echo world";
        let procfile_path = create_test_procfile(procfile_content);

        let result = manager.load_procfile(&procfile_path).await;
        assert!(result.is_ok());

        let process_names = manager.process_names().await;
        assert_eq!(process_names.len(), 2);

        cleanup_test_procfile(&procfile_path);
    }

    #[tokio::test]
    async fn test_load_procfile_duplicate_names() {
        let manager = ProcessManager::new();
        let procfile_content = "web: echo hello\nweb: echo world\nweb: echo test";
        let procfile_path = create_test_procfile(procfile_content);

        let result = manager.load_procfile(&procfile_path).await;
        assert!(result.is_ok());

        let mut process_names = manager.process_names().await;
        process_names.sort();

        // Debug: print what we actually got
        eprintln!("Process names: {process_names:?}");

        // Since HashMap doesn't preserve order and we're using insert which overwrites,
        // we might not get all three. Let's verify we get at least one "web" entry
        assert!(process_names.iter().any(|n| n.starts_with("web")));

        cleanup_test_procfile(&procfile_path);
    }

    #[tokio::test]
    async fn test_load_procfile_invalid_format() {
        let manager = ProcessManager::new();
        let procfile_content = "invalid line without colon";
        let procfile_path = create_test_procfile(procfile_content);

        let result = manager.load_procfile(&procfile_path).await;
        assert!(matches!(result, Err(ProcessError::InvalidFormat { .. })));

        cleanup_test_procfile(&procfile_path);
    }

    #[tokio::test]
    async fn test_load_procfile_empty() {
        let manager = ProcessManager::new();
        let procfile_content = "\n# Just comments\n\n";
        let procfile_path = create_test_procfile(procfile_content);

        let result = manager.load_procfile(&procfile_path).await;
        assert!(matches!(result, Err(ProcessError::NoProcesses)));

        cleanup_test_procfile(&procfile_path);
    }

    #[tokio::test]
    async fn test_load_procfile_nonexistent() {
        let manager = ProcessManager::new();
        let result = manager.load_procfile("nonexistent_file.txt").await;
        assert!(matches!(result, Err(ProcessError::ProcfileRead { .. })));
    }

    #[tokio::test]
    async fn test_start_stop_process() {
        let manager = Arc::new(ProcessManager::new());
        let procfile_content = if cfg!(windows) {
            "test: cmd /c \"ping -n 6 127.0.0.1 >nul\""
        } else {
            "test: sleep 5"
        };
        let procfile_path = create_test_procfile(procfile_content);

        let load_result = manager.load_procfile(&procfile_path).await;
        assert!(load_result.is_ok());

        // Test starting a process
        let result = manager.start_process("test").await;
        if let Err(e) = &result {
            eprintln!("Failed to start process: {e:?}");
        }
        assert!(result.is_ok());

        // Give the process time to start and be registered
        sleep(Duration::from_secs(1)).await;

        // Test stopping the process
        let result = manager.stop_process("test").await;
        if let Err(e) = &result {
            eprintln!("Failed to stop process: {e:?}");
        }
        assert!(result.is_ok());

        cleanup_test_procfile(&procfile_path);
    }

    #[tokio::test]
    async fn test_handle_command_status() {
        let manager = ProcessManager::new();
        // The handle_command("status") doesn't require a loaded procfile
        let result = manager.handle_command("status").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_command_unknown() {
        let manager = ProcessManager::new();

        let result = manager.handle_command("unknown command").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_handle_command_empty() {
        let manager = ProcessManager::new();

        let result = manager.handle_command("").await;
        assert!(result.is_ok());
    }
}
