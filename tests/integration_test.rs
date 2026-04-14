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

    // The new binary prints help to stderr when no command is given
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Cross-platform process manager for Procfile-based applications"));
    assert!(stderr.contains("Usage:"));
}

#[test]
fn test_gaffa_run_nonexistent_procfile() {
    let output = Command::new("cargo")
        .args(["run", "--", "run", "-p", "nonexistent.procfile"])
        .output()
        .expect("Failed to execute command");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Failed to read Procfile"));
}

#[test]
fn test_gaffa_with_test_procfile() {
    // Create a simple test Procfile
    let procfile_content = if cfg!(windows) {
        "echo: cmd /c echo Hello from test"
    } else {
        "echo: echo 'Hello from test'"
    };
    std::fs::write("test_integration.procfile", procfile_content)
        .expect("Failed to write test procfile");

    // Start gaffa with the test procfile in a separate thread
    let handle = thread::spawn(|| {
        let mut child = Command::new("cargo")
            .args(["run", "--", "run", "-p", "test_integration.procfile"])
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
    let procfile_content = if cfg!(windows) {
        "logger: cmd /c echo Log this message"
    } else {
        "logger: echo 'Log this message'"
    };
    std::fs::write("test_log.procfile", procfile_content).expect("Failed to write test procfile");

    // Start gaffa with log file
    let handle = thread::spawn(|| {
        let mut child = Command::new("cargo")
            .args([
                "run",
                "--",
                "run",
                "-p",
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

    // gaffa rotates `test_output.log` to `test_output-YYYY-MM-DD_NNN.log`,
    // so find the most recent rotated file produced by this session.
    let rotated: Vec<std::path::PathBuf> = std::fs::read_dir(".")
        .expect("Failed to read current directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("test_output-") && n.ends_with(".log"))
                .unwrap_or(false)
        })
        .collect();
    assert!(
        !rotated.is_empty(),
        "No rotated test_output-*.log file was created"
    );

    let log_path = rotated
        .iter()
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .expect("Failed to pick newest rotated log");
    let log_content = std::fs::read_to_string(log_path).expect("Failed to read log file");
    assert!(log_content.contains("logger"));

    // Cleanup
    let _ = std::fs::remove_file("test_log.procfile");
    for p in &rotated {
        let _ = std::fs::remove_file(p);
    }
    let _ = std::fs::remove_file("test_output.log");
}

#[test]
fn test_gaffa_specific_processes() {
    // Create a test Procfile with multiple processes
    let procfile_content = if cfg!(windows) {
        "web: cmd /c echo Web server\nworker: cmd /c echo Worker process\nscheduler: cmd /c echo Scheduler"
    } else {
        "web: echo 'Web server'\nworker: echo 'Worker process'\nscheduler: echo 'Scheduler'"
    };
    std::fs::write("test_specific.procfile", procfile_content)
        .expect("Failed to write test procfile");

    // Start gaffa with only specific processes
    let handle = thread::spawn(|| {
        let mut child = Command::new("cargo")
            .args([
                "run",
                "--",
                "run",
                "-p",
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
