use tokio::process::Child;

#[cfg(target_os = "windows")]
pub mod windows {
    use super::*;
    use crate::constants::GRACEFUL_SHUTDOWN_TIMEOUT;
    use std::sync::OnceLock;
    use std::time::Duration;
    use winapi::shared::minwindef::{DWORD, FALSE};
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    };
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::winnt::{
        HANDLE, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JobObjectExtendedLimitInformation,
        PROCESS_ALL_ACCESS,
    };

    /// RAII wrapper for a Windows Job Object handle.
    /// When dropped, the handle is closed and — because of
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — all assigned processes are killed.
    struct JobObject(HANDLE);

    // SAFETY: The Job Object HANDLE is process-global and thread-safe.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl Drop for JobObject {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Global Job Object — created once, lives for the gaffa process lifetime.
    static JOB: OnceLock<JobObject> = OnceLock::new();

    /// Create (or return existing) Job Object configured with KILL_ON_JOB_CLOSE.
    fn get_or_create_job() -> Option<HANDLE> {
        let job = JOB.get_or_init(|| {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if handle.is_null() {
                    eprintln!("[gaffa] WARNING: failed to create Job Object — child cleanup on exit will not work");
                    return JobObject(std::ptr::null_mut());
                }

                // Configure: kill all processes when the job handle is closed
                let mut info: winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                    std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

                let ok = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of::<winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()
                        as DWORD,
                );

                if ok == FALSE {
                    eprintln!("[gaffa] WARNING: failed to configure Job Object — child cleanup on exit will not work");
                    CloseHandle(handle);
                    return JobObject(std::ptr::null_mut());
                }

                JobObject(handle)
            }
        });

        if job.0.is_null() { None } else { Some(job.0) }
    }

    /// Assign a spawned child process to the global Job Object.
    /// This ensures the child (and its entire process tree) is killed when gaffa
    /// exits — whether gracefully, via Ctrl+C, or via crash.
    pub fn assign_child_to_job(child: &Child) {
        let Some(pid) = child.id() else { return };
        let Some(job_handle) = get_or_create_job() else {
            return;
        };

        unsafe {
            let process_handle = OpenProcess(PROCESS_ALL_ACCESS, FALSE, pid);
            if process_handle.is_null() {
                eprintln!("[gaffa] WARNING: failed to open process {pid} for job assignment");
                return;
            }

            let ok = AssignProcessToJobObject(job_handle, process_handle);
            if ok == FALSE {
                eprintln!("[gaffa] WARNING: failed to assign process {pid} to job object");
            }

            CloseHandle(process_handle);
        }
    }

    pub fn configure_command(cmd: &mut tokio::process::Command) {
        // Create the child in a new process group so Ctrl+C is not forwarded
        // directly from the console — gaffa manages shutdown itself.
        // `creation_flags` is available on tokio::process::Command on Windows.
        cmd.creation_flags(crate::constants::CREATE_NEW_PROCESS_GROUP);
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
            let max_checks =
                (GRACEFUL_SHUTDOWN_TIMEOUT.as_millis() / check_interval.as_millis()) as usize;

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

            // Process is still running — force kill as last resort
            force_kill_process(child).await;

            // One final check
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }

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
    use crate::constants::{PROCESS_KILL_TIMEOUT, PROCESS_WAIT_TIMEOUT, SIGTERM_WAIT_TIMEOUT};

    pub fn configure_command(cmd: &mut tokio::process::Command) {
        use std::os::unix::process::CommandExt;
        // Create new process group
        cmd.process_group(0);
    }

    /// No-op on Unix — process groups handle cleanup.
    pub fn assign_child_to_job(_child: &Child) {}

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
