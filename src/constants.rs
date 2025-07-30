use std::time::Duration;

// Process management timeouts
pub const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
pub const PROCESS_KILL_TIMEOUT: Duration = Duration::from_secs(3);
pub const SIGTERM_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
pub const PROCESS_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
pub const KILL_RETRY_WAIT: Duration = Duration::from_secs(1);

// Process monitoring intervals
pub const INITIAL_CHECK_INTERVAL: Duration = Duration::from_millis(100);
pub const MAX_CHECK_INTERVAL: Duration = Duration::from_secs(2);
pub const SPAWN_WAIT_DELAY: Duration = Duration::from_millis(100);

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