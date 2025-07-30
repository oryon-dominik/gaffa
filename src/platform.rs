use tokio::process::Child;

#[cfg(target_os = "windows")]
pub mod windows {
    use super::*;
    use crate::constants::GRACEFUL_SHUTDOWN_TIMEOUT;
    use std::time::Duration;
    
    pub fn configure_command(cmd: &mut tokio::process::Command) {
        // On Windows, we'll use the default console group
        // This allows processes to receive Ctrl+C from the console
        let _ = cmd; // Suppress unused warning
    }
    
    pub async fn terminate_process(child: &mut Child) -> Option<i32> {
        // First try to terminate the process gracefully using taskkill without /F
        if let Some(pid) = child.id() {
            // Try taskkill without /F (force) flag for graceful termination
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/PID", &pid.to_string()])
                .output();
            
            // Check periodically if the process has exited
            let check_interval = Duration::from_millis(500);
            let max_checks = (GRACEFUL_SHUTDOWN_TIMEOUT.as_millis() / check_interval.as_millis()) as usize;
            
            for _ in 0..max_checks {
                // Check if process exited
                if let Ok(Some(status)) = child.try_wait() {
                    return status.code();
                }
                
                // Wait before next check
                tokio::time::sleep(check_interval).await;
            }
            
            // Final check after full timeout
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }
            
            // Process is still running - it's ignoring termination requests
            // Don't force kill - let the process manager handle it
            None
        } else {
            None
        }
    }
    
    pub async fn force_kill_process(child: &mut Child) -> Option<i32> {
        if let Some(pid) = child.id() {
            // Force kill with /F and /T flags
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .output();
        }
        None
    }
}

#[cfg(not(target_os = "windows"))]
pub mod unix {
    use super::*;
    use crate::constants::{SIGTERM_WAIT_TIMEOUT, PROCESS_KILL_TIMEOUT, PROCESS_WAIT_TIMEOUT};
    
    pub fn configure_command(cmd: &mut tokio::process::Command) {
        use std::os::unix::process::CommandExt;
        // Create new process group
        cmd.process_group(0);
    }
    
    pub async fn terminate_process(child: &mut Child) -> Option<i32> {
        if let Some(pid) = child.id() {
            unsafe {
                // Send SIGTERM to the process
                libc::kill(pid as i32, libc::SIGTERM);
            }
            
            // Give time for graceful shutdown
            tokio::time::sleep(SIGTERM_WAIT_TIMEOUT).await;
            
            // Check if process exited
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }
            
            // Try SIGTERM to process group
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
            
            // Wait again
            tokio::time::sleep(PROCESS_KILL_TIMEOUT).await;
            
            // Check again
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }
        }
        
        // Try tokio's kill method
        match tokio::time::timeout(PROCESS_WAIT_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) => status.code(),
            _ => None,
        }
    }
    
    pub async fn force_kill_process(child: &mut Child) -> Option<i32> {
        if let Some(pid) = child.id() {
            unsafe {
                // Send SIGKILL to the process group
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        None
    }
}

// Re-export the appropriate module based on the target OS
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(target_os = "windows"))]
pub use unix::*;