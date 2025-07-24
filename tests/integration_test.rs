use std::process::Command;
use std::thread;
use std::time::Duration;

#[test]
fn test_gaffa_help() {
    let output = Command::new("cargo")
        .args(["run", "--", "--help"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("gaffa"));
    assert!(stdout.contains("Procfile"));
}

#[test]
fn test_gaffa_no_args() {
    let output = Command::new("cargo")
        .args(["run"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Gaffa - Procfile Process Manager"));
    assert!(stdout.contains("Usage:"));
}

#[test]
fn test_gaffa_run_nonexistent_procfile() {
    let output = Command::new("cargo")
        .args(["run", "--", "run", "-f", "nonexistent.procfile"])
        .output()
        .expect("Failed to execute command");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Failed to read Procfile"));
}

#[test]
fn test_gaffa_with_test_procfile() {
    // Create a simple test Procfile
    let procfile_content = "echo: echo 'Hello from test'";
    std::fs::write("test_integration.procfile", procfile_content)
        .expect("Failed to write test procfile");

    // Start gaffa with the test procfile in a separate thread
    let handle = thread::spawn(|| {
        let mut child = Command::new("cargo")
            .args(["run", "--", "run", "-f", "test_integration.procfile"])
            .spawn()
            .expect("Failed to start gaffa");

        // Let it run for a bit
        thread::sleep(Duration::from_secs(2));

        // Kill the process
        let _ = child.kill();
        child.wait().expect("Failed to wait for child");
    });

    handle.join().expect("Thread panicked");

    // Cleanup
    let _ = std::fs::remove_file("test_integration.procfile");
}

#[test]
fn test_gaffa_log_file() {
    // Create a simple test Procfile
    let procfile_content = "logger: echo 'Log this message'";
    std::fs::write("test_log.procfile", procfile_content).expect("Failed to write test procfile");

    // Start gaffa with log file
    let handle = thread::spawn(|| {
        let mut child = Command::new("cargo")
            .args([
                "run",
                "--",
                "run",
                "-f",
                "test_log.procfile",
                "--log-file",
                "test_output.log",
            ])
            .spawn()
            .expect("Failed to start gaffa");

        // Let it run for a bit
        thread::sleep(Duration::from_secs(2));

        // Kill the process
        let _ = child.kill();
        child.wait().expect("Failed to wait for child");
    });

    handle.join().expect("Thread panicked");

    // Check if log file was created
    assert!(std::path::Path::new("test_output.log").exists());

    // Read and verify log content
    let log_content = std::fs::read_to_string("test_output.log").expect("Failed to read log file");
    assert!(log_content.contains("logger"));

    // Cleanup
    let _ = std::fs::remove_file("test_log.procfile");
    let _ = std::fs::remove_file("test_output.log");
}

#[test]
fn test_gaffa_specific_processes() {
    // Create a test Procfile with multiple processes
    let procfile_content =
        "web: echo 'Web server'\nworker: echo 'Worker process'\nscheduler: echo 'Scheduler'";
    std::fs::write("test_specific.procfile", procfile_content)
        .expect("Failed to write test procfile");

    // Start gaffa with only specific processes
    let handle = thread::spawn(|| {
        let mut child = Command::new("cargo")
            .args([
                "run",
                "--",
                "run",
                "-f",
                "test_specific.procfile",
                "web",
                "worker",
            ])
            .spawn()
            .expect("Failed to start gaffa");

        // Let it run for a bit
        thread::sleep(Duration::from_secs(2));

        // Kill the process
        let _ = child.kill();
        child.wait().expect("Failed to wait for child");
    });

    handle.join().expect("Thread panicked");

    // Cleanup
    let _ = std::fs::remove_file("test_specific.procfile");
}
