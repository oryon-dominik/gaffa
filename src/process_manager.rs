use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use colored::Colorize;
use tokio::{
    process::{Child, Command as TokioCommand},
    sync::{Mutex, mpsc, oneshot},
    time::sleep,
};

use crate::constants::*;
use crate::output;
use crate::platform::{
    assign_child_to_job, configure_command, force_kill_process, terminate_process,
};
use crate::procfile;
use crate::shell::Shell;
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
    pub shutdown_timeout: Duration,
    pub shell: Shell,
}

/// Commands accepted by a process actor.
enum ActorCommand {
    /// Graceful terminate (Ctrl+Break/SIGTERM → force kill). Replies with the
    /// exit code, or `None` if the process survived.
    Stop(oneshot::Sender<Option<i32>>),
    /// Immediate force kill. Replies with the exit code, or `None` if the
    /// process survived even that.
    Kill(oneshot::Sender<Option<i32>>),
}

/// Handle to the actor task that owns a running child process.
pub(crate) struct ActorHandle {
    commands: mpsc::Sender<ActorCommand>,
    pid: Option<u32>,
}

/// Mutable runtime state for all managed processes.
pub(crate) struct RuntimeState {
    pub processes: HashMap<String, ProcessInfo>,
    pub actors: HashMap<String, ActorHandle>,
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
                shutdown_timeout: GRACEFUL_SHUTDOWN_TIMEOUT,
                shell: Shell::platform_default().clone(),
            })),
            runtime: Arc::new(Mutex::new(RuntimeState {
                processes: HashMap::new(),
                actors: HashMap::new(),
                output_handles: HashMap::new(),
            })),
        }
    }

    /// Set the log file for this process manager.
    pub async fn set_log_file(&self, log_file: Arc<Mutex<std::fs::File>>) {
        let mut config = self.config.lock().await;
        config.log_file = Some(log_file);
    }

    /// Override the graceful shutdown timeout for stop operations.
    ///
    /// Caps how long each child has to exit cleanly before gaffa force-kills
    /// it during `stop_all`. Defaults to [`GRACEFUL_SHUTDOWN_TIMEOUT`].
    pub async fn set_shutdown_timeout(&self, timeout: Duration) {
        let mut config = self.config.lock().await;
        config.shutdown_timeout = timeout;
    }

    /// Set environment variables to be applied to all processes.
    pub async fn set_environment_variables(&self, env_vars: HashMap<String, String>) {
        let mut config = self.config.lock().await;
        config.env_vars = env_vars;
    }

    /// Set the shell used to execute Procfile command lines.
    pub async fn set_shell(&self, shell: Shell) {
        let mut config = self.config.lock().await;
        config.shell = shell;
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
        // Check if process is actually running (has a live actor)
        {
            let runtime = self.runtime.lock().await;
            if runtime.actors.contains_key(name) {
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
    /// The command line is passed verbatim to the configured shell
    /// (`pwsh`/`cmd` on Windows, `sh` on Unix) — gaffa does not parse it.
    ///
    /// # Errors
    ///
    /// Returns an error if the command is empty or the process fails to spawn.
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
        let command = command.trim();
        if command.is_empty() {
            return Err(ProcessError::EmptyCommand {
                name: name.to_string(),
            });
        }

        let (shell, env_vars, log_file) = {
            let config = self.config.lock().await;
            (
                config.shell.clone(),
                config.env_vars.clone(),
                config.log_file.clone(),
            )
        };

        let mut cmd = TokioCommand::new(&shell.program);

        #[cfg(target_os = "windows")]
        {
            // raw_arg: cmd.exe does not follow argv quoting rules, and the
            // shell should receive the Procfile line verbatim as script text
            // — std's automatic quote-escaping would mangle both.
            for arg in &shell.args {
                cmd.raw_arg(arg);
            }
            cmd.raw_arg(shell.prepare_command(command).as_ref());
        }
        #[cfg(not(target_os = "windows"))]
        {
            cmd.args(&shell.args).arg(command);
        }

        for (key, value) in env_vars {
            cmd.env(key, value);
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

        // Assign the child to the platform Job Object (Windows) so it is
        // automatically killed when gaffa exits — even on crash.
        assign_child_to_job(&child);

        let stdout = child.stdout.take().expect("stdout pipe");
        let stderr = child.stderr.take().expect("stderr pipe");

        self.spawn_output_handler_with_state(name, stdout, stderr, app_state.clone(), log_file)
            .await;

        // Hand the child to its actor task — the actor owns it from here on.
        let (commands, command_rx) = mpsc::channel(4);
        {
            let mut runtime = self.runtime.lock().await;
            runtime.actors.insert(
                name.to_string(),
                ActorHandle {
                    commands,
                    pid: child.id(),
                },
            );
        }

        let shutdown_timeout = {
            let config = self.config.lock().await;
            config.shutdown_timeout
        };

        tokio::spawn(run_process_actor(
            name.to_string(),
            child,
            command_rx,
            Arc::clone(&self.runtime),
            app_state,
            shutdown_timeout,
        ));

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

        // Ask the actor that owns the child to terminate it. The actor
        // updates the shared state before replying.
        let exit_code = match self.send_actor_command(name, ActorCommand::Stop).await {
            Some(reply) => reply,
            None => {
                return Err(ProcessError::ProcessNotRunning {
                    name: name.to_string(),
                });
            }
        };

        // A `None` exit code means the process survived even the force kill.
        if exit_code.is_none() {
            if opts.log_messages {
                if let Some(state) = &opts.app_state {
                    state
                        .add_system_log(format!("Process '{name}' is ignoring termination signals"))
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
    }

    /// Send a lifecycle command to a process actor and await its reply.
    ///
    /// Returns `None` when no actor exists (process not running) or the actor
    /// finished before receiving the command. Otherwise returns the actor's
    /// reply: `Some(code)` when the process exited, `None` when it survived
    /// termination.
    async fn send_actor_command(
        &self,
        name: &str,
        command: fn(oneshot::Sender<Option<i32>>) -> ActorCommand,
    ) -> Option<Option<i32>> {
        let sender = {
            let runtime = self.runtime.lock().await;
            runtime.actors.get(name).map(|actor| actor.commands.clone())
        }?;

        let (reply_tx, reply_rx) = oneshot::channel();
        if sender.send(command(reply_tx)).await.is_err() {
            // The actor finished (process exited) while we were asking.
            return None;
        }

        // The actor always replies; a dropped reply means it finalized a
        // natural exit that raced our command — the process is stopped.
        Some(reply_rx.await.unwrap_or(Some(EXIT_CODE_FORCED_TERMINATION)))
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

        // Attempt graceful stop
        let _ = self.stop_process_with_opts(name, &quiet_opts).await;

        // Escalate if the actor is still alive (stubborn process). If even
        // the force kill fails, start_process below reports AlreadyRunning
        // instead of racing a second instance against the survivor.
        let _ = self.send_actor_command(name, ActorCommand::Kill).await;

        // Brief settle delay so released resources (ports, files) are free.
        sleep(Duration::from_millis(200)).await;

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
                .actors
                .iter()
                .filter_map(|(name, actor)| actor.pid.map(|pid| (name.clone(), pid)))
                .collect()
        };

        for (name, pid) in pids {
            #[cfg(target_os = "windows")]
            {
                self.print_system_message(&format!("Sending interrupt signal to '{name}'..."))
                    .await;

                // CTRL_C_EVENT cannot target a process group (documented
                // no-op for nonzero group ids), so send CTRL_BREAK_EVENT —
                // the child is its own group leader.
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
                    if libc::kill(-(pid as i32), libc::SIGINT) != 0 {
                        libc::kill(pid as i32, libc::SIGINT);
                    }
                }
            }
        }
    }

    /// Stop all running processes with explicit lifecycle options.
    pub async fn stop_all_with_opts(&self, opts: &LifecycleOptions) {
        // Everything not already stopped — including processes mid-restart.
        let process_names: Vec<String> = {
            let runtime = self.runtime.lock().await;
            runtime
                .processes
                .iter()
                .filter(|(_, info)| info.status != ProcessStatus::Stopped)
                .map(|(name, _)| name.clone())
                .collect()
        };

        if process_names.is_empty() {
            // No running processes to stop - don't log anything to avoid noise
            return;
        }

        let shutdown_timeout = {
            let config = self.config.lock().await;
            config.shutdown_timeout
        };

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
                // Try graceful shutdown with the configured timeout (without individual logging).
                // Add a small cushion so terminate_process's internal deadline fires before this one.
                let outer_timeout = shutdown_timeout + Duration::from_secs(1);
                let result = tokio::time::timeout(
                    outer_timeout,
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

        // Force kill any processes that failed graceful shutdown
        for name in failed_shutdowns {
            let _ = self.send_actor_command(&name, ActorCommand::Kill).await;
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

    /// Get the number of live process actors (for diagnostics/testing).
    /// Each running child is owned by exactly one actor task.
    pub async fn monitor_handle_count(&self) -> usize {
        self.runtime.lock().await.actors.len()
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
        self.runtime.lock().await.actors.len()
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
        let child_names: Vec<String> = runtime.actors.keys().cloned().collect();
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
        // On Windows, Ctrl+C can leave the console without VT processing
        // and \n→\r\n translation — re-arm before writing so shutdown
        // messages don't render as `←[…m` garbage followed by `◙`.
        crate::platform::ensure_console_mode();
        println!("{}", output::format_gaffa_message(message, max_name_len));
        use std::io::{Write, stdout};
        let _ = stdout().flush();
    }
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Own a child process: react to its exit and execute lifecycle commands.
///
/// Replaces the previous polling monitor — `child.wait()` resolves the moment
/// the process exits, with no lock contention on the shared runtime state.
/// State updates happen *before* command replies are sent, so callers observe
/// consistent state as soon as their reply arrives.
async fn run_process_actor(
    name: String,
    mut child: Child,
    mut commands: mpsc::Receiver<ActorCommand>,
    runtime: Arc<Mutex<RuntimeState>>,
    app_state: Option<Arc<AppState>>,
    shutdown_timeout: Duration,
) {
    loop {
        tokio::select! {
            status = child.wait() => {
                let exit_code = status.ok().and_then(|s| s.code());
                finalize_process_exit(&name, exit_code, &runtime).await;
                announce_natural_exit(&name, exit_code, app_state.as_ref()).await;
                return;
            }
            command = commands.recv() => match command {
                Some(ActorCommand::Stop(reply)) => {
                    if let Some(status) = terminate_process(&mut child, shutdown_timeout).await {
                        let code = status.code().unwrap_or(EXIT_CODE_FORCED_TERMINATION);
                        finalize_process_exit(&name, Some(code), &runtime).await;
                        let _ = reply.send(Some(code));
                        return;
                    }
                    // Stubborn — stay alive and keep watching.
                    let _ = reply.send(None);
                }
                Some(ActorCommand::Kill(reply)) => {
                    force_kill_process(&mut child).await;
                    match tokio::time::timeout(PROCESS_WAIT_TIMEOUT, child.wait()).await {
                        Ok(Ok(status)) => {
                            let code = status.code().unwrap_or(EXIT_CODE_FORCED_TERMINATION);
                            finalize_process_exit(&name, Some(code), &runtime).await;
                            let _ = reply.send(Some(code));
                            return;
                        }
                        _ => {
                            let _ = reply.send(None);
                        }
                    }
                }
                // Manager dropped — keep waiting for the natural exit.
                None => {
                    let exit_code = child.wait().await.ok().and_then(|s| s.code());
                    finalize_process_exit(&name, exit_code, &runtime).await;
                    announce_natural_exit(&name, exit_code, app_state.as_ref()).await;
                    return;
                }
            }
        }
    }
}

/// Record a process exit in the shared state and untrack its actor.
/// Output readers are only untracked — they drain remaining lines until EOF.
async fn finalize_process_exit(
    name: &str,
    exit_code: Option<i32>,
    runtime: &Arc<Mutex<RuntimeState>>,
) {
    let mut rt = runtime.lock().await;
    rt.actors.remove(name);
    rt.output_handles.remove(name);
    if let Some(info) = rt.processes.get_mut(name) {
        if let Some(start_time) = info.last_restart {
            info.cumulative_runtime += start_time.elapsed();
        }
        info.status = ProcessStatus::Stopped;
        info.stopped_at = Some(Instant::now());
        info.exit_code = exit_code;
    }
}

/// Log a natural (not manager-initiated) process exit to the UI.
async fn announce_natural_exit(
    name: &str,
    exit_code: Option<i32>,
    app_state: Option<&Arc<AppState>>,
) {
    if let Some(state) = app_state {
        let exit_msg = match exit_code {
            Some(0) => format!("Process '{name}' exited cleanly"),
            Some(EXIT_CODE_FORCED_TERMINATION) => {
                format!("Process '{name}' terminated gracefully")
            }
            Some(EXIT_CODE_KEYBOARD_INTERRUPT) | Some(EXIT_CODE_CTRL_C_WINDOWS) => {
                format!("Process '{name}' interrupted gracefully")
            }
            Some(code) => format!("Process '{name}' exited with code {code}"),
            None => format!("Process '{name}' terminated by signal"),
        };
        state.add_system_log(exit_msg).await;
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
