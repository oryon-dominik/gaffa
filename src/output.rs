use std::sync::Arc;

use colored::{Color, Colorize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::Mutex,
};

use crate::constants::PROCESS_COLORS;
use crate::ui::AppState;

/// Spawn tasks to handle process stdout and stderr streams.
///
/// Returns `(stdout_handle, stderr_handle)` so the caller can store them.
pub fn spawn_output_handlers(
    name: &str,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    app_state: Option<Arc<AppState>>,
    log_file: Option<Arc<Mutex<std::fs::File>>>,
    max_name_len: usize,
    process_color: Color,
) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let name_str = name.to_string();
    let stdout_reader = BufReader::new(stdout);
    let app_state_stdout = app_state.clone();
    let log_file_stdout = log_file.clone();

    let stdout_name = name_str.clone();
    let stdout_handle = tokio::spawn(async move {
        let mut lines = stdout_reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim_end().to_string();

            // Skip empty lines to avoid clutter
            if line.is_empty() {
                continue;
            }

            if let Some(state) = &app_state_stdout {
                state
                    .add_log(stdout_name.clone(), line.clone(), false)
                    .await;
            } else {
                // Re-enable VTP before printing — Ctrl+C on Windows can
                // corrupt the console mode between lines of output.
                crate::platform::ensure_console_mode();

                let colored_name = stdout_name.color(process_color);
                let padding = " ".repeat(max_name_len.saturating_sub(stdout_name.len()));
                println!("{colored_name}{padding} | {}", line);

                // Force immediate output to terminal
                use std::io::{Write, stdout};
                let _ = stdout().flush();

                // Write to log file if available
                if let Some(log_file) = &log_file_stdout {
                    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                    let log_line = format!("[{timestamp}] [{stdout_name}] {line}\n");
                    let mut file = log_file.lock().await;
                    let _ = file.write_all(log_line.as_bytes());
                    let _ = file.flush();
                }
            }
        }
    });

    let stderr_name = name_str;
    let stderr_reader = BufReader::new(stderr);
    let app_state_stderr = app_state;

    let stderr_handle = tokio::spawn(async move {
        let mut lines = stderr_reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim_end().to_string();

            // Skip empty lines to avoid clutter
            if line.is_empty() {
                continue;
            }

            if let Some(state) = &app_state_stderr {
                state.add_log(stderr_name.clone(), line.clone(), true).await;
            } else {
                // Re-enable VTP before printing — Ctrl+C on Windows can
                // corrupt the console mode between lines of output.
                crate::platform::ensure_console_mode();

                let colored_name = stderr_name.color(process_color);
                let padding = " ".repeat(max_name_len.saturating_sub(stderr_name.len()));
                println!("{colored_name}{padding} | {}", line);

                // Force immediate output to terminal
                use std::io::{Write, stdout};
                let _ = stdout().flush();

                // Write to log file if available
                if let Some(log_file) = &log_file {
                    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                    let log_line = format!("[{timestamp}] [STDERR] [{stderr_name}] {line}\n");
                    let mut file = log_file.lock().await;
                    let _ = file.write_all(log_line.as_bytes());
                    let _ = file.flush();
                }
            }
        }
    });

    (stdout_handle, stderr_handle)
}

/// Format a system message with "gaffa" prefix in magenta.
///
/// Returns the formatted string; the caller decides whether to `println!` or
/// `eprintln!`.
pub fn format_gaffa_message(message: &str, max_name_len: usize) -> String {
    let colored_gaffa = "gaffa".magenta();
    let colored_msg = message.magenta();
    let padding_len = max_name_len.saturating_sub(5); // "gaffa" is 5 chars
    let padding = " ".repeat(padding_len);
    format!("{colored_gaffa}{padding} | {colored_msg}")
}

/// Format an error message with "gaffa" prefix in red.
///
/// Returns the formatted string; the caller decides whether to `println!` or
/// `eprintln!`.
pub fn format_error_message(message: &str, max_name_len: usize) -> String {
    let colored_gaffa = "gaffa".red();
    let colored_msg = message.red();
    let padding_len = max_name_len.saturating_sub(5); // "gaffa" is 5 chars
    let padding = " ".repeat(padding_len);
    format!("{colored_gaffa}{padding} | {colored_msg}")
}

/// Get a process color by index (consistent assignment).
///
/// Wraps around the available colors when the index exceeds the palette size.
pub fn get_process_color(index: usize) -> Color {
    PROCESS_COLORS[index % PROCESS_COLORS.len()]
}
