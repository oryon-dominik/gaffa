#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

/// Test that sending SIGINT to gaffa causes child processes to be terminated.
///
/// This spawns gaffa with a long-running child (`sleep 300`), sends SIGINT,
/// then checks that the child's PID is no longer alive.
#[cfg(unix)]
#[test]
fn test_sigint_terminates_child_processes() {
    use std::io::{BufRead, BufReader};

    // Build first so we can run the binary directly
    let build = Command::new("cargo")
        .args(["build"])
        .output()
        .expect("Failed to build");
    assert!(build.status.success(), "cargo build failed");

    let binary = std::path::Path::new("target/debug/gaffa");
    assert!(binary.exists(), "gaffa binary not found at {:?}", binary);

    // Create a procfile with a long-running process that writes its PID
    let marker = format!("gaffa_signal_test_{}", std::process::id());
    let procfile_content = format!(
        "sleeper: bash -c 'echo {marker} $$ && exec sleep 300'\n"
    );

    let test_dir = std::env::temp_dir().join(format!("gaffa_signal_test_{}", std::process::id()));
    std::fs::create_dir_all(&test_dir).unwrap();
    let procfile_path = test_dir.join("Procfile");
    std::fs::write(&procfile_path, &procfile_content).unwrap();

    // Start gaffa directly, with its own process group so SIGINT doesn't leak.
    // Redirect stdout to a file to avoid broken pipe when the test stops reading.
    let stdout_path = test_dir.join("stdout.log");
    let stdout_file = std::fs::File::create(&stdout_path).unwrap();
    let mut gaffa = unsafe {
        Command::new(binary)
            .args([
                "run",
                "-p",
                procfile_path.to_str().unwrap(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(stdout_file)
            .stderr(std::process::Stdio::piped())
            .pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            })
            .spawn()
            .expect("Failed to start gaffa")
    };

    // Poll the stdout file until we see the marker with the child PID
    let mut child_pid: Option<u32> = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);

    while std::time::Instant::now() < deadline {
        if let Ok(file) = std::fs::File::open(&stdout_path) {
            let reader = BufReader::new(file);
            for line in reader.lines().flatten() {
                if line.contains(&marker) {
                    if let Some(pid_str) = line.split_whitespace().last() {
                        if let Ok(pid) = pid_str.parse::<u32>() {
                            child_pid = Some(pid);
                        }
                    }
                }
            }
        }
        if child_pid.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let child_pid = child_pid.expect("Failed to capture child process PID from output");

    // Verify the child is alive before we send the signal
    let alive_before = unsafe { libc::kill(child_pid as i32, 0) };
    assert_eq!(alive_before, 0, "Child PID {} should be alive before SIGINT", child_pid);

    // Small delay to ensure gaffa's main loop is fully running
    std::thread::sleep(Duration::from_millis(200));

    // Send SIGINT to the gaffa process (simulating Ctrl+C)
    let gaffa_pid = gaffa.id();
    unsafe {
        libc::kill(gaffa_pid as i32, libc::SIGINT);
    }

    // Wait for gaffa to exit (with timeout to avoid hanging)
    let start = std::time::Instant::now();
    let status: std::process::ExitStatus = loop {
        match gaffa.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = gaffa.kill();
                    panic!("gaffa did not exit within 30 seconds after SIGINT");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("Error waiting for gaffa: {}", e),
        }
    };

    // gaffa should exit with 130 (128 + SIGINT=2) on interrupt
    assert_eq!(
        status.code(),
        Some(130),
        "Expected exit code 130 on SIGINT, got {:?}",
        status.code()
    );

    // Give a brief moment for OS cleanup
    std::thread::sleep(Duration::from_millis(500));

    // Verify the child process is no longer alive
    let alive_after = unsafe { libc::kill(child_pid as i32, 0) };
    assert_ne!(
        alive_after, 0,
        "Child PID {} should be terminated after gaffa received SIGINT",
        child_pid
    );

    // Cleanup
    let _ = std::fs::remove_dir_all(&test_dir);
}
