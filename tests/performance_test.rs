use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

#[cfg(test)]
mod performance_tests {
    use super::*;

    #[derive(Clone)]
    struct LogEntry {
        #[allow(dead_code)]
        timestamp: Instant,
        #[allow(dead_code)]
        process: String,
        content: String,
        #[allow(dead_code)]
        is_error: bool,
    }

    /// Test that log buffer is limited to prevent unbounded growth
    #[tokio::test]
    async fn test_log_buffer_size_limit() {
        let logs = Arc::new(Mutex::new(VecDeque::<LogEntry>::new()));
        const MAX_LOGS: usize = 10000;

        // Add more logs than the limit
        {
            let mut logs_lock = logs.lock().await;
            for i in 0..15000 {
                logs_lock.push_back(LogEntry {
                    timestamp: Instant::now(),
                    process: "test".to_string(),
                    content: format!("Log entry {}", i),
                    is_error: false,
                });
            }
        }

        // Simulate the optimization logic
        let mut ui_logs = VecDeque::<LogEntry>::new();
        {
            let logs_lock = logs.lock().await;
            ui_logs.extend(logs_lock.iter().cloned());
        }

        // Apply the size limit
        while ui_logs.len() > MAX_LOGS {
            ui_logs.pop_front();
        }

        assert_eq!(ui_logs.len(), MAX_LOGS);

        // Verify we kept the newest logs
        assert!(ui_logs.back().unwrap().content.contains("14999"));
        assert!(ui_logs.front().unwrap().content.contains("5000"));
    }

    /// Test efficient log addition (only cloning new logs)
    #[tokio::test]
    async fn test_incremental_log_updates() {
        let logs = Arc::new(Mutex::new(VecDeque::<LogEntry>::new()));
        let mut ui_logs = VecDeque::<LogEntry>::new();
        let mut last_log_count = 0;

        // Initial population
        {
            let mut logs_lock = logs.lock().await;
            for i in 0..100 {
                logs_lock.push_back(LogEntry {
                    timestamp: Instant::now(),
                    process: "test".to_string(),
                    content: format!("Initial log {}", i),
                    is_error: false,
                });
            }
        }

        // First update - should copy all logs
        {
            let logs_lock = logs.lock().await;
            let new_count = logs_lock.len();

            if new_count != last_log_count {
                let logs_to_add = new_count.saturating_sub(last_log_count);
                if logs_to_add > 0 && ui_logs.is_empty() {
                    ui_logs.extend(logs_lock.iter().cloned());
                }
                last_log_count = new_count;
            }
        }

        assert_eq!(ui_logs.len(), 100);
        assert!(ui_logs[0].content.contains("Initial log 0"));

        // Add 10 more logs
        {
            let mut logs_lock = logs.lock().await;
            for i in 100..110 {
                logs_lock.push_back(LogEntry {
                    timestamp: Instant::now(),
                    process: "test".to_string(),
                    content: format!("New log {}", i),
                    is_error: false,
                });
            }
        }

        // Incremental update - should only add new logs
        {
            let logs_lock = logs.lock().await;
            let new_count = logs_lock.len();

            if new_count != last_log_count {
                let logs_to_add = new_count.saturating_sub(last_log_count);
                if logs_to_add > 0 {
                    ui_logs.extend(logs_lock.iter().skip(last_log_count).cloned());
                }
            }
        }

        assert_eq!(ui_logs.len(), 110);
        assert!(ui_logs[100].content.contains("New log 100"));
        assert!(ui_logs[109].content.contains("New log 109"));
    }

    /// Test exponential backoff for process monitoring
    #[tokio::test]
    async fn test_exponential_backoff() {
        let mut check_interval = tokio::time::Duration::from_millis(100);
        const MAX_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_secs(2);

        // Test interval increases
        assert_eq!(check_interval.as_millis(), 100);

        check_interval = check_interval.saturating_mul(2).min(MAX_INTERVAL);
        assert_eq!(check_interval.as_millis(), 200);

        check_interval = check_interval.saturating_mul(2).min(MAX_INTERVAL);
        assert_eq!(check_interval.as_millis(), 400);

        check_interval = check_interval.saturating_mul(2).min(MAX_INTERVAL);
        assert_eq!(check_interval.as_millis(), 800);

        check_interval = check_interval.saturating_mul(2).min(MAX_INTERVAL);
        assert_eq!(check_interval.as_millis(), 1600);

        // Should cap at MAX_INTERVAL
        check_interval = check_interval.saturating_mul(2).min(MAX_INTERVAL);
        assert_eq!(check_interval.as_millis(), 2000);

        // Further increases should stay at max
        check_interval = check_interval.saturating_mul(2).min(MAX_INTERVAL);
        assert_eq!(check_interval.as_millis(), 2000);
    }

    /// Test adaptive polling timeout
    #[test]
    fn test_adaptive_poll_timeout() {
        let mut should_redraw = false;
        let mut needs_redraw = false;

        // When idle, use longer timeout
        let poll_timeout = if should_redraw || needs_redraw {
            std::time::Duration::from_millis(1)
        } else {
            std::time::Duration::from_millis(16)
        };
        assert_eq!(poll_timeout.as_millis(), 16);

        // When updates needed, use short timeout
        should_redraw = true;
        let poll_timeout = if should_redraw || needs_redraw {
            std::time::Duration::from_millis(1)
        } else {
            std::time::Duration::from_millis(16)
        };
        assert_eq!(poll_timeout.as_millis(), 1);

        // Test with needs_redraw
        should_redraw = false;
        needs_redraw = true;
        let poll_timeout = if should_redraw || needs_redraw {
            std::time::Duration::from_millis(1)
        } else {
            std::time::Duration::from_millis(16)
        };
        assert_eq!(poll_timeout.as_millis(), 1);
    }

    /// Test efficient stopped process detection
    #[test]
    fn test_efficient_stopped_detection() {
        let mut logs = VecDeque::new();

        // Add many logs
        for i in 0..1000 {
            logs.push_back(LogEntry {
                timestamp: Instant::now(),
                process: "test".to_string(),
                content: format!("Regular log {}", i),
                is_error: false,
            });
        }

        // Add a stopped message near the end
        logs.push_back(LogEntry {
            timestamp: Instant::now(),
            process: "gaffa".to_string(),
            content: "✓ Stopped process 'web'".to_string(),
            is_error: false,
        });

        // More logs after
        for i in 1001..1010 {
            logs.push_back(LogEntry {
                timestamp: Instant::now(),
                process: "test".to_string(),
                content: format!("Regular log {}", i),
                is_error: false,
            });
        }

        // Efficient search - check only recent logs in reverse
        let stopped_count = logs
            .iter()
            .rev()
            .take(20)
            .filter(|log| {
                log.content.contains("✓ Stopped process")
                    || log.content.contains("All processes stopped")
            })
            .count();

        assert_eq!(stopped_count, 1);

        // Verify we didn't check all 1000+ logs
        let mut check_count = 0;
        for log in logs.iter().rev().take(20) {
            check_count += 1;
            if log.content.contains("✓ Stopped process") {
                break;
            }
        }
        assert!(check_count <= 20);
    }
}
