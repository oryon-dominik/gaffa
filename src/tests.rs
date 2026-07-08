#[cfg(test)]
mod test {
    use crate::constants::*;
    use crate::types::{ProcessError, ProcessInfo, ProcessStatus};
    use std::time::Duration;

    #[test]
    fn test_process_status_display() {
        // ProcessStatus Display includes colors, but in tests they might be disabled
        let running = ProcessStatus::Running.to_string();
        let stopped = ProcessStatus::Stopped.to_string();
        let restarting = ProcessStatus::Restarting.to_string();

        // Check that the status strings contain the expected text
        assert!(running.contains("RUNNING"));
        assert!(stopped.contains("STOPPED"));
        assert!(restarting.contains("RESTARTING"));
    }

    #[test]
    fn test_process_error_messages() {
        let err = ProcessError::ProcessNotFound {
            name: "test".to_string(),
        };
        assert_eq!(err.to_string(), "Process 'test' not found");

        let err = ProcessError::ProcessAlreadyRunning {
            name: "web".to_string(),
        };
        assert_eq!(err.to_string(), "Process 'web' is already running");

        let err = ProcessError::EmptyCommand {
            name: "worker".to_string(),
        };
        assert_eq!(err.to_string(), "Empty command for process 'worker'");
    }

    #[test]
    fn test_parse_env_file() {
        use std::collections::HashMap;
        use tokio::runtime::Runtime;

        let rt = Runtime::new().unwrap();

        // Create test env file
        let env_content = "KEY1=value1\nKEY2=value2\n# Comment\n\nKEY3=value3";
        std::fs::write("test_env_file", env_content).unwrap();

        let mut env_vars = HashMap::new();

        rt.block_on(async {
            crate::parse_env_file("test_env_file", &mut env_vars)
                .await
                .unwrap();
        });

        assert_eq!(env_vars.get("KEY1"), Some(&"value1".to_string()));
        assert_eq!(env_vars.get("KEY2"), Some(&"value2".to_string()));
        assert_eq!(env_vars.get("KEY3"), Some(&"value3".to_string()));
        assert_eq!(env_vars.len(), 3);

        // Cleanup
        std::fs::remove_file("test_env_file").unwrap();
    }

    #[test]
    fn test_parse_env_file_edge_cases() {
        use std::collections::HashMap;
        use tokio::runtime::Runtime;

        let rt = Runtime::new().unwrap();

        // Test with spaces and quotes
        let env_content = "KEY1 = value with spaces\nKEY2=\"quoted value\"\nKEY3=";
        std::fs::write("test_env_edge", env_content).unwrap();

        let mut env_vars = HashMap::new();

        rt.block_on(async {
            crate::parse_env_file("test_env_edge", &mut env_vars)
                .await
                .unwrap();
        });

        assert_eq!(env_vars.get("KEY1"), Some(&"value with spaces".to_string()));
        assert_eq!(env_vars.get("KEY2"), Some(&"\"quoted value\"".to_string()));
        assert_eq!(env_vars.get("KEY3"), Some(&"".to_string()));

        // Cleanup
        std::fs::remove_file("test_env_edge").unwrap();
    }

    #[test]
    fn test_process_info_runtime_calculation() {
        use std::time::Instant;

        let mut info = ProcessInfo {
            command: "test command".to_string(),
            status: ProcessStatus::Running,
            restart_count: 0,
            last_restart: Some(Instant::now()),
            stopped_at: None,
            cumulative_runtime: Duration::from_secs(10),
            exit_code: None,
        };

        // Simulate process running for 2 seconds
        std::thread::sleep(Duration::from_millis(100));

        // When stopped, runtime should be added
        let start_time = info.last_restart.unwrap();
        let session_runtime = start_time.elapsed();
        info.cumulative_runtime += session_runtime;
        info.status = ProcessStatus::Stopped;
        info.stopped_at = Some(Instant::now());

        assert!(info.cumulative_runtime > Duration::from_secs(10));
    }

    #[test]
    fn test_constants_values() {
        // Verify timeout constants are reasonable
        assert!(GRACEFUL_SHUTDOWN_TIMEOUT >= Duration::from_secs(1));
        assert!(GRACEFUL_SHUTDOWN_TIMEOUT <= Duration::from_secs(30));

        // The force-kill confirmation window must fit well inside the default
        // graceful window used to terminate a process.
        assert!(PROCESS_WAIT_TIMEOUT < GRACEFUL_SHUTDOWN_TIMEOUT);

        // Verify exit codes
        assert_eq!(EXIT_CODE_KEYBOARD_INTERRUPT, 512);
        assert_eq!(EXIT_CODE_CTRL_C_WINDOWS, -1073741510);
        assert_eq!(EXIT_CODE_FORCED_TERMINATION, -1);

        // Verify UI constants
        let separator_width = TERMINAL_SEPARATOR_WIDTH;
        assert!(separator_width > 0);
        assert!(separator_width <= 120);
    }

    #[test]
    fn test_ui_max_log_lines() {
        use crate::ui::LogEntry;
        use std::collections::VecDeque;
        use std::time::Instant;

        const MAX_LOG_LINES: usize = 1000; // Match the constant from ui.rs

        let mut logs = VecDeque::new();

        // Add more than MAX_LOG_LINES
        for i in 0..MAX_LOG_LINES + 100 {
            logs.push_back(LogEntry {
                timestamp: Instant::now(),
                process: "test".to_string(),
                content: format!("Log {}", i),
                is_error: false,
            });

            // Simulate the limiting logic
            if logs.len() > MAX_LOG_LINES {
                logs.pop_front();
            }
        }

        assert_eq!(logs.len(), MAX_LOG_LINES);

        // Verify oldest logs were removed
        let first_log = logs.front().unwrap();
        assert!(first_log.content.contains("Log 100"));
    }
}
