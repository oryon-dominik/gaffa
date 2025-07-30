use tokio::process::Child;
use crate::constants::*;

#[cfg(target_os = "windows")]
pub mod windows {
    use super::*;
    #[allow(unused_imports)]
    use crate::constants::CREATE_NEW_PROCESS_GROUP;
    
    pub fn configure_command(cmd: &mut tokio::process::Command) {
        // Tokio's Command doesn't expose creation_flags on Windows yet
        // This would need platform-specific handling or a different approach
        let _ = cmd; // Suppress unused warning
    }
    
    pub async fn terminate_process(child: &mut Child) -> Option<i32> {
        if let Some(pid) = child.id() {
            // Try Ctrl+C first (SIGINT)
            unsafe {
                use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_C_EVENT};
                let _ = GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid);
            }
            
            // Give time for graceful shutdown
            tokio::time::sleep(PROCESS_KILL_TIMEOUT).await;
            
            // Check if process exited
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }
            
            // Try Ctrl+C again
            unsafe {
                use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_C_EVENT};
                let _ = GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid);
            }
            
            // Give more time
            tokio::time::sleep(PROCESS_KILL_TIMEOUT).await;
            
            // Check again
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }
            
            // Now try Ctrl+Break as last resort before force kill
            unsafe {
                use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
                let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
            }
            
            tokio::time::sleep(PROCESS_KILL_TIMEOUT).await;
            
            // Check once more
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }

            // Try child.kill() which might work for some processes
            let _ = child.kill().await;
            tokio::time::sleep(KILL_RETRY_WAIT).await;
            
            // Check again
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }
        }
        None
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