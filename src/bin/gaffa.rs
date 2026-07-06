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
    println!("      --shutdown-timeout <SECONDS>");
    println!("                            Grace period before force-kill (default: 10)");
    println!("      --shell <PROGRAM>     Shell used to run commands (default: pwsh or cmd on");
    println!("                            Windows, sh on Unix; env override: GAFFA_SHELL)");
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

/// Rotate a log file path for a new session: `path/to/gaffa.log` becomes
/// `path/to/gaffa-YYYY-MM-DD_NNN.log` where NNN is the next unused 3-digit
/// sequence for that date. Previous sessions stay on disk as backups so
/// you can diff one run against another.
fn rotate_log_path(path: &str) -> std::path::PathBuf {
    use std::path::{Path, PathBuf};

    let orig = Path::new(path);
    let dir: PathBuf = orig
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = orig
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("gaffa");
    let ext = orig.extension().and_then(|s| s.to_str());

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();

    for n in 1..=999u32 {
        let name = match ext {
            Some(e) => format!("{stem}-{date}_{n:03}.{e}"),
            None => format!("{stem}-{date}_{n:03}"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    // Pathological case: 999 sessions on the same day already on disk.
    // Overwrite the last slot rather than failing outright.
    let name = match ext {
        Some(e) => format!("{stem}-{date}_999.{e}"),
        None => format!("{stem}-{date}_999"),
    };
    dir.join(name)
}

/// Render the boxed startup banner. Falls back to a compact prefixed
/// announcement when the terminal is too narrow or stdout is not a TTY,
/// so piped output stays greppable.
fn print_startup_banner(
    procfile_path: &str,
    process_names: &[String],
    log_path_requested: Option<&str>,
    log_path_rotated: Option<&std::path::Path>,
) {
    use colored::Colorize;

    let version = env!("CARGO_PKG_VERSION");
    let title = format!("gaffa {version}");
    let procs_joined = process_names.join(", ");
    let procs_value = format!("{} · {procs_joined}", process_names.len());

    let mut rows: Vec<(&str, String)> = vec![
        ("procfile", procfile_path.to_string()),
        ("processes", procs_value.clone()),
    ];
    if let Some(p) = log_path_requested {
        rows.push(("logfile", p.to_string()));
    }
    rows.push(("controls", "q or Ctrl+C to stop".to_string()));

    let label_w = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
    let value_w = rows
        .iter()
        .map(|(_, v)| v.chars().count())
        .max()
        .unwrap_or(0)
        .max(title.chars().count());
    let inner_w = label_w + 2 + value_w;

    let term_w = crossterm::terminal::size()
        .ok()
        .map(|(w, _)| w as usize)
        .unwrap_or(120);

    // Need the box plus two leading spaces and the two │ edges — 6 cols.
    if inner_w + 6 > term_w {
        // Compact fallback: plain prefixed lines, always copy-safe.
        println!(
            "{}",
            output::format_gaffa_message(
                &format!("{title} · {procs_value}"),
                5
            )
        );
        if let Some(p) = log_path_requested {
            println!(
                "{}",
                output::format_gaffa_message(&format!("logging to {p}"), 5)
            );
        }
        println!(
            "{}",
            output::format_gaffa_message("q or Ctrl+C to stop", 5)
        );
        announce_log_rotation(log_path_requested, log_path_rotated);
        return;
    }

    let seg = "─".repeat(inner_w + 2);
    let top = format!("╭{seg}╮");
    let mid = format!("├{seg}┤");
    let bot = format!("╰{seg}╯");
    let v = "│".bright_black();

    println!();
    println!("  {}", top.bright_black());
    let title_padded = format!("{title:<inner_w$}");
    println!("  {v} {} {v}", title_padded.magenta().bold());
    println!("  {}", mid.bright_black());
    for (label, value) in &rows {
        let label_cell = format!("{label:<label_w$}").bright_black();
        let value_cell = format!("{value:<value_w$}");
        println!("  {v} {label_cell}  {value_cell} {v}");
    }
    println!("  {}", bot.bright_black());
    println!();

    announce_log_rotation(log_path_requested, log_path_rotated);
}

/// Print a one-line `gaffa │ rotated <requested> → <rotated>` notice when
/// the actual on-disk file differs from the path the user passed with
/// `--log-file`. Keeps the banner header clean while making the rotation
/// observable — grep-friendly and copy-safe.
fn announce_log_rotation(
    requested: Option<&str>,
    rotated: Option<&std::path::Path>,
) {
    let (Some(req), Some(rot)) = (requested, rotated) else {
        return;
    };
    let rot_str = rot.display().to_string();
    // Compare by filename when the rotated path just adds parent dirs; a
    // true "same path" means rotation was a no-op and there is nothing
    // to announce.
    if rot_str == req {
        return;
    }
    println!(
        "{}",
        output::format_gaffa_message(&format!("log rotated → {rot_str}"), 5)
    );
}

async fn run_non_interactive(
    manager: Arc<ProcessManager>,
    processes_to_run: Option<Vec<String>>,
    procfile_path: &str,
    log_path_requested: Option<&str>,
    log_path_rotated: Option<&std::path::Path>,
) -> Result<bool> {
    // Returns true if interrupted
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::signal;

    let interrupted = Arc::new(AtomicBool::new(false));

    // Get max name length for proper alignment
    let max_name_len = manager.get_max_name_length().await;

    // Collect the set of processes we're about to start so we can announce
    // them in a single compact banner instead of N identical "Starting" lines.
    let startup_names: Vec<String> = match &processes_to_run {
        Some(names) => names.clone(),
        None => manager.process_names().await,
    };

    print_startup_banner(
        procfile_path,
        &startup_names,
        log_path_requested,
        log_path_rotated,
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

    // Start processes. We announced the full set in a single banner above,
    // so here we only surface failures — success is implied by subsequent
    // child output.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    for name in &startup_names {
        if let Err(e) = manager
            .start_process_with_opts(name, &LifecycleOptions::quiet())
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
    use colored::Colorize;

    let snapshot = manager.process_snapshot().await;

    // Build rows up front so we can compute column widths from real content.
    struct Row {
        name: String,
        color: colored::Color,
        status: String,
        runtime: String,
    }

    let rows: Vec<Row> = snapshot
        .iter()
        .map(|(name, info, color)| {
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

            let status_str = match (&info.status, info.exit_code) {
                (gaffa::ProcessStatus::Running, _) => "running".to_string(),
                (gaffa::ProcessStatus::Stopped, Some(0)) => "exit 0".to_string(),
                (gaffa::ProcessStatus::Stopped, Some(-1)) => "terminated".to_string(),
                (gaffa::ProcessStatus::Stopped, Some(512)) => "interrupted".to_string(),
                (gaffa::ProcessStatus::Stopped, Some(-1073741510)) => {
                    "interrupted".to_string()
                }
                (gaffa::ProcessStatus::Stopped, Some(code)) => format!("exit {code}"),
                (gaffa::ProcessStatus::Stopped, None) => "stopped".to_string(),
                _ => "unknown".to_string(),
            };

            Row {
                name: name.clone(),
                color: *color,
                status: status_str,
                runtime: runtime_str,
            }
        })
        .collect();

    const H_NAME: &str = "process";
    const H_STATUS: &str = "status";
    const H_RUNTIME: &str = "runtime";

    let name_w = rows
        .iter()
        .map(|r| r.name.len())
        .max()
        .unwrap_or(0)
        .max(H_NAME.len());
    let status_w = rows
        .iter()
        .map(|r| r.status.len())
        .max()
        .unwrap_or(0)
        .max(H_STATUS.len());
    let runtime_w = rows
        .iter()
        .map(|r| r.runtime.len())
        .max()
        .unwrap_or(0)
        .max(H_RUNTIME.len());

    // Box-drawing helpers. Each cell has 1 space of left/right padding, so
    // horizontal segments are `width + 2` wide.
    let seg = |w: usize| "─".repeat(w + 2);
    let top = format!("╭{}┬{}┬{}╮", seg(name_w), seg(status_w), seg(runtime_w));
    let mid = format!("├{}┼{}┼{}┤", seg(name_w), seg(status_w), seg(runtime_w));
    let bot = format!("╰{}┴{}┴{}╯", seg(name_w), seg(status_w), seg(runtime_w));
    let v = "│".bright_black();

    let colorize_status = |s: &str| -> colored::ColoredString {
        if s == "exit 0" {
            s.green()
        } else if s == "interrupted" {
            s.yellow()
        } else if s == "running" {
            s.cyan()
        } else {
            s.red()
        }
    };

    eprintln!();
    eprintln!("  {}", "Session terminated".bold());
    eprintln!();
    eprintln!("  {}", top.bright_black());
    eprintln!(
        "  {v} {:<name_w$} {v} {:<status_w$} {v} {:<runtime_w$} {v}",
        H_NAME.bold(),
        H_STATUS.bold(),
        H_RUNTIME.bold(),
    );
    eprintln!("  {}", mid.bright_black());

    for row in &rows {
        let name_cell = format!("{:<name_w$}", row.name).color(row.color);
        let status_cell = colorize_status(&format!("{:<status_w$}", row.status));
        let runtime_cell = format!("{:<runtime_w$}", row.runtime).dimmed();
        eprintln!("  {v} {name_cell} {v} {status_cell} {v} {runtime_cell} {v}");
    }

    eprintln!("  {}", bot.bright_black());

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

/// Ensure the parent directory of a log file exists, prompting the user
/// to create it if it does not.
fn ensure_log_parent_dir(path: &str) -> Result<()> {
    use std::io::{BufRead, Write};

    let parent = match std::path::Path::new(path).parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => return Ok(()),
    };

    if parent.exists() {
        return Ok(());
    }

    eprint!(
        "Log file directory '{}' does not exist. Create it? [y/N]: ",
        parent.display()
    );
    let _ = std::io::stderr().flush();

    let mut answer = String::new();
    let stdin = std::io::stdin();
    let _ = stdin.lock().read_line(&mut answer);
    let yes = matches!(answer.trim(), "y" | "Y" | "yes" | "YES");

    if !yes {
        return Err(ProcessError::ProcfileRead {
            path: path.to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("parent directory '{}' does not exist", parent.display()),
            ),
        });
    }

    std::fs::create_dir_all(parent).map_err(|e| ProcessError::ProcfileRead {
        path: path.to_string(),
        source: e,
    })?;

    Ok(())
}

async fn handle_run_command(run_matches: &clap::ArgMatches) -> Result<()> {
    #[cfg(windows)]
    install_console_ctrl_handler();

    // Ensure console ANSI support is active from the start.
    gaffa::platform::ensure_console_mode();

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

    // Apply the configured graceful-shutdown timeout.
    if let Some(secs) = run_matches.get_one::<u64>("shutdown-timeout") {
        manager
            .set_shutdown_timeout(Duration::from_secs(*secs))
            .await;
    }

    // Resolve the shell: --shell > GAFFA_SHELL > platform default
    let shell = gaffa::Shell::resolve(run_matches.get_one::<String>("shell").map(String::as_str));
    manager.set_shell(shell).await;

    // Set environment variables
    if !env_vars.is_empty() {
        manager.set_environment_variables(env_vars).await;
    }

    // Set up log file if specified. Each session gets its own rotated
    // file — `gaffa.log` → `gaffa-YYYY-MM-DD_NNN.log` — so prior sessions
    // stay on disk as backups next to the current one.
    let rotated_log_path: Option<std::path::PathBuf> = if let Some(path) = log_file_path {
        ensure_log_parent_dir(path)?;
        let rotated = rotate_log_path(path);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&rotated)
            .map_err(|e| ProcessError::ProcfileRead {
                path: rotated.display().to_string(),
                source: e,
            })?;
        manager.set_log_file(Arc::new(Mutex::new(file))).await;
        Some(rotated)
    } else {
        None
    };

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
        match run_non_interactive(
            manager.clone(),
            processes_to_run,
            procfile_path,
            log_file_path.map(String::as_str),
            rotated_log_path.as_deref(),
        )
        .await
        {
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
                )
                .arg(
                    Arg::new("shutdown-timeout")
                        .long("shutdown-timeout")
                        .value_name("SECONDS")
                        .help("Grace period (seconds) for each process to exit before force-kill (default: 10)")
                        .default_value("10")
                        .value_parser(clap::value_parser!(u64).range(1..=3600)),
                )
                .arg(
                    Arg::new("shell")
                        .long("shell")
                        .value_name("PROGRAM")
                        .help("Shell used to run commands (default: pwsh, fallback cmd, on Windows; sh on Unix; env override: GAFFA_SHELL)")
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
