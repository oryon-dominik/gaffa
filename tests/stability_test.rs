//! Stability & memory safety tests for gaffa's process management.
//!
//! These tests document known issues and verify correct behavior around:
//! - Handle cleanup on natural process exit
//! - Log buffer caps and memory growth
//! - Handle accumulation across restart cycles
//! - Graceful shutdown for all exit paths
//! - Channel backpressure with fast producers
//! - Channel replacement and lost logs
//!
//! ## Known Bugs Documented
//!
//! - **HANDLE_LEAK**: monitor_handles and output_handles are not cleaned up
//!   when a process exits naturally. Only stop_process_internal removes them.
//! - **LOG_CLONE**: Every new log entry triggers clone_from of the entire
//!   VecDeque in the log_handle task (ui.rs:312).
//! - **LOST_LOGS**: Logs sent before run_terminal_ui() replaces the channel
//!   sender are silently lost because AppState::new() drops the receiver.
//! - **RESTART_STUBBORN**: restart_process fails with ProcessAlreadyRunning
//!   for stubborn processes because stop returns Ok but child stays in map.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gaffa::ui::{AppState, LogEntry};
use gaffa::{ProcessManager, ProcessStatus};
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a temporary Procfile with the given content and return its path.
fn create_procfile(content: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let path = format!("test_stability_procfile_{}_{count}.txt", std::process::id());
    std::fs::write(&path, content).expect("Failed to write test procfile");
    path
}

/// Remove a temporary Procfile.
fn cleanup_procfile(path: &str) {
    let _ = std::fs::remove_file(path);
}

/// Short-lived command that exits immediately (platform-specific).
fn echo_command() -> &'static str {
    if cfg!(windows) {
        r#"cmd /c "echo hello from gaffa test""#
    } else {
        "echo hello from gaffa test"
    }
}

/// Short-lived command that runs for a few seconds then exits.
/// Uses `ping` on Windows (responds to taskkill /T) or `sleep` on Unix.
/// Keep duration short (< 10s) so graceful shutdown timeout (10s) can handle it.
fn short_sleep_command() -> &'static str {
    if cfg!(windows) {
        // ping -n 4 = ~3 seconds. Short enough for taskkill /T to work
        // because the process exits before the 10s timeout.
        r#"cmd /c "ping -n 4 127.0.0.1 >nul""#
    } else {
        "sleep 3"
    }
}

/// Command that produces many output lines quickly (platform-specific).
/// The Windows variant is native PowerShell — pin the manager's shell to
/// `powershell` (always present) when using it.
fn fast_output_command(lines: u32) -> String {
    if cfg!(windows) {
        format!("1..{lines} | ForEach-Object {{ 'line ' + $_ }}")
    } else {
        format!(r#"i=1; while [ $i -le {lines} ]; do echo "line $i"; i=$((i+1)); done"#)
    }
}

// ===========================================================================
// 1. Handle cleanup on natural process exit
// ===========================================================================

#[tokio::test]
async fn test_natural_exit_updates_process_status() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("quick: {}", echo_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("quick").await.unwrap();

    // Wait for the process to finish on its own
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Process should be marked as Stopped
    let info = manager
        .get_process_info("quick")
        .await
        .expect("process should exist");
    assert_eq!(
        info.status,
        ProcessStatus::Stopped,
        "naturally exited process should have Stopped status"
    );

    // Should have an exit code
    assert!(
        info.exit_code.is_some(),
        "naturally exited process should have an exit code"
    );
    assert_eq!(info.exit_code, Some(0), "clean exit should have code 0");

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_natural_exit_removes_from_children() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("quick: {}", echo_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("quick").await.unwrap();

    // Immediately after start, child should be present
    {
        let children_count = manager.children_count().await;
        assert!(
            children_count > 0,
            "child should be in children map right after start"
        );
    }

    // Wait for natural exit
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Child should be removed by monitor task
    let children_count = manager.children_count().await;
    assert_eq!(
        children_count, 0,
        "children map should be empty after natural exit"
    );

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_natural_exit_handle_cleanup() {
    // KNOWN BUG [HANDLE_LEAK]: monitor_handles and output_handles are NOT
    // cleaned up when a process exits naturally. Only stop_process_internal()
    // calls .abort() and removes them. The monitor task completes but its
    // JoinHandle stays in the HashMap forever.
    let manager = Arc::new(ProcessManager::new());
    let content = format!("quick: {}", echo_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("quick").await.unwrap();

    // After start: should have 1 monitor handle and 2 output handles
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        manager.monitor_handle_count().await,
        1,
        "should have 1 monitor handle after start"
    );
    assert_eq!(
        manager.output_handle_count().await,
        2,
        "should have 2 output handles (stdout+stderr) after start"
    );

    // Wait for natural exit
    tokio::time::sleep(Duration::from_secs(3)).await;

    // FIXED [HANDLE_LEAK]: handles are now cleaned up on natural exit
    let monitor_count = manager.monitor_handle_count().await;
    let output_count = manager.output_handle_count().await;

    assert_eq!(
        monitor_count, 0,
        "FIXED [HANDLE_LEAK]: monitor handle should be cleaned up after natural exit"
    );
    assert_eq!(
        output_count, 0,
        "FIXED [HANDLE_LEAK]: output handles should be cleaned up after natural exit"
    );

    cleanup_procfile(&path);
}

// ===========================================================================
// 2. Log buffer caps and duplication
// ===========================================================================

#[tokio::test]
async fn test_log_buffer_cap_1000_in_log_handler() {
    // Simulates the log handler's behavior: VecDeque capped at 1000 entries
    const MAX_LOG_LINES: usize = 1000;
    let mut logs: VecDeque<LogEntry> = VecDeque::new();

    for i in 0..2000 {
        logs.push_back(LogEntry {
            timestamp: Instant::now(),
            process: "test".to_string(),
            content: format!("line {i}"),
            is_error: false,
        });
        if logs.len() > MAX_LOG_LINES {
            logs.pop_front();
        }
    }

    assert_eq!(logs.len(), MAX_LOG_LINES);
    assert_eq!(logs.front().unwrap().content, "line 1000");
    assert_eq!(logs.back().unwrap().content, "line 1999");
}

#[tokio::test]
async fn test_log_buffer_cap_10000_in_ui_state() {
    // Simulates the UIState's behavior: VecDeque capped at 10,000 entries
    const MAX_LOGS: usize = 10_000;
    let mut logs: VecDeque<LogEntry> = VecDeque::new();

    for i in 0..15_000 {
        logs.push_back(LogEntry {
            timestamp: Instant::now(),
            process: "test".to_string(),
            content: format!("line {i}"),
            is_error: false,
        });
        while logs.len() > MAX_LOGS {
            logs.pop_front();
        }
    }

    assert_eq!(logs.len(), MAX_LOGS);
    assert_eq!(logs.front().unwrap().content, "line 5000");
    assert_eq!(logs.back().unwrap().content, "line 14999");
}

#[tokio::test]
async fn test_log_triple_duplication_clone_overhead() {
    // DOCUMENTS KNOWN ISSUE [LOG_CLONE]: log_handle clones the entire VecDeque
    // into AppState.logs on every single log entry via state_logs.clone_from(&logs).
    //
    // This test measures the allocation pattern to document the overhead.
    let state = Arc::new(AppState::new());

    let start = Instant::now();
    for i in 0..500 {
        state
            .add_log("test".to_string(), format!("log entry {i}"), false)
            .await;
    }
    let duration = start.elapsed();

    // The send via channel is cheap. The expensive clone_from happens
    // in the log_handle task (not here), but we document the architecture.
    assert!(
        duration < Duration::from_secs(1),
        "adding 500 logs via channel should be fast, took {:?}",
        duration
    );
}

// ===========================================================================
// 3. Restart cycle handle accumulation
// ===========================================================================

#[tokio::test]
async fn test_restart_cleans_up_handles() {
    // Uses short_sleep_command (3s ping) which is terminable within the 10s timeout.
    let manager = Arc::new(ProcessManager::new());
    let content = format!("svc: {}", short_sleep_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("svc").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Initial state: 1 monitor, 2 output handles
    assert_eq!(manager.monitor_handle_count().await, 1);
    assert_eq!(manager.output_handle_count().await, 2);

    // Restart the process
    manager.restart_process("svc").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // After restart: should still be exactly 1 monitor and 2 output handles
    assert_eq!(
        manager.monitor_handle_count().await,
        1,
        "after restart, should have exactly 1 monitor handle (not accumulated)"
    );
    assert_eq!(
        manager.output_handle_count().await,
        2,
        "after restart, should have exactly 2 output handles (not accumulated)"
    );

    // Clean up: wait for the 3s process to become terminable, then stop
    manager.stop_all().await;
    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_multiple_restarts_no_handle_accumulation() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("svc: {}", short_sleep_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("svc").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let restart_count = 3;
    for i in 0..restart_count {
        manager.restart_process("svc").await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let children = manager.children_count().await;

        assert_eq!(
            children,
            1,
            "restart #{}: should have exactly 1 child process",
            i + 1
        );
    }

    // Verify restart count was tracked
    {
        let info = manager.get_process_info("svc").await.unwrap();
        assert_eq!(
            info.restart_count, restart_count,
            "restart count should match number of restarts"
        );
    }

    // Clean up
    manager.stop_all().await;
    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_restart_preserves_cumulative_runtime() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("svc: {}", short_sleep_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("svc").await.unwrap();

    // Let it run for a measurable time
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Restart
    manager.restart_process("svc").await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Cumulative runtime should include time from before the restart
    let info = manager.get_process_info("svc").await.unwrap();
    assert!(
        info.cumulative_runtime >= Duration::from_secs(1),
        "cumulative runtime should include pre-restart time, got {:?}",
        info.cumulative_runtime
    );

    // Clean up
    manager.stop_all().await;
    cleanup_procfile(&path);
}

// ===========================================================================
// 4. Graceful shutdown — all exit paths
// ===========================================================================

#[tokio::test]
async fn test_stop_process_updates_status_and_exit_code() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("svc: {}", short_sleep_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("svc").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Stop the process
    manager.stop_process("svc").await.unwrap();

    let info = manager.get_process_info("svc").await.unwrap();

    assert_eq!(info.status, ProcessStatus::Stopped);
    assert!(
        info.exit_code.is_some(),
        "stopped process should have exit code"
    );
    assert!(
        info.stopped_at.is_some(),
        "stopped process should have stopped_at timestamp"
    );

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_stop_process_cleans_up_handles() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("svc: {}", short_sleep_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("svc").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Before stop
    assert_eq!(manager.children_count().await, 1);
    assert_eq!(manager.monitor_handle_count().await, 1);
    assert_eq!(manager.output_handle_count().await, 2);

    // Stop
    manager.stop_process("svc").await.unwrap();

    // After stop: all handles should be cleaned up
    assert_eq!(
        manager.children_count().await,
        0,
        "children should be empty after stop"
    );
    assert_eq!(
        manager.monitor_handle_count().await,
        0,
        "monitor handles should be cleaned after stop"
    );
    assert_eq!(
        manager.output_handle_count().await,
        0,
        "output handles should be cleaned after stop"
    );

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_stop_all_stops_multiple_processes() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!(
        "svc1: {}\nsvc2: {}\nsvc3: {}",
        short_sleep_command(),
        short_sleep_command(),
        short_sleep_command()
    );
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();

    // Start all
    for name in manager.process_names().await {
        manager.start_process(&name).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        manager.children_count().await,
        3,
        "should have 3 running children"
    );

    // Stop all
    manager.stop_all().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // All should be stopped
    assert_eq!(
        manager.children_count().await,
        0,
        "all children should be gone"
    );

    let snapshot = manager.process_snapshot().await;
    for (name, info, _color) in &snapshot {
        assert_eq!(
            info.status,
            ProcessStatus::Stopped,
            "process '{name}' should be Stopped after stop_all"
        );
    }

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_stop_already_stopped_process_returns_error() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("svc: {}", echo_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("svc").await.unwrap();

    // Wait for natural exit
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Trying to stop an already-stopped process should return an error
    let result = manager.stop_process("svc").await;
    assert!(
        result.is_err(),
        "stopping an already-stopped process should return an error"
    );

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_stop_nonexistent_process_returns_error() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("svc: {}", echo_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();

    let result = manager.stop_process("nonexistent").await;
    assert!(
        result.is_err(),
        "stopping a nonexistent process should return an error"
    );

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_start_already_running_returns_error() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("svc: {}", short_sleep_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("svc").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let result = manager.start_process("svc").await;
    assert!(
        result.is_err(),
        "starting an already-running process should return an error"
    );

    // Cleanup
    manager.stop_all().await;
    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_stop_all_with_no_running_processes_is_noop() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("svc: {}", echo_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();

    // Don't start anything — stop_all should be a safe no-op
    manager.stop_all().await;

    assert_eq!(manager.children_count().await, 0);

    cleanup_procfile(&path);
}

// ===========================================================================
// 5. Channel backpressure
// ===========================================================================

#[tokio::test]
async fn test_fast_producer_log_channel() {
    // Test that the unbounded log channel handles fast producers without
    // blocking or deadlocking. Documents that there is NO backpressure —
    // the channel grows unboundedly if producers outpace consumers.
    let state = Arc::new(AppState::new());

    let start = Instant::now();
    let log_count = 10_000;

    for i in 0..log_count {
        state
            .add_log(
                format!("proc{}", i % 5),
                format!("fast output line {i}"),
                false,
            )
            .await;
    }

    let duration = start.elapsed();

    assert!(
        duration < Duration::from_secs(2),
        "sending {log_count} logs via unbounded channel should be fast, took {:?}",
        duration
    );
}

#[tokio::test]
async fn test_fast_output_process_doesnt_block() {
    // Start a process that produces many output lines quickly and verify
    // gaffa doesn't deadlock or lose the process handle.
    let manager = Arc::new(ProcessManager::new());
    if cfg!(windows) {
        manager
            .set_shell(gaffa::Shell::from_program("powershell"))
            .await;
    }
    let content = format!("fast: {}", fast_output_command(500));
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("fast").await.unwrap();

    // Wait for the process to finish producing output and exit
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Process should have exited cleanly
    let info = manager
        .get_process_info("fast")
        .await
        .expect("process should exist");
    assert_eq!(
        info.status,
        ProcessStatus::Stopped,
        "fast-output process should have stopped"
    );
    assert_eq!(
        info.exit_code,
        Some(0),
        "fast-output process should have exited cleanly"
    );

    cleanup_procfile(&path);
}

// ===========================================================================
// 6. AppState channel replacement — lost logs
// ===========================================================================

#[tokio::test]
async fn test_appstate_buffers_logs_before_ui_starts() {
    // FIXED [LOST_LOGS]: AppState::new() no longer creates a throwaway channel.
    // Logs sent before run_terminal_ui() replaces the sender are buffered directly.
    let state = AppState::new();

    state
        .add_log(
            "test".to_string(),
            "this log will be buffered".to_string(),
            false,
        )
        .await;

    // Logs are now buffered directly into AppState.logs when no consumer exists.
    let logs = state.logs.lock().await;
    assert_eq!(
        logs.len(),
        1,
        "FIXED [LOST_LOGS]: logs sent before UI starts are now buffered"
    );
}

#[tokio::test]
async fn test_appstate_logs_are_populated_via_channel_consumer() {
    // Verify that AppState.logs is only populated by an external consumer
    // (the log_handle task in run_terminal_ui), not by add_log itself.
    let state = Arc::new(AppState::new());

    // Create a proper channel and replace the sender
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LogEntry>();
    {
        let mut state_tx = state.tx.lock().await;
        *state_tx = Some(tx);
    }

    state
        .add_log(
            "test".to_string(),
            "this log should be receivable".to_string(),
            false,
        )
        .await;

    // The log should be available on the receiver
    let entry = rx.try_recv();
    assert!(entry.is_ok(), "log should be receivable on the channel");
    assert_eq!(entry.unwrap().content, "this log should be receivable");

    // But AppState.logs is still empty — the consumer populates it
    let logs = state.logs.lock().await;
    assert_eq!(
        logs.len(),
        0,
        "AppState.logs should be empty — the log_handle task populates it, not add_log"
    );
}

// ===========================================================================
// 7. Process state consistency
// ===========================================================================

#[tokio::test]
async fn test_process_status_transitions() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("svc: {}", short_sleep_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();

    // Initial status: Stopped (loaded but not started)
    {
        let info = manager.get_process_info("svc").await.unwrap();
        assert_eq!(info.status, ProcessStatus::Stopped);
        assert_eq!(info.restart_count, 0);
        assert!(info.last_restart.is_none());
    }

    // After start: Running
    manager.start_process("svc").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    {
        let info = manager.get_process_info("svc").await.unwrap();
        assert_eq!(info.status, ProcessStatus::Running);
        assert!(info.last_restart.is_some());
    }

    // After stop: Stopped
    manager.stop_process("svc").await.unwrap();
    {
        let info = manager.get_process_info("svc").await.unwrap();
        assert_eq!(info.status, ProcessStatus::Stopped);
        assert!(info.stopped_at.is_some());
    }

    // After restart from stopped: Running again
    manager.start_process("svc").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    {
        let info = manager.get_process_info("svc").await.unwrap();
        assert_eq!(info.status, ProcessStatus::Running);
        assert!(info.restart_count >= 1);
    }

    // Cleanup
    manager.stop_all().await;
    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_multiple_processes_independent_lifecycle() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("short: {}\nlong: {}", echo_command(), short_sleep_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();

    // Start both
    manager.start_process("short").await.unwrap();
    manager.start_process("long").await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Short should have exited naturally, long might still be running or just finished
    let short_info = manager.get_process_info("short").await.unwrap();
    assert_eq!(
        short_info.status,
        ProcessStatus::Stopped,
        "short-lived process should be stopped"
    );

    // Clean up whatever is left
    manager.stop_all().await;

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_log_file_write() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("logger: {}", echo_command());
    let path = create_procfile(&content);
    let log_path = format!("test_stability_log_{}.txt", std::process::id());

    // Set up log file
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap();
    manager.set_log_file(Arc::new(Mutex::new(log_file))).await;

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("logger").await.unwrap();

    // Wait for process to finish and output to be written
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Read log file
    let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();

    assert!(
        log_content.contains("hello from gaffa test"),
        "log file should contain process output, got: {log_content}"
    );
    assert!(
        log_content.contains("[logger]"),
        "log file should contain process label"
    );

    // Cleanup
    cleanup_procfile(&path);
    let _ = std::fs::remove_file(&log_path);
}

// ===========================================================================
// 8. Stress tests
// ===========================================================================

#[tokio::test]
async fn test_rapid_start_stop_cycles() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!("svc: {}", short_sleep_command());
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();

    for i in 0..3 {
        manager.start_process("svc").await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        manager.stop_process("svc").await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify clean state after each cycle
        assert_eq!(
            manager.children_count().await,
            0,
            "cycle {}: children should be empty after stop",
            i + 1
        );
    }

    // After all stop cycles, handles should be cleaned up
    assert_eq!(
        manager.monitor_handle_count().await,
        0,
        "after all stop cycles, monitor handles should be 0"
    );
    assert_eq!(
        manager.output_handle_count().await,
        0,
        "after all stop cycles, output handles should be 0"
    );

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_concurrent_stop_all_is_safe() {
    let manager = Arc::new(ProcessManager::new());
    let content = format!(
        "a: {}\nb: {}\nc: {}",
        short_sleep_command(),
        short_sleep_command(),
        short_sleep_command()
    );
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();
    for name in manager.process_names().await {
        manager.start_process(&name).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Call stop_all twice concurrently — should not panic or deadlock
    let m1 = manager.clone();
    let m2 = manager.clone();
    let (_r1, _r2) = tokio::join!(async move { m1.stop_all().await }, async move {
        m2.stop_all().await
    });

    // Both should complete without panicking (the join! proves no deadlock)
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        manager.children_count().await,
        0,
        "all children should be gone after concurrent stop_all"
    );

    cleanup_procfile(&path);
}

// ===========================================================================
// 9. Procfile parsing edge cases
// ===========================================================================

#[tokio::test]
async fn test_procfile_with_duplicate_names() {
    let manager = ProcessManager::new();
    let content = format!(
        "web: {}\nweb: {}\nweb: {}",
        echo_command(),
        echo_command(),
        echo_command()
    );
    let path = create_procfile(&content);

    manager.load_procfile(&path).await.unwrap();

    let mut names = manager.process_names().await;
    names.sort();

    // Duplicate names should be suffixed with .2, .3, etc.
    assert!(
        names.iter().any(|n| n == "web"),
        "should have 'web' process"
    );
    assert!(
        names.len() >= 2,
        "duplicate names should create multiple entries, got: {:?}",
        names
    );

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_procfile_empty_returns_error() {
    let manager = ProcessManager::new();
    let path = create_procfile("# just comments\n\n");

    let result = manager.load_procfile(&path).await;
    assert!(result.is_err(), "empty procfile should return error");

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_procfile_invalid_format_returns_error() {
    let manager = ProcessManager::new();
    let path = create_procfile("this is not valid");

    let result = manager.load_procfile(&path).await;
    assert!(
        result.is_err(),
        "invalid procfile format should return error"
    );

    cleanup_procfile(&path);
}

#[tokio::test]
async fn test_environment_variables_applied_to_process() {
    let manager = Arc::new(ProcessManager::new());

    // Use a command that prints an env var
    let content = if cfg!(windows) {
        "env_test: cmd /c \"echo %GAFFA_TEST_VAR%\"".to_string()
    } else {
        "env_test: sh -c 'echo $GAFFA_TEST_VAR'".to_string()
    };
    let path = create_procfile(&content);
    let log_path = format!("test_env_log_{}.txt", std::process::id());

    // Set env var and log file
    let mut env_vars = std::collections::HashMap::new();
    env_vars.insert("GAFFA_TEST_VAR".to_string(), "hello_from_env".to_string());
    manager.set_environment_variables(env_vars).await;

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap();
    manager.set_log_file(Arc::new(Mutex::new(log_file))).await;

    manager.load_procfile(&path).await.unwrap();
    manager.start_process("env_test").await.unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;

    let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log_content.contains("hello_from_env"),
        "log should contain the env var value, got: {log_content}"
    );

    cleanup_procfile(&path);
    let _ = std::fs::remove_file(&log_path);
}
