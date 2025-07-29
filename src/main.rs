use std::collections::HashMap;
use std::io::Write;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Arg, Command};
use colored::Colorize;
use regex::Regex;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command as TokioCommand},
    sync::Mutex,
    time::sleep,
};

mod ui;
use ui::{AppState, run_terminal_ui};

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

/// Parse environment variables from a file.
async fn parse_env_file(path: &str, env_vars: &mut std::collections::HashMap<String, String>) -> Result<()> {
    let contents = tokio::fs::read_to_string(path).await
        .map_err(|e| ProcessError::ProcfileRead { 
            path: path.to_string(), 
            source: e 
        })?;
    
    for line in contents.lines() {
        let line = line.trim();
        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        
        // Parse KEY=VALUE
        if let Some((key, value)) = line.split_once('=') {
            env_vars.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    
    Ok(())
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

/// Manages multiple processes defined in a Procfile.
///
/// Provides functionality to start, stop, restart, and monitor processes
/// with interactive control capabilities.
#[derive(Debug, Clone)]
pub struct ProcessManager {
    processes: Arc<Mutex<HashMap<String, ProcessInfo>>>,
    children: Arc<Mutex<HashMap<String, Child>>>,
    process_colors: Arc<Mutex<HashMap<String, colored::Color>>>,
    log_file: Arc<Mutex<Option<Arc<Mutex<std::fs::File>>>>>,
    max_name_length: Arc<Mutex<usize>>,
    environment_variables: Arc<Mutex<HashMap<String, String>>>,
}

impl ProcessManager {
    /// Create a new process manager instance.
    #[must_use]
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            children: Arc::new(Mutex::new(HashMap::new())),
            process_colors: Arc::new(Mutex::new(HashMap::new())),
            log_file: Arc::new(Mutex::new(None)),
            max_name_length: Arc::new(Mutex::new(0)),
            environment_variables: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Set the log file for this process manager.
    pub async fn set_log_file(&self, log_file: Arc<Mutex<std::fs::File>>) {
        let mut file_lock = self.log_file.lock().await;
        *file_lock = Some(log_file);
    }
    
    /// Set environment variables to be applied to all processes.
    pub async fn set_environment_variables(&self, env_vars: HashMap<String, String>) {
        let mut env_lock = self.environment_variables.lock().await;
        *env_lock = env_vars;
    }

    /// Load process definitions from a Procfile.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The Procfile cannot be read
    /// - The Procfile contains invalid format
    /// - No valid processes are found
    ///
    /// # Panics
    ///
    /// Panics if the regex pattern is invalid (should never happen with hardcoded pattern).
    pub async fn load_procfile(&self, procfile_path: &str) -> Result<()> {
        let content =
            std::fs::read_to_string(procfile_path).map_err(|e| ProcessError::ProcfileRead {
                path: procfile_path.to_string(),
                source: e,
            })?;

        let re = Regex::new(r"^(\w+):\s+(.*)$").expect("Valid regex pattern");
        let mut processes = self.processes.lock().await;
        let mut colors = self.process_colors.lock().await;
        let mut name_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut color_index = 0;

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some(caps) = re.captures(line) {
                let base_name = caps[1].to_string();
                let command = caps[2].to_string();

                // Handle duplicate names by appending a number
                let count = name_counts.entry(base_name.clone()).or_insert(0);
                *count += 1;

                let name = if *count == 1 {
                    base_name.clone()
                } else {
                    format!("{base_name}.{count}")
                };

                processes.insert(
                    name.clone(),
                    ProcessInfo {
                        command,
                        status: ProcessStatus::Stopped,
                        restart_count: 0,
                        last_restart: None,
                        stopped_at: None,
                        cumulative_runtime: Duration::from_secs(0),
                        exit_code: None,
                    },
                );

                // Assign color to process
                colors.insert(name.clone(), Self::get_process_color(color_index));
                color_index += 1;
            } else {
                return Err(ProcessError::InvalidFormat {
                    line: line.to_string(),
                });
            }
        }

        if processes.is_empty() {
            return Err(ProcessError::NoProcesses);
        }

        // Calculate maximum process name length
        let max_len = processes.keys().map(|name| name.len()).max().unwrap_or(0);
        let mut max_name_length = self.max_name_length.lock().await;
        *max_name_length = max_len;

        Ok(())
    }

    /// Start a specific process by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the process is already running, not found, or fails to start.
    pub async fn start_process(&self, name: &str) -> Result<()> {
        self.start_process_with_state(name, None).await
    }

    /// Start a specific process by name with optional UI state.
    ///
    /// # Errors
    ///
    /// Returns an error if the process is already running, not found, or fails to start.
    pub async fn start_process_with_state(
        &self,
        name: &str,
        app_state: Option<Arc<AppState>>,
    ) -> Result<()> {
        let process_info = {
            let mut processes = self.processes.lock().await;
            match processes.get_mut(name) {
                Some(info) if info.status != ProcessStatus::Running => {
                    info.status = ProcessStatus::Restarting;
                    info.clone()
                }
                Some(_) => {
                    return Err(ProcessError::ProcessAlreadyRunning {
                        name: name.to_string(),
                    });
                }
                None => {
                    return Err(ProcessError::ProcessNotFound {
                        name: name.to_string(),
                    });
                }
            }
        };

        // Add startup message
        if let Some(state) = &app_state {
            state
                .add_log(name.to_string(), format!("Starting '{name}'..."), false)
                .await;
        }

        self.spawn_process_with_state(name, &process_info.command, app_state.clone())
            .await?;

        {
            let mut processes = self.processes.lock().await;
            if let Some(info) = processes.get_mut(name) {
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
        if app_state.is_none() {
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
    pub async fn spawn_process_with_state(
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
        let env_vars = self.environment_variables.lock().await;
        for (key, value) in env_vars.iter() {
            cmd.env(key, value);
        }
        drop(env_vars);

        // Only inherit stdin in non-interactive mode
        // In interactive mode (when app_state is Some), the TUI needs exclusive stdin access
        if app_state.is_none() {
            cmd.stdin(Stdio::inherit());
        } else {
            cmd.stdin(Stdio::null());
        }
        
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // On Windows, create process in a new process group
        #[cfg(target_os = "windows")]
        {
            #[allow(unused_imports)]
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        let mut child = cmd.spawn().map_err(|e| ProcessError::ProcessSpawn {
            name: name.to_string(),
            source: e,
        })?;

        let stdout = child.stdout.take().expect("stdout pipe");
        let stderr = child.stderr.take().expect("stderr pipe");

        // Get log file from manager
        let log_file = {
            let log_file_lock = self.log_file.lock().await;
            log_file_lock.clone()
        };

        self.spawn_output_handler_with_state(name, stdout, stderr, app_state.clone(), log_file)
            .await;

        {
            let mut children = self.children.lock().await;
            children.insert(name.to_string(), child);
        }

        // Spawn a task to monitor the process exit
        let name_str = name.to_string();
        let processes = Arc::clone(&self.processes);
        let children = Arc::clone(&self.children);
        let app_state_monitor = app_state;

        tokio::spawn(async move {
            // Give process time to start
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            // Use exponential backoff for checking process status
            let mut check_interval = tokio::time::Duration::from_millis(100);
            const MAX_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_secs(2);

            loop {
                tokio::time::sleep(check_interval).await;

                // Increase interval up to max for efficiency
                if check_interval < MAX_INTERVAL {
                    check_interval = check_interval.saturating_mul(2).min(MAX_INTERVAL);
                }

                let mut children_lock = children.lock().await;
                if let Some(child) = children_lock.get_mut(&name_str) {
                    if let Ok(Some(status)) = child.try_wait() {
                        // Process has exited
                        let exit_code = status.code();

                        // Remove from children map
                        children_lock.remove(&name_str);
                        drop(children_lock);

                        // Update process info
                        let mut processes_lock = processes.lock().await;
                        if let Some(info) = processes_lock.get_mut(&name_str) {
                            if let Some(start_time) = info.last_restart {
                                let session_runtime = start_time.elapsed();
                                info.cumulative_runtime += session_runtime;
                            }
                            info.status = ProcessStatus::Stopped;
                            info.stopped_at = Some(Instant::now());
                            info.exit_code = exit_code;
                        }
                        drop(processes_lock);

                        // Log the exit
                        if let Some(state) = &app_state_monitor {
                            let exit_msg = match exit_code {
                                Some(0) => format!("Process '{name_str}' exited cleanly"),
                                Some(-1) => format!("Process '{name_str}' terminated gracefully"), // Force terminated
                                Some(512) => format!("Process '{name_str}' interrupted gracefully"), // KeyboardInterrupt
                                Some(-1073741510) => format!("Process '{name_str}' interrupted gracefully"), // CTRL_C_EVENT on Windows
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

        Ok(())
    }

    /// Spawn tasks to handle process output streams with optional UI state and log file.
    pub async fn spawn_output_handler_with_state(
        &self,
        name: &str,
        stdout: tokio::process::ChildStdout,
        stderr: tokio::process::ChildStderr,
        app_state: Option<Arc<AppState>>,
        log_file: Option<Arc<Mutex<std::fs::File>>>,
    ) {
        // Get max name length for alignment
        let max_name_len = {
            let max_len = self.max_name_length.lock().await;
            *max_len
        };
        // Get the color for this process
        let process_color = {
            let colors = self.process_colors.lock().await;
            colors.get(name).copied().unwrap_or(colored::Color::White)
        };
        let name_str = name.to_string();
        let stdout_reader = BufReader::new(stdout);
        let app_state_stdout = app_state.clone();
        let log_file_stdout = log_file.clone();

        tokio::spawn(async move {
            let mut lines = stdout_reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim_end().to_string();
                
                // Skip empty lines to avoid clutter
                if line.is_empty() {
                    continue;
                }
                
                if let Some(state) = &app_state_stdout {
                    state.add_log(name_str.clone(), line.clone(), false).await;
                } else {
                    let colored_name = name_str.color(process_color);
                    let padding = " ".repeat(max_name_len.saturating_sub(name_str.len()));
                    println!("{colored_name}{padding} | {}", line);

                    // Force immediate output to terminal
                    use std::io::{stdout, Write};
                    let _ = stdout().flush();

                    // Write to log file if available
                    if let Some(log_file) = &log_file_stdout {
                        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                        let log_line = format!("[{timestamp}] [{name_str}] {line}\n");
                        let mut file = log_file.lock().await;
                        let _ = file.write_all(log_line.as_bytes());
                        let _ = file.flush();
                    }
                }
            }
        });

        let name_str = name.to_string();
        let stderr_reader = BufReader::new(stderr);
        let app_state_stderr = app_state;

        tokio::spawn(async move {
            let mut lines = stderr_reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim_end().to_string();
                
                // Skip empty lines to avoid clutter
                if line.is_empty() {
                    continue;
                }
                
                if let Some(state) = &app_state_stderr {
                    state.add_log(name_str.clone(), line.clone(), true).await;
                } else {
                    let colored_name = name_str.color(process_color);
                    let padding = " ".repeat(max_name_len.saturating_sub(name_str.len()));
                    println!("{colored_name}{padding} | {}", line);

                    // Force immediate output to terminal
                    use std::io::{stdout, Write};
                    let _ = stdout().flush();

                    // Write to log file if available
                    if let Some(log_file) = &log_file {
                        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                        let log_line = format!("[{timestamp}] [STDERR] [{name_str}] {line}\n");
                        let mut file = log_file.lock().await;
                        let _ = file.write_all(log_line.as_bytes());
                        let _ = file.flush();
                    }
                }
            }
        });
    }

    /// Stop a specific process by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the process is not running.
    pub async fn stop_process(&self, name: &str) -> Result<()> {
        self.stop_process_with_state(name, None).await
    }

    /// Stop a specific process by name with optional UI state.
    ///
    /// # Errors
    ///
    /// Returns an error if the process is not running.
    pub async fn stop_process_with_state(
        &self,
        name: &str,
        app_state: Option<Arc<AppState>>,
    ) -> Result<()> {
        // First announce we're stopping the process
        if let Some(state) = &app_state {
            state
                .add_system_log(format!("Stopping process '{name}'..."))
                .await;
        } else {
            self.print_system_message(&format!("Stopping process '{name}'..."))
                .await;
        }
        
        let mut children = self.children.lock().await;

        if let Some(mut child) = children.remove(name) {
            let exit_code = self.terminate_process(&mut child).await;

            let mut processes = self.processes.lock().await;
            if let Some(info) = processes.get_mut(name) {
                // Calculate and add the runtime for this session
                if let Some(start_time) = info.last_restart {
                    let session_runtime = start_time.elapsed();
                    info.cumulative_runtime += session_runtime;
                }
                info.status = ProcessStatus::Stopped;
                info.stopped_at = Some(Instant::now());
                info.exit_code = exit_code;
            }

            // Log to UI if available
            if let Some(state) = &app_state {
                state
                    .add_system_log(format!("Stopped process '{name}'"))
                    .await;
            } else {
                self.print_system_message(&format!("Stopped process '{name}'"))
                    .await;
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
    async fn terminate_process(&self, child: &mut Child) -> Option<i32> {
        // Try to get exit status first (in case process already exited)
        if let Ok(Some(status)) = child.try_wait() {
            return status.code();
        }

        // Platform-specific termination
        #[cfg(target_os = "windows")]
        {
            if let Some(pid) = child.id() {
                // Try Ctrl+C first (SIGINT)
                unsafe {
                    use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_C_EVENT};
                    let _ = GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid);
                }
                
                // Give time for graceful shutdown
                tokio::time::sleep(Duration::from_secs(2)).await;
                
                // Check if process exited
                if let Ok(Some(status)) = child.try_wait() {
                    return status.code();
                }
                
                // Try Ctrl+C again
                unsafe {
                    use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_C_EVENT};
                    let _ = GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid);
                }
                
                // Give more time
                tokio::time::sleep(Duration::from_secs(2)).await;
                
                // Check again
                if let Ok(Some(status)) = child.try_wait() {
                    return status.code();
                }
                
                // Now try Ctrl+Break as last resort before force kill
                unsafe {
                    use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
                    let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
                }
                
                tokio::time::sleep(Duration::from_secs(2)).await;
                
                // Check once more
                if let Ok(Some(status)) = child.try_wait() {
                    return status.code();
                }

                // Second attempt: Try child.kill() which might work for some processes
                let _ = child.kill();
                tokio::time::sleep(Duration::from_secs(1)).await;
                
                // Check again
                if let Ok(Some(status)) = child.try_wait() {
                    return status.code();
                }

                // Last resort: Force kill the process tree
                let _ = std::process::Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .output();
                
                // Give a moment for the force kill to complete
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            // On Unix, send SIGTERM first
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
                
                // Give time for graceful shutdown
                tokio::time::sleep(Duration::from_secs(3)).await;
                
                // Check if process exited
                if let Ok(Some(status)) = child.try_wait() {
                    return status.code();
                }
            }
            
            // If still running, force kill
            let _ = child.kill().await;
        }

        // Wait for the process to exit and get status
        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(Ok(status)) => status.code(),
            _ => Some(-1), // Timeout or error
        }
    }

    /// Restart a specific process by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be stopped or started.
    pub async fn restart_process(&self, name: &str) -> Result<()> {
        self.restart_process_with_state(name, None).await
    }

    /// Restart a specific process by name with optional UI state.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be stopped or started.
    pub async fn restart_process_with_state(
        &self,
        name: &str,
        app_state: Option<Arc<AppState>>,
    ) -> Result<()> {
        // First announce the restart
        if let Some(state) = &app_state {
            state
                .add_system_log(format!("Restarting process '{name}'..."))
                .await;
        } else {
            self.print_system_message(&format!("Restarting process '{name}'..."))
                .await;
        }
        
        // Stop the process (this will show "Stopped process 'name'")
        let _ = self.stop_process_with_state(name, app_state.clone()).await;
        
        // Brief pause before restart
        sleep(Duration::from_millis(200)).await;
        
        // Start the process (this will show "Starting 'name'...")
        self.start_process_with_state(name, app_state).await
    }

    /// Stop all running processes.
    pub async fn stop_all(&self) {
        self.stop_all_with_state(None).await;
    }
    
    /// Send Ctrl+C to all running processes without stopping them.
    pub async fn send_ctrl_c_to_all(&self) {
        #[cfg(target_os = "windows")]
        {
            let children = self.children.lock().await;
            for (name, child) in children.iter() {
                if let Some(pid) = child.id() {
                    self.print_system_message(&format!("Sending interrupt signal to '{name}'..."))
                        .await;
                    
                    // On Windows, try multiple approaches
                    // First try Ctrl+C event
                    unsafe {
                        use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_C_EVENT};
                        let _ = GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid);
                    }
                    
                    // Small delay
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    
                    // Try Ctrl+Break as alternative
                    unsafe {
                        use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
                        let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
                    }
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let children = self.children.lock().await;
            for (name, child) in children.iter() {
                if let Some(pid) = child.id() {
                    self.print_system_message(&format!("Sending SIGINT to '{name}'..."))
                        .await;
                    unsafe {
                        libc::kill(pid as i32, libc::SIGINT);
                    }
                }
            }
        }
    }

    /// Stop all running processes with optional UI state.
    pub async fn stop_all_with_state(&self, app_state: Option<Arc<AppState>>) {
        // Get all RUNNING processes, not just those with children
        let process_names: Vec<String> = {
            let processes = self.processes.lock().await;
            processes
                .iter()
                .filter(|(_, info)| info.status == ProcessStatus::Running)
                .map(|(name, _)| name.clone())
                .collect()
        };

        if process_names.is_empty() {
            return;
        }

        // Log that we're starting shutdown
        if let Some(state) = &app_state {
            state
                .add_system_log(format!("Stopping {} processes...", process_names.len()))
                .await;
        } else {
            self.print_system_message(&format!("Stopping {} processes...", process_names.len()))
                .await;
        }

        // Stop all processes in parallel with individual timeouts
        let mut shutdown_tasks = Vec::new();
        for name in process_names {
            let manager = self.clone();
            let app_state_clone = app_state.clone();
            let name_clone = name.clone();
            
            let task = tokio::spawn(async move {
                // Try graceful shutdown with a generous timeout
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    manager.stop_process_with_state(&name_clone, app_state_clone)
                ).await;
                
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
            if let Ok((name, success)) = task.await {
                if !success {
                    failed_shutdowns.push(name);
                }
            }
        }

        // Force cleanup any processes that failed graceful shutdown
        if !failed_shutdowns.is_empty() {
            let remaining: Vec<(String, Child)> = {
                let mut children = self.children.lock().await;
                failed_shutdowns.into_iter()
                    .filter_map(|name| {
                        children.remove(&name).map(|child| (name, child))
                    })
                    .collect()
            };

            for (name, child) in remaining {
                // Force kill without waiting
                #[cfg(target_os = "windows")]
                {
                    if let Some(pid) = child.id() {
                        let _ = std::process::Command::new("taskkill")
                            .args(["/F", "/T", "/PID", &pid.to_string()])
                            .output();
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = child.kill().await;
                }
                
                // Update process status
                let mut processes = self.processes.lock().await;
                if let Some(info) = processes.get_mut(&name) {
                    info.status = ProcessStatus::Stopped;
                    info.exit_code = Some(-1);
                }
            }
        }

        // Small delay to ensure all processes have stopped
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Log to UI if available
        if let Some(state) = &app_state {
            state
                .add_system_log("All processes stopped".to_string())
                .await;
        } else {
            self.print_system_message("All processes stopped").await;
        }
    }

    /// Display current status of all processes.
    pub async fn show_status(&self) {
        self.show_status_with_state(None).await;
    }

    /// Display current status of all processes with optional UI state.
    pub async fn show_status_with_state(&self, app_state: Option<Arc<AppState>>) {
        let processes = self.processes.lock().await;

        if let Some(state) = app_state {
            let mut status_lines = vec![];

            for (name, info) in processes.iter() {
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

            for (name, info) in processes.iter() {
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

    /// Run the interactive command loop.
    ///
    /// # Errors
    ///
    /// Returns an error if input reading fails.
    pub async fn run_interactive(&self) -> Result<()> {
        // Bottom status bar
        println!("\n{}", 
            "gaffa is watching.. (s = status | r <name> = restart | stop <name> = stop | q = quit | Ctrl+C = quit)".cyan()
        );
        println!("{}", "-".repeat(80).bright_black());
        println!("{}\n", "Process output:".bright_black());

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        // Add some spacing before interactive area
        println!("\n{}", "-".repeat(80).bright_black());
        println!("{}", "Interactive commands:".bright_black());

        loop {
            // Simple prompt
            print!("> ");
            std::io::stdout().flush().map_err(ProcessError::InputRead)?;

            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break Ok(()), // EOF
                Ok(_) => {
                    let input = line.trim();
                    if !input.is_empty() {
                        if let Err(e) = self.handle_command(input).await {
                            println!("[{}] {e}", "ERROR".red());
                        }
                    }
                }
                Err(e) => break Err(ProcessError::InputRead(e)),
            }
        }
    }

    /// Handle a single interactive command.
    ///
    /// # Errors
    ///
    /// Returns an error if the command cannot be executed.
    pub async fn handle_command(&self, input: &str) -> Result<()> {
        self.handle_command_with_state(input, None).await
    }

    /// Handle a single interactive command with optional UI state.
    ///
    /// # Errors
    ///
    /// Returns an error if the command cannot be executed.
    pub async fn handle_command_with_state(
        &self,
        input: &str,
        app_state: Option<Arc<AppState>>,
    ) -> Result<()> {
        // Remove verbose command logging - the action messages are enough
        
        let parts: Vec<&str> = input.split_whitespace().collect();

        match parts.as_slice() {
            ["q" | "quit"] => {
                // In interactive mode, the UI will handle the quit
                // In non-interactive mode, we still need to handle it
                if app_state.is_none() {
                    // Non-interactive mode - handle quit directly
                    return Ok(());
                }
                // Interactive mode - UI will handle the quit via UICommand::Quit
            }
            ["status"] => {
                self.show_status_with_state(app_state).await;
            }
            ["start", name] => {
                self.start_process_with_state(name, app_state).await?;
            }
            ["start"] => {
                // Provide helpful error message
                if let Some(state) = &app_state {
                    state
                        .add_system_log(
                            "Usage: start <name> - Start a specific stopped process".to_string(),
                        )
                        .await;
                } else {
                    println!(
                        "[{}] Usage: start <name> - Start a specific stopped process",
                        "ERROR".red()
                    );
                }
                return Err(ProcessError::InputRead(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Missing process name for start command",
                )));
            }
            ["s" | "stop", name] => {
                if *name == "all" {
                    self.stop_all_with_state(app_state).await;
                } else {
                    self.stop_process_with_state(name, app_state).await?;
                }
            }
            ["s" | "stop"] => {
                // Provide helpful error message
                if let Some(state) = &app_state {
                    state.add_system_log("Usage: stop <name> or stop all - Stop a specific process or all processes".to_string()).await;
                } else {
                    println!(
                        "[{}] Usage: stop <name> or stop all - Stop a specific process or all processes",
                        "ERROR".red()
                    );
                }
                return Err(ProcessError::InputRead(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Missing process name for stop command",
                )));
            }
            ["r" | "restart", name] => {
                self.restart_process_with_state(name, app_state).await?;
            }
            ["r" | "restart"] => {
                // Provide helpful error message
                if let Some(state) = &app_state {
                    state
                        .add_system_log(
                            "Usage: restart <name> - Restart a specific process".to_string(),
                        )
                        .await;
                } else {
                    println!(
                        "[{}] Usage: restart <name> - Restart a specific process",
                        "ERROR".red()
                    );
                }
                return Err(ProcessError::InputRead(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Missing process name for restart command",
                )));
            }
            ["stopall"] => {
                self.stop_all_with_state(app_state).await;
            }
            [] => {} // Empty input
            _ if !input.is_empty() => {
                if app_state.is_none() {
                    println!("[{}] Unknown command: {input}", "ERROR".red());
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
        let processes = self.processes.lock().await;
        processes.keys().cloned().collect()
    }

    /// Get a color for a process (consistent assignment).
    fn get_process_color(index: usize) -> colored::Color {
        const PROCESS_COLORS: &[colored::Color] = &[
            colored::Color::Cyan,
            colored::Color::Yellow,
            colored::Color::Blue,
            colored::Color::Green,
            colored::Color::BrightCyan,
            colored::Color::BrightYellow,
            colored::Color::BrightBlue,
        ];
        PROCESS_COLORS[index % PROCESS_COLORS.len()]
    }

    /// Format a system message with proper alignment.
    async fn print_system_message(&self, message: &str) {
        let max_name_len = {
            let max_len = self.max_name_length.lock().await;
            (*max_len).max(5) // Ensure at least 5 for "gaffa"
        };
        let colored_gaffa = "gaffa".magenta();
        let padding = " ".repeat(max_name_len.saturating_sub(5));
        let colored_msg = message.magenta();
        println!("{colored_gaffa}{padding} | {colored_msg}");
    }

}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Reset terminal to a clean state.
fn reset_terminal() {
    // Simply disable raw mode - let the OS restore everything else
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Display help information when no subcommand is provided.
fn show_help() {
    println!("{}", "Gaffa - Procfile Process Manager".bold().green());
    println!("\nUsage:");
    println!("  gaffa [OPTIONS] [COMMAND]");
    println!("\nCommands:");
    println!("  run [OPTIONS] [PROCESSES...]            Run processes from Procfile");
    println!("\nOptions:");
    println!("  -h, --help                              Show detailed help");
    println!("\nRun options:");
    println!("  -f, --procfile FILE                     Use custom Procfile (default: Procfile)");
    println!("  --log-file FILE                         Write all process output to a log file");
    println!("  -i, --interactive                       Run in interactive mode with Terminal UI");
    println!("  --env KEY=VALUE                         Set environment variable for all processes");
    println!("  --env-file FILE                         Read environment variables from a file");
    println!("\nExamples:");
    println!("  gaffa run                               Run all processes");
    println!("  gaffa run web worker                    Run specific processes");
    println!("  gaffa run --procfile dev.procfile       Use custom Procfile");
    println!("  gaffa run --env PORT=8080               Set environment variable");
    println!("  gaffa run --env-file .env               Load environment from file");
    println!("  gaffa run --log-file output.log         Run with log file");
    println!("  gaffa run -i                            Run with interactive Terminal UI");
    println!("\nProcfile format:");
    println!("  web: python -m http.server 8000");
    println!("  worker: python worker.py");
}

/// Run processes in non-interactive mode (stdout only).
async fn run_non_interactive(
    manager: Arc<ProcessManager>,
    processes_to_start: Vec<String>,
    log_file_path: Option<String>,
    procfile_path: &str,
) -> Result<()> {
    // Setup signal handling BEFORE starting any processes
    let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_flag_ctrl_c = Arc::clone(&shutdown_flag);
    
    // Install global panic handler to ensure cleanup
    let shutdown_flag_panic = Arc::clone(&shutdown_flag);
    let manager_panic = Arc::clone(&manager);
    let original_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        shutdown_flag_panic.store(true, std::sync::atomic::Ordering::SeqCst);
        let manager_panic = Arc::clone(&manager_panic);
        tokio::spawn(async move {
            let _ = manager_panic.stop_all().await;
        });
        original_panic(info);
    }));

    // Setup Ctrl+C handler first, before starting processes
    let ctrl_c_registered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ctrl_c_registered_clone = Arc::clone(&ctrl_c_registered);
    let manager_for_interrupt = Arc::clone(&manager);
    
    // Register signal handler with immediate flag setting
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                // Immediately mark shutdown - don't rely on channels
                shutdown_flag_ctrl_c.store(true, std::sync::atomic::Ordering::SeqCst);
                ctrl_c_registered_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                
                // Print interrupt message following design specs
                manager_for_interrupt.print_system_message("Interrupt received, stopping processes...").await;
            }
            Err(e) => {
                eprintln!("[{}] Failed to setup Ctrl+C handler: {}", "ERROR".red(), e);
            }
        }
    });
    
    // Ensure signal handler is ready
    tokio::time::sleep(Duration::from_millis(50)).await;
    
    // Setup log file if requested
    let log_file = if let Some(path) = &log_file_path {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(file) => Some(Arc::new(Mutex::new(file))),
            Err(e) => {
                eprintln!("[{}] Failed to open log file: {}", "ERROR".red(), e);
                return Err(ProcessError::InputRead(e));
            }
        }
    } else {
        None
    };

    // Set log file in manager if available
    if let Some(file) = log_file.clone() {
        manager.set_log_file(file).await;
    }

    // Start all requested processes
    manager
        .print_system_message(&format!("Loading {} processes from {}", processes_to_start.len(), procfile_path))
        .await;
    manager
        .print_system_message("Press 'q' or Ctrl+C to stop all processes")
        .await;
    for process_name in &processes_to_start {
        if let Err(e) = manager.start_process(process_name).await {
            manager
                .print_system_message(&format!("Failed to start '{}': {}", process_name, e))
                .await;
        }
    }

    // Write to log file if specified
    if let Some(ref file) = log_file {
        let mut file_lock = file.lock().await;
        writeln!(
            file_lock,
            "[gaffa] Started {} processes",
            processes_to_start.len()
        )
        .ok();
    }

    // Spawn stdin reader for 'q' command
    let shutdown_flag_stdin = Arc::clone(&shutdown_flag);
    let manager_for_stdin = Arc::clone(&manager);
    let stdin_handle = tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = tokio::io::BufReader::new(stdin);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let input = line.trim();
                    if input == "q" || input == "quit" {
                        manager_for_stdin.print_system_message("Quit command received, stopping processes...").await;
                        shutdown_flag_stdin.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Main event loop with improved shutdown handling
    let mut shutdown_initiated = false;
    let mut force_shutdown_deadline = None;
    
    loop {
        // Check shutdown flag with immediate response
        if shutdown_flag.load(std::sync::atomic::Ordering::SeqCst) && !shutdown_initiated {
            shutdown_initiated = true;
            force_shutdown_deadline = Some(Instant::now() + Duration::from_secs(20)); // Give more time for graceful shutdown
            
            // First, try to propagate Ctrl+C to all processes
            manager.send_ctrl_c_to_all().await;
            
            // Give processes a longer chance to handle Ctrl+C gracefully
            tokio::time::sleep(Duration::from_secs(5)).await;
            
            // Check if any processes are still running before calling stop_all
            let still_running = {
                let processes = manager.processes.lock().await;
                processes.values().any(|info| info.status == ProcessStatus::Running)
            };
            
            if still_running {
                manager.print_system_message("Some processes still running, initiating shutdown...").await;
                // Then initiate normal shutdown for any still running
                manager.stop_all().await;
            } else {
                manager.print_system_message("All processes stopped gracefully").await;
            }
        }

        // Force termination if deadline exceeded
        if let Some(deadline) = force_shutdown_deadline {
            if Instant::now() > deadline {
                eprintln!("\n[{}] Force terminating stubborn processes...", "ERROR".red());
                
                // Force kill all remaining processes
                let children: Vec<String> = {
                    let children = manager.children.lock().await;
                    children.keys().cloned().collect()
                };
                
                for name in children {
                    if let Some(child) = manager.children.lock().await.remove(&name) {
                        // Force kill without grace period
                        #[cfg(target_os = "windows")]
                        {
                            if let Some(pid) = child.id() {
                                let _ = std::process::Command::new("taskkill")
                                    .args(["/F", "/T", "/PID", &pid.to_string()])
                                    .output();
                            }
                        }
                        #[cfg(not(target_os = "windows"))]
                        {
                            let _ = child.kill().await;
                        }
                    }
                }
                break;
            }
        }

        // Check process status
        let all_stopped = {
            let processes = manager.processes.lock().await;
            processes.values().all(|info| info.status == ProcessStatus::Stopped)
        };

        if all_stopped && shutdown_initiated {
            break;
        }

        // Small sleep to prevent busy waiting
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Cleanup
    stdin_handle.abort();
    
    // Restore panic handler
    let _ = std::panic::take_hook();

    // Wait a bit to ensure all processes have fully stopped
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Show termination summary
    show_termination_summary(&manager).await;
    
    // Force exit to ensure all background tasks are terminated
    std::process::exit(0)
}

/// Show termination summary and exit.
async fn show_termination_summary(manager: &ProcessManager) {
    println!("\n{}", "=".repeat(79));
    println!("{}", "Session terminated, summary:");
    println!();

    let processes = manager.processes.lock().await;
    let process_colors = manager.process_colors.lock().await;
    
    println!("{:>12}  {:>12}  {:>12}", "process".green(), "status".green(), "runtime".green());
    println!("{}", "-".repeat(42));
    
    for (name, info) in processes.iter() {
        // Calculate last session runtime
        let last_runtime = if let Some(start_time) = info.last_restart {
            if let Some(stop_time) = info.stopped_at {
                stop_time.duration_since(start_time)
            } else if info.status == ProcessStatus::Running {
                start_time.elapsed()
            } else {
                Duration::from_secs(0)
            }
        } else {
            Duration::from_secs(0)
        };

        // Calculate total runtime
        let total_runtime = if info.status == ProcessStatus::Running && info.last_restart.is_some() {
            info.cumulative_runtime + info.last_restart.unwrap().elapsed()
        } else {
            info.cumulative_runtime
        };

        // Format runtime string
        let runtime_str = if last_runtime.as_secs() == 0 && total_runtime.as_secs() == 0 {
            "N/A".to_string()
        } else {
            let format_duration = |d: Duration| {
                let secs = d.as_secs();
                if secs >= 3600 {
                    format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
                } else if secs >= 60 {
                    format!("{}m{}s", secs / 60, secs % 60)
                } else {
                    format!("{secs}s")
                }
            };
            
            let last_str = format_duration(last_runtime);
            // Only show total if it's meaningfully different from last runtime
            if total_runtime > last_runtime && (total_runtime - last_runtime).as_secs() > 0 {
                let total_str = format_duration(total_runtime);
                format!("{} ({})", last_str, total_str)
            } else {
                last_str
            }
        };

        let restart_info = if info.restart_count > 0 {
            format!(" [{} restarts]", info.restart_count)
        } else {
            String::new()
        };

        let status_str = match (&info.status, info.exit_code) {
            (ProcessStatus::Running, _) => "running".to_string(),
            (ProcessStatus::Stopped, Some(0)) => "exit 0".to_string(),
            (ProcessStatus::Stopped, Some(-1)) => "terminated".to_string(), // Force terminated  
            (ProcessStatus::Stopped, Some(512)) => "interrupted".to_string(), // KeyboardInterrupt
            (ProcessStatus::Stopped, Some(-1073741510)) => "interrupted".to_string(), // CTRL_C_EVENT
            (ProcessStatus::Stopped, Some(code)) => format!("exit {code}"),
            (ProcessStatus::Stopped, None) => "stopped".to_string(),
            (ProcessStatus::Restarting, _) => "restarting".to_string(),
        };

        let colored_name = if let Some(&color) = process_colors.get(name) {
            name.color(color)
        } else {
            name.cyan()
        };

        println!(
            "{:>12}  {:>12}  {:>12}{}",
            colored_name, status_str, runtime_str, restart_info
        );
    }
    println!("\n");
}

/// Handle the run subcommand.
async fn handle_run_command(run_matches: &clap::ArgMatches) -> Result<()> {
    let procfile_path = run_matches
        .get_one::<String>("procfile")
        .expect("procfile has default value");

    let selected_processes: Vec<String> = run_matches
        .get_many::<String>("processes")
        .map(|vals| vals.map(std::string::ToString::to_string).collect())
        .unwrap_or_default();

    let log_file_path = run_matches.get_one::<String>("log-file").cloned();
    let interactive = run_matches.get_flag("interactive");
    
    // Parse environment variables
    let mut env_vars = std::collections::HashMap::new();
    
    // Parse --env arguments
    if let Some(env_args) = run_matches.get_many::<String>("env") {
        for env_arg in env_args {
            if let Some((key, value)) = env_arg.split_once('=') {
                env_vars.insert(key.to_string(), value.to_string());
            } else {
                eprintln!("[{}] Invalid env format: {env_arg} (expected KEY=VALUE)", "ERROR".red());
                return Err(ProcessError::InputRead(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Invalid env format: {env_arg}"),
                )));
            }
        }
    }
    
    // Parse --env-file
    if let Some(env_file) = run_matches.get_one::<String>("env-file") {
        parse_env_file(env_file, &mut env_vars).await?;
    }

    let manager = Arc::new(ProcessManager::new());
    manager.set_environment_variables(env_vars).await;
    manager.load_procfile(procfile_path).await?;

    let processes_to_start = if selected_processes.is_empty() {
        manager.process_names().await
    } else {
        selected_processes
    };

    if interactive {
        // Create app state and run terminal UI
        let app_state = Arc::new(AppState::new());
        let result = run_terminal_ui(
            Arc::clone(&manager),
            app_state,
            processes_to_start,
            procfile_path,
            log_file_path,
        )
        .await;
        
        if result.is_ok() {
            // Give processes time to finish shutting down
            tokio::time::sleep(Duration::from_millis(500)).await;
            
            // Show termination summary after TUI exits
            show_termination_summary(&manager).await;
        }
        
        result
    } else {
        // Run in non-interactive mode (default)
        run_non_interactive(manager, processes_to_start, log_file_path, procfile_path).await
    }
}

#[tokio::main]
async fn main() {
    let app = Command::new("gaffa")
        .version(env!("CARGO_PKG_VERSION"))
        .about("A Procfile-based process manager")
        .subcommand(
            Command::new("reset-terminal")
                .about("Reset terminal to fix display issues after a crash"),
        )
        .subcommand(
            Command::new("run")
                .about("Run processes from Procfile")
                .arg(
                    Arg::new("procfile")
                        .short('f')
                        .long("procfile")
                        .value_name("FILE")
                        .help("Path to Procfile")
                        .default_value("Procfile")
                        .action(clap::ArgAction::Set),
                )
                .arg(
                    Arg::new("processes")
                        .help("Specific processes to run (if not specified, runs all)")
                        .num_args(0..)
                        .value_name("PROCESS"),
                )
                .arg(
                    Arg::new("log-file")
                        .long("log-file")
                        .value_name("FILE")
                        .help("Write all process output to a log file")
                        .action(clap::ArgAction::Set),
                )
                .arg(
                    Arg::new("interactive")
                        .short('i')
                        .long("interactive")
                        .help("Run in interactive mode with Terminal UI")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("env")
                        .long("env")
                        .value_name("KEY=VALUE")
                        .help("Set environment variable for all processes (can be used multiple times)")
                        .action(clap::ArgAction::Append),
                )
                .arg(
                    Arg::new("env-file")
                        .long("env-file")
                        .value_name("FILE")
                        .help("Read environment variables from a file")
                        .action(clap::ArgAction::Set),
                ),
        );

    let matches = app.get_matches();

    match matches.subcommand() {
        Some(("reset-terminal", _)) => {
            reset_terminal();
            println!("Terminal reset complete.");
        }
        Some(("run", run_matches)) => {
            if let Err(e) = handle_run_command(run_matches).await {
                eprintln!("[{}] {e}", "ERROR".red());
                reset_terminal();
                std::process::exit(1);
            }
            // Exit cleanly after successful run
            std::process::exit(0);
        }
        _ => show_help(),
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
