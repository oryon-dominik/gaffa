use std::sync::Arc;

use colored::{Color, Colorize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::Mutex,
};
use unicode_width::UnicodeWidthChar;

use crate::constants::PROCESS_COLORS;
use crate::ui::AppState;

/// Wrap a raw line so each chunk fits within `content_w` display columns
/// while preserving ANSI CSI/OSC escape sequences verbatim (they contribute
/// 0 width). Each chunk except the last gets a trailing SGR reset so any
/// colour span in progress does not bleed into the next prefixed row.
fn wrap_ansi_preserving(input: &str, content_w: usize) -> Vec<String> {
    if content_w == 0 {
        return vec![input.to_string()];
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w: usize = 0;
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // ANSI escape: copy the whole sequence, 0 display width.
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            let start = i;
            match bytes[i + 1] {
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
                }
                b']' => {
                    i += 2;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b
                            && i + 1 < bytes.len()
                            && bytes[i + 1] == b'\\'
                        {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    i += 2;
                }
            }
            current.push_str(&input[start..i]);
            continue;
        }

        // Next UTF-8 codepoint.
        let ch = input[i..].chars().next().unwrap();
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);

        if current_w + ch_w > content_w && !current.is_empty() {
            current.push_str("\x1b[0m");
            chunks.push(std::mem::take(&mut current));
            current_w = 0;
        }

        current.push(ch);
        current_w += ch_w;
        i += ch.len_utf8();
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Query the current terminal width, returning `None` when stdout is not a
/// TTY (piped, redirected, or the query fails).
fn current_term_width() -> Option<usize> {
    crossterm::terminal::size()
        .ok()
        .map(|(w, _)| w as usize)
        .filter(|w| *w > 0)
}

/// Print a prefixed process line, wrapping its content so continuation rows
/// align under the content column with a blank name cell and the same
/// coloured separator. Falls back to a single unwrapped print when stdout
/// is not a TTY or the terminal is too narrow to host a meaningful split.
fn print_prefixed_wrapped(
    name: &str,
    max_name_len: usize,
    process_color: Color,
    sep_color: Color,
    line: &str,
) {
    let padding_len = max_name_len.saturating_sub(name.len());
    let padding = " ".repeat(padding_len);
    let colored_name = name.color(process_color);
    let sep = "│".color(sep_color).dimmed();
    // Fixed prefix display width: name (padded to max) + " │ "
    let prefix_w = max_name_len + 3;

    let term_w = current_term_width();
    // Need room for at least ~10 content columns to bother wrapping.
    let content_w = match term_w {
        Some(w) if w > prefix_w + 10 => w - prefix_w,
        _ => {
            println!("{colored_name}{padding} {sep} {line}");
            return;
        }
    };

    let chunks = wrap_ansi_preserving(line, content_w);
    let blank_name = " ".repeat(max_name_len);
    for (idx, chunk) in chunks.iter().enumerate() {
        if idx == 0 {
            println!("{colored_name}{padding} {sep} {chunk}");
        } else {
            println!("{blank_name} {sep} {chunk}");
        }
    }
}

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

                // Stdout: separator tinted to the process colour so each
                // stream reads as a single visual column. Wraps at terminal
                // width with continuation rows aligned under the content.
                print_prefixed_wrapped(
                    &stdout_name,
                    max_name_len,
                    process_color,
                    process_color,
                    &line,
                );

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

                // Stderr: red-tinted separator so these lines are
                // distinguishable from stdout at a glance without shouting.
                print_prefixed_wrapped(
                    &stderr_name,
                    max_name_len,
                    process_color,
                    Color::Red,
                    &line,
                );

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

    use super::wrap_ansi_preserving;

    #[test]
    fn wrap_short_line_is_single_chunk() {
        let chunks = wrap_ansi_preserving("short line", 80);
        assert_eq!(chunks, vec!["short line".to_string()]);
    }

    #[test]
    fn wrap_splits_on_display_width() {
        let chunks = wrap_ansi_preserving("abcdefghij", 4);
        // 10 chars, width 4 → "abcd", "efgh", "ij"; each non-last chunk has
        // a trailing reset.
        assert_eq!(
            chunks,
            vec![
                "abcd\x1b[0m".to_string(),
                "efgh\x1b[0m".to_string(),
                "ij".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_preserves_ansi_across_splits() {
        // "\x1b[32mHELLO WORLD\x1b[0m" at width 5 → "\x1b[32mHELLO" + reset,
        // then " WORL" + reset, then "D\x1b[0m". Escapes carry 0 width.
        let input = "\x1b[32mHELLO WORLD\x1b[0m";
        let chunks = wrap_ansi_preserving(input, 5);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].contains("HELLO"));
        assert!(chunks[0].ends_with("\x1b[0m"));
        assert!(chunks[1].contains(" WORL"));
        assert!(chunks[1].ends_with("\x1b[0m"));
        assert!(chunks[2].contains("D"));
    }

    #[test]
    fn wrap_zero_width_returns_unmodified() {
        let chunks = wrap_ansi_preserving("anything", 0);
        assert_eq!(chunks, vec!["anything".to_string()]);
    }
}
