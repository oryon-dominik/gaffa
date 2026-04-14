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
                // Dim box-drawing separator tinted to the process colour so
                // each stream reads as a single visual column.
                let sep = "│".color(process_color).dimmed();
                println!("{colored_name}{padding} {sep} {}", line);

                // Force immediate output to terminal
                use std::io::{Write, stdout};
                let _ = stdout().flush();

                // Write to log file if available
                if let Some(log_file) = &log_file_stdout {
                    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                    let clean = strip_ansi_escapes(&line);
                    let log_line = format!("[{timestamp}] [{stdout_name}] {clean}\n");
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
                // Red-tinted separator so stderr lines are distinguishable
                // from stdout without shouting.
                let sep = "│".red().dimmed();
                println!("{colored_name}{padding} {sep} {}", line);

                // Force immediate output to terminal
                use std::io::{Write, stdout};
                let _ = stdout().flush();

                // Write to log file if available
                if let Some(log_file) = &log_file {
                    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                    let clean = strip_ansi_escapes(&line);
                    let log_line = format!("[{timestamp}] [STDERR] [{stderr_name}] {clean}\n");
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
    let sep = "│".magenta().dimmed();
    format!("{colored_gaffa}{padding} {sep} {colored_msg}")
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
    let sep = "│".red().dimmed();
    format!("{colored_gaffa}{padding} {sep} {colored_msg}")
}

/// Get a process color by index (consistent assignment).
///
/// Wraps around the available colors when the index exceeds the palette size.
pub fn get_process_color(index: usize) -> Color {
    PROCESS_COLORS[index % PROCESS_COLORS.len()]
}

/// Strip ANSI CSI and OSC escape sequences from `input` so log files contain
/// plain text even when a child process (e.g. Vite, npm) writes coloured
/// output. Non-escape content is preserved verbatim.
pub fn strip_ansi_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // ESC (0x1B) begins a control sequence.
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                // CSI: ESC [ params* intermediates* final
                // params:        0x30..=0x3F  (digits, ';', '?', etc.)
                // intermediates: 0x20..=0x2F
                // final:         0x40..=0x7E
                b'[' => {
                    i += 2;
                    while i < bytes.len() && (0x30..=0x3f).contains(&bytes[i]) {
                        i += 1;
                    }
                    while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() && (0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    continue;
                }
                // OSC: ESC ] ... (BEL | ESC \)
                b']' => {
                    i += 2;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
                // Single-char escapes (RIS, NEL, etc.) — drop ESC + one byte.
                _ => {
                    i += 2;
                    continue;
                }
            }
        }

        // Non-escape byte: copy as-is. `input` is valid UTF-8 so we can push
        // the char at this boundary without splitting a multi-byte sequence.
        let ch_end = next_char_boundary(bytes, i);
        out.push_str(&input[i..ch_end]);
        i = ch_end;
    }

    out
}

fn next_char_boundary(bytes: &[u8], i: usize) -> usize {
    let first = bytes[i];
    let width = if first < 0x80 {
        1
    } else if first < 0xc0 {
        // Continuation byte in the middle of a sequence — shouldn't happen
        // for valid UTF-8 but be defensive.
        1
    } else if first < 0xe0 {
        2
    } else if first < 0xf0 {
        3
    } else {
        4
    };
    (i + width).min(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::strip_ansi_escapes;

    #[test]
    fn strips_csi_color_codes() {
        let input = "  \x1b[32m\x1b[1mVITE\x1b[22m v5.4.21\x1b[39m  ready";
        assert_eq!(strip_ansi_escapes(input), "  VITE v5.4.21  ready");
    }

    #[test]
    fn strips_csi_with_params() {
        let input = "\x1b[38;2;255;0;0mred\x1b[0m";
        assert_eq!(strip_ansi_escapes(input), "red");
    }

    #[test]
    fn strips_osc_terminated_by_bel() {
        let input = "\x1b]0;title\x07hello";
        assert_eq!(strip_ansi_escapes(input), "hello");
    }

    #[test]
    fn preserves_plain_text() {
        let input = "GET /login/ HTTP/1.1 200 1845";
        assert_eq!(strip_ansi_escapes(input), input);
    }

    #[test]
    fn preserves_unicode() {
        let input = "\x1b[32m➜\x1b[39m  Local";
        assert_eq!(strip_ansi_escapes(input), "➜  Local");
    }
}
