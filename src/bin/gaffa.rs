use clap::{Arg, Command};
use gaffa::output;
use gaffa::{LifecycleOptions, ProcessError, ProcessManager, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Reset terminal to normal state.
fn reset_terminal() {
    use std::io::Write;

    // Show cursor first
    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);

    // Disable raw mode
    let _ = crossterm::terminal::disable_raw_mode();

    // Leave alternate screen and cleanup
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
        crossterm::style::ResetColor
    );

    // Flush output
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    // Platform-specific reset
    #[cfg(unix)]
    {
        // Reset terminal to sane state
        let _ = std::process::Command::new("stty").arg("sane").status();
    }
}

/// Simple terminal cleanup for non-interactive mode
fn reset_terminal_simple() {
    use std::io::Write;

    // Re-enable virtual terminal processing on Windows so that ANSI codes
    // and \n → \r\n translation work correctly after Ctrl+C.
    #[cfg(windows)]
    {
        use winapi::shared::minwindef::DWORD;
        use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
        use winapi::um::processenv::GetStdHandle;
        use winapi::um::winbase::STD_OUTPUT_HANDLE;

        const ENABLE_PROCESSED_OUTPUT: DWORD = 0x0001;
        const ENABLE_WRAP_AT_EOL_OUTPUT: DWORD = 0x0002;
        const ENABLE_VIRTUAL_TERMINAL_PROCESSING: DWORD = 0x0004;

        unsafe {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode: DWORD = 0;
            if GetConsoleMode(handle, &mut mode) != 0 {
                mode |= ENABLE_PROCESSED_OUTPUT
                    | ENABLE_WRAP_AT_EOL_OUTPUT
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
                let _ = SetConsoleMode(handle, mode);
            }
        }
    }

    // Ensure cursor is visible and colors are reset
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::cursor::Show,
        crossterm::style::ResetColor
    );

    // Flush output
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}

fn show_help() {
    println!("gaffa run [OPTIONS] [PROCESS_NAMES]...");
    println!();
    println!("Run processes from a Procfile");
    println!();
    println!("Options:");
    println!("  -i, --interactive         Run in interactive mode with Terminal UI");
    println!("  -p, --procfile <FILE>     Path to Procfile (default: ./Procfile)");
    println!("  -l, --log-file <FILE>     Log output to file");
    println!("      --env <KEY=VALUE>     Set environment variable (can be used multiple times)");
    println!("      --env-file <FILE>     Read environment variables from file");
    println!();
    println!("Arguments:");
    println!("  [PROCESS_NAMES]...       Specific processes to run (runs all if omitted)");
    println!();
    println!("Examples:");
    println!("  gaffa run                    # Run all processes");
    println!("  gaffa run web worker         # Run only web and worker processes");
    println!("  gaffa run -i                 # Run in interactive mode");
    println!("  gaffa run --env PORT=8000    # Set environment variable");
}

async fn run_non_interactive(
    manager: Arc<ProcessManager>,
    processes_to_run: Option<Vec<String>>,
) -> Result<bool> {
    // Returns true if interrupted
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::signal;

    let interrupted = Arc::new(AtomicBool::new(false));

    // Get max name length for proper alignment
    let max_name_len = manager.get_max_name_length().await;

    // Show startup messages like legacy version
    let process_count = if let Some(ref names) = processes_to_run {
        names.len()
    } else {
        manager.process_names().await.len()
    };

    println!(
        "{}",
        output::format_gaffa_message(
            &format!("Loading {} processes from procfile", process_count),
            max_name_len
        )
    );
    println!(
        "{}",
        output::format_gaffa_message("Press 'q' or Ctrl+C to stop all processes", max_name_len)
    );

    // Spawn signal handler
    tokio::spawn({
        let manager = Arc::clone(&manager);
        let interrupted = Arc::clone(&interrupted);
        async move {
            // Re-fetch max_name_len inside the spawned task
            let _max_name_len = manager.get_max_name_length().await;

            #[cfg(unix)]
            {
                let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
                    .expect("Failed to set up SIGTERM handler");
                let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
                    .expect("Failed to set up SIGINT handler");

                tokio::select! {
                    _ = sigterm.recv() => {
                        eprintln!();
                    }
                    _ = sigint.recv() => {
                        eprintln!();
                    }
                }
            }

            #[cfg(windows)]
            {
                signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
                eprintln!();
            }

            interrupted.store(true, Ordering::SeqCst);
        }
    });

    // Start processes
    if let Some(names) = processes_to_run {
        for name in names {
            println!(
                "{}",
                output::format_gaffa_message(
                    &format!("Starting process '{}'...", name),
                    max_name_len
                )
            );
            // Ensure the message is flushed before starting the process
            use std::io::Write;
            let _ = std::io::stdout().flush();

            if let Err(e) = manager
                .start_process_with_opts(&name, &LifecycleOptions::quiet())
                .await
            {
                eprintln!(
                    "{}",
                    output::format_error_message(
                        &format!("Failed to start process '{}': {}", name, e),
                        max_name_len
                    )
                );
                return Err(e);
            }
        }
    } else {
        // Start all processes
        let process_names = manager.process_names().await;
        for name in process_names {
            println!(
                "{}",
                output::format_gaffa_message(
                    &format!("Starting process '{}'...", name),
                    max_name_len
                )
            );
            // Ensure the message is flushed before starting the process
            use std::io::Write;
            let _ = std::io::stdout().flush();

            if let Err(e) = manager
                .start_process_with_opts(&name, &LifecycleOptions::quiet())
                .await
            {
                eprintln!(
                    "{}",
                    output::format_error_message(
                        &format!("Failed to start process '{}': {}", name, e),
                        max_name_len
                    )
                );
                return Err(e);
            }
        }
    }

    // Wait for all processes to finish or for interrupt
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;

        if interrupted.load(Ordering::SeqCst) {
            break;
        }

        let all_stopped = manager.all_stopped().await;

        if all_stopped {
            break;
        }
    }

    let was_interrupted = interrupted.load(Ordering::SeqCst);

    if was_interrupted {
        // Reset terminal state FIRST — Ctrl+C on Windows corrupts the
        // console mode, causing raw ANSI codes and broken newlines in
        // any output that follows (including process shutdown messages).
        reset_terminal_simple();

        // Stop all child processes before printing the summary so their
        // output does not interleave with the termination report.
        manager.stop_all_with_opts(&LifecycleOptions::quiet()).await;

        // Fix status of processes that exited but weren't tracked
        manager.fix_orphaned_process_status().await;
    }

    // Wait for output handlers to fully drain
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Flush stdout so prior process output is complete before summary
    {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
    }

    // Reset again before summary in case process output corrupted state
    reset_terminal_simple();

    show_termination_summary(&manager, was_interrupted).await;

    if was_interrupted {
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::process::exit(130);
    }

    Ok(interrupted.load(Ordering::SeqCst))
}

/// Show termination summary and exit.
async fn show_termination_summary(manager: &ProcessManager, _was_interrupted: bool) {
    let max_name_len = manager.get_max_name_length().await;
    let snapshot = manager.process_snapshot().await;

    let table_width = max_name_len + 30;

    eprintln!();
    eprintln!("{}", "-".repeat(table_width));
    eprintln!("  Session terminated");
    eprintln!("{}", "-".repeat(table_width));

    // Simpler header format
    let header_padding = " ".repeat(max_name_len.saturating_sub(7));
    eprintln!("  process{}     status        runtime", header_padding);
    eprintln!("{}", "-".repeat(table_width));

    for (name, info, _color) in &snapshot {
        // Calculate runtime
        let runtime = if let Some(stopped_at) = info.stopped_at {
            if let Some(last_restart) = info.last_restart {
                stopped_at.duration_since(last_restart)
            } else {
                Duration::from_secs(0)
            }
        } else if let Some(last_restart) = info.last_restart {
            last_restart.elapsed()
        } else {
            Duration::from_secs(0)
        };

        // Format runtime string
        let runtime_str = if runtime.as_millis() == 0 {
            "N/A".to_string()
        } else if runtime.as_secs() >= 3600 {
            let h = runtime.as_secs() / 3600;
            let m = (runtime.as_secs() % 3600) / 60;
            let s = runtime.as_secs() % 60;
            format!("{h}h {m}m {s}s")
        } else if runtime.as_secs() >= 60 {
            let m = runtime.as_secs() / 60;
            let s = runtime.as_secs() % 60;
            format!("{m}m {s}s")
        } else if runtime.as_secs() >= 1 {
            format!("{}s", runtime.as_secs())
        } else {
            format!("{}ms", runtime.as_millis())
        };

        // Determine status based on exit code and status
        let status_str = match (&info.status, info.exit_code) {
            (gaffa::ProcessStatus::Running, _) => "running".to_string(),
            (gaffa::ProcessStatus::Stopped, Some(0)) => "exit 0".to_string(),
            (gaffa::ProcessStatus::Stopped, Some(-1)) => "terminated".to_string(),
            (gaffa::ProcessStatus::Stopped, Some(512)) => "interrupted".to_string(),
            (gaffa::ProcessStatus::Stopped, Some(-1073741510)) => "interrupted".to_string(), // Windows Ctrl+C
            (gaffa::ProcessStatus::Stopped, Some(code)) => format!("exit {}", code),
            (gaffa::ProcessStatus::Stopped, None) => "stopped".to_string(),
            _ => "unknown".to_string(),
        };

        // Format with proper alignment — plain text, no ANSI colors
        let name_padding = " ".repeat(max_name_len.saturating_sub(name.len()));
        eprintln!(
            "  {}{} {:>12} {:>12}",
            name, name_padding, status_str, runtime_str
        );
    }

    eprintln!("{}", "-".repeat(table_width));

    // Ensure output is flushed
    use std::io::Write;
    let _ = std::io::stderr().flush();
}

/// Install a synchronous console ctrl handler on Windows that resets the
/// console output mode the moment Ctrl+C is pressed — before any child
/// process or async handler writes garbled output.
#[cfg(windows)]
fn install_console_ctrl_handler() {
    use winapi::shared::minwindef::DWORD;
    use winapi::um::consoleapi::{GetConsoleMode, SetConsoleCtrlHandler, SetConsoleMode};
    use winapi::um::processenv::GetStdHandle;
    use winapi::um::winbase::STD_OUTPUT_HANDLE;

    const ENABLE_PROCESSED_OUTPUT: DWORD = 0x0001;
    const ENABLE_WRAP_AT_EOL_OUTPUT: DWORD = 0x0002;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: DWORD = 0x0004;

    unsafe extern "system" fn handler(_ctrl_type: DWORD) -> i32 {
        // Re-enable processed output and VT processing so ANSI codes
        // and \n→\r\n translation work correctly.
        unsafe {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode: DWORD = 0;
            if GetConsoleMode(handle, &mut mode) != 0 {
                mode |= ENABLE_PROCESSED_OUTPUT
                    | ENABLE_WRAP_AT_EOL_OUTPUT
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
                SetConsoleMode(handle, mode);
            }
        }
        // Return FALSE (0) so the default handler (which terminates) does NOT
        // run — tokio's signal handler will pick it up instead.
        0
    }

    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

async fn handle_run_command(run_matches: &clap::ArgMatches) -> Result<()> {
    #[cfg(windows)]
    install_console_ctrl_handler();

    let procfile_path = run_matches
        .get_one::<String>("procfile")
        .map(String::as_str)
        .unwrap_or("Procfile");

    let interactive = run_matches.get_flag("interactive");

    let log_file_path = run_matches.get_one::<String>("log-file");

    let processes_to_run: Option<Vec<String>> = run_matches
        .get_many::<String>("processes")
        .map(|vals| vals.cloned().collect());

    // Parse environment variables
    let mut env_vars: HashMap<String, String> = HashMap::new();

    // From --env flags
    if let Some(env_args) = run_matches.get_many::<String>("env") {
        for env_arg in env_args {
            if let Some((key, value)) = env_arg.split_once('=') {
                env_vars.insert(key.to_string(), value.to_string());
            } else {
                // We don't have max_name_len yet, so use default formatting
                eprintln!(
                    "{}",
                    output::format_error_message(
                        &format!("Invalid environment variable format: {}", env_arg),
                        0,
                    )
                );
                eprintln!(
                    "{}",
                    output::format_error_message("Expected format: KEY=VALUE", 0)
                );
                return Err(ProcessError::InvalidFormat {
                    line: env_arg.clone(),
                });
            }
        }
    }

    // From --env-file
    if let Some(env_file) = run_matches.get_one::<String>("env-file") {
        gaffa::parse_env_file(env_file, &mut env_vars).await?;
    }

    let manager = Arc::new(ProcessManager::new());

    // Set environment variables
    if !env_vars.is_empty() {
        manager.set_environment_variables(env_vars).await;
    }

    // Set up log file if specified
    if let Some(path) = log_file_path {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| ProcessError::ProcfileRead {
                path: path.clone(),
                source: e,
            })?;
        manager.set_log_file(Arc::new(Mutex::new(file))).await;
    }

    manager.load_procfile(procfile_path).await?;

    // Filter processes if specific ones were requested
    if let Some(ref names) = processes_to_run {
        // Check if all requested processes exist before filtering
        let current_names = manager.process_names().await;
        for name in names {
            if !current_names.contains(name) {
                return Err(ProcessError::ProcessNotFound { name: name.clone() });
            }
        }
        manager.retain_processes(names).await;
    }

    if interactive {
        match manager.run_interactive().await {
            Ok(()) => {
                // The UI already cleaned up the terminal, so we don't need reset_terminal()
                // Add a longer delay to ensure terminal is fully restored and ready for output
                std::thread::sleep(std::time::Duration::from_millis(200));

                // Force terminal into a good state for output
                let _ = crossterm::execute!(
                    std::io::stderr(),
                    crossterm::cursor::Show,
                    crossterm::style::ResetColor
                );

                // Show termination summary
                show_termination_summary(&manager, false).await;
            }
            Err(e) => {
                // In case of error, we should reset the terminal
                reset_terminal();
                eprintln!("{}", output::format_error_message(&e.to_string(), 0));
                return Err(e);
            }
        }
    } else {
        match run_non_interactive(manager.clone(), processes_to_run).await {
            Ok(_) => {
                reset_terminal_simple();
            }
            Err(e) => {
                // Reset terminal on error
                reset_terminal();
                eprintln!("{}", output::format_error_message(&e.to_string(), 0));
                return Err(e);
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    // Set up a panic handler to ensure terminal is reset
    let original_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        reset_terminal();
        original_panic(info);
    }));

    let app = Command::new("gaffa")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Cross-platform process manager for Procfile-based applications")
        .arg_required_else_help(true)
        .subcommand(
            Command::new("reset-terminal")
                .about("Reset terminal to normal state (use if terminal is messed up)"),
        )
        .subcommand(
            Command::new("run")
                .about("Run processes from a Procfile")
                .arg(
                    Arg::new("procfile")
                        .short('p')
                        .long("procfile")
                        .value_name("FILE")
                        .help("Path to Procfile")
                        .default_value("Procfile"),
                )
                .arg(
                    Arg::new("processes")
                        .value_name("PROCESS_NAMES")
                        .help("Specific processes to run (runs all if omitted)")
                        .num_args(0..),
                )
                .arg(
                    Arg::new("log-file")
                        .short('l')
                        .long("log-file")
                        .value_name("FILE")
                        .help("Log output to file"),
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
                eprintln!("{}", output::format_error_message(&e.to_string(), 0));
                reset_terminal();
                std::process::exit(1);
            }
            // Exit cleanly after successful run
            std::process::exit(0);
        }
        _ => show_help(),
    }
}
