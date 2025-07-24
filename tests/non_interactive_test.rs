use std::process::Command;

#[test]
fn test_interactive_flag() {
    // Test that --interactive flag is accepted
    let output = Command::new("cargo")
        .args(["run", "--", "run", "--interactive", "--help"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Run processes from Procfile") || output.status.success());
}

#[test]
#[ignore] // This test requires building the binary
fn test_non_interactive_output() {
    // Create a test Procfile
    std::fs::write(
        "test_procfile_non_interactive.txt",
        "echo_test: echo Hello from non-interactive mode\n",
    )
    .expect("Failed to write test procfile");

    // Run in default non-interactive mode (no --interactive flag)
    let output = Command::new("cargo")
        .args([
            "run",
            "--",
            "run",
            "--procfile",
            "test_procfile_non_interactive.txt",
        ])
        .output();

    // Clean up
    let _ = std::fs::remove_file("test_procfile_non_interactive.txt");

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Should show startup message
        assert!(
            stdout.contains("Starting") || stderr.contains("Starting"),
            "Output should contain startup message. stdout: {}, stderr: {}",
            stdout,
            stderr
        );
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn test_help_includes_interactive() {
        // Test that help text includes the --interactive option
        let output = Command::new("cargo")
            .args(["run", "--", "run", "--help"])
            .output()
            .expect("Failed to execute command");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("--interactive")
                || stdout.contains("-i")
                || stdout.contains("Terminal UI"),
            "Help should mention --interactive flag"
        );
    }
}
