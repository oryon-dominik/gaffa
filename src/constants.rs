use std::time::Duration;

// Process management timeouts
// Default graceful shutdown timeout — overridable at runtime via
// `--shutdown-timeout` (see `ProcessManager::set_shutdown_timeout`).
pub const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// Window to confirm a force-killed process has actually exited.
pub const PROCESS_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Poll interval while waiting for a terminating process to exit.
pub const TERMINATION_POLL_INTERVAL: Duration = Duration::from_millis(100);

// Exit codes
pub const EXIT_CODE_KEYBOARD_INTERRUPT: i32 = 512;
pub const EXIT_CODE_CTRL_C_WINDOWS: i32 = -1073741510;
pub const EXIT_CODE_FORCED_TERMINATION: i32 = -1;

// Windows specific constants
#[cfg(target_os = "windows")]
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

// Terminal display
pub const TERMINAL_SEPARATOR_WIDTH: usize = 79;
pub const MAX_DISPLAY_LINES: usize = 10;

// Process colors (excluding magenta which is reserved for gaffa)
pub const PROCESS_COLORS: &[colored::Color] = &[
    colored::Color::Cyan,
    colored::Color::Yellow,
    colored::Color::Blue,
    colored::Color::Green,
    colored::Color::BrightCyan,
    colored::Color::BrightYellow,
    colored::Color::BrightBlue,
];
