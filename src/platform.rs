use tokio::process::Child;

#[cfg(target_os = "windows")]
pub mod windows {
    use super::*;
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

    pub async fn terminate_process(child: &mut Child, timeout: Duration) -> Option<i32> {
        if let Some(pid) = child.id() {
            // Children are spawned with CREATE_NEW_PROCESS_GROUP, so `pid`
            // doubles as the process group id. `GenerateConsoleCtrlEvent` can
            // only deliver CTRL_BREAK_EVENT to a targeted group — CTRL_C_EVENT
            // is restricted to group 0 (the caller's own group). Python,
            // Node.js and most console runtimes handle Ctrl+Break as a
            // graceful-shutdown signal, which is what we want here.
            unsafe {
                use winapi::um::wincon::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
                let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
            }

            // Poll at a short interval so responsive children return quickly.
            let check_interval = Duration::from_millis(100);
            let max_checks =
                ((timeout.as_millis() / check_interval.as_millis()) as usize).max(1);

            for _ in 0..max_checks {
                if let Ok(Some(status)) = child.try_wait() {
                    return status.code();
                }
                tokio::time::sleep(check_interval).await;
            }

            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }

            // Stubborn child — force kill as last resort.
            force_kill_process(child).await;

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

    /// Ensure the console output mode has the flags required for correct ANSI
    /// escape-code interpretation and newline translation (`\n` → `\r\n`).
    ///
    /// On Windows, Ctrl+C can corrupt the console mode, disabling
    /// `ENABLE_VIRTUAL_TERMINAL_PROCESSING` and `ENABLE_PROCESSED_OUTPUT`.
    /// Calling this before every line of coloured output guarantees the
    /// terminal renders ANSI sequences instead of showing raw `←[94m` garbage.
    ///
    /// The check is cheap (one `GetConsoleMode` syscall); `SetConsoleMode` is
    /// only called when the flags are actually missing.
    pub fn ensure_console_mode() {
        use winapi::shared::minwindef::DWORD;
        use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
        use winapi::um::processenv::GetStdHandle;
        use winapi::um::winbase::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

        const ENABLE_PROCESSED_OUTPUT: DWORD = 0x0001;
        const ENABLE_WRAP_AT_EOL_OUTPUT: DWORD = 0x0002;
        const ENABLE_VIRTUAL_TERMINAL_PROCESSING: DWORD = 0x0004;
        const REQUIRED_FLAGS: DWORD =
            ENABLE_PROCESSED_OUTPUT | ENABLE_WRAP_AT_EOL_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;

        unsafe {
            for std_handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let handle = GetStdHandle(std_handle);
                if handle.is_null() {
                    continue;
                }
                let mut mode: DWORD = 0;
                // GetConsoleMode fails for redirected handles — skip those.
                if GetConsoleMode(handle, &mut mode) != 0 && (mode & REQUIRED_FLAGS != REQUIRED_FLAGS)
                {
                    mode |= REQUIRED_FLAGS;
                    let _ = SetConsoleMode(handle, mode);
                }
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub mod unix {
    use super::*;
    use crate::constants::{PROCESS_KILL_TIMEOUT, SIGTERM_WAIT_TIMEOUT};
    use std::time::Duration;

    pub fn configure_command(cmd: &mut tokio::process::Command) {
        use std::os::unix::process::CommandExt;
        // Create new process group
        cmd.process_group(0);
    }

    /// No-op on Unix — process groups handle cleanup.
    pub fn assign_child_to_job(_child: &Child) {}

    pub async fn terminate_process(child: &mut Child, timeout: Duration) -> Option<i32> {
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

        // Final wait, capped at the caller-supplied timeout.
        match tokio::time::timeout(timeout, child.wait()).await {
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

    /// No-op on Unix — ANSI escape codes are natively supported.
    pub fn ensure_console_mode() {}
}

// Re-export the appropriate module based on the target OS
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(target_os = "windows"))]
pub use unix::*;
