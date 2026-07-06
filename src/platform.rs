use tokio::process::Child;

#[cfg(target_os = "windows")]
pub mod windows {
    use super::*;
    use crate::constants::{PROCESS_WAIT_TIMEOUT, TERMINATION_POLL_INTERVAL};
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

    /// Terminate gracefully (Ctrl+Break), escalating to a force kill.
    /// Waits up to `timeout` for the graceful signal to take effect before
    /// escalating. Returns the exit status, or `None` if the process is still
    /// running.
    pub async fn terminate_process(
        child: &mut Child,
        timeout: Duration,
    ) -> Option<std::process::ExitStatus> {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }

        if let Some(pid) = child.id() {
            // Graceful: Ctrl+Break to the child's process group (the child is
            // its own group leader thanks to CREATE_NEW_PROCESS_GROUP).
            // Console apps actually receive this — taskkill without /F only
            // posts WM_CLOSE, which windowless console processes never see.
            let signal_sent = unsafe {
                use winapi::um::wincon::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
                GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0
            };

            if signal_sent {
                // Poll for exit — returns as soon as the process is gone,
                // within the caller-supplied graceful window.
                let mut waited = Duration::ZERO;
                while waited < timeout {
                    if let Ok(Some(status)) = child.try_wait() {
                        return Some(status);
                    }
                    tokio::time::sleep(TERMINATION_POLL_INTERVAL).await;
                    waited += TERMINATION_POLL_INTERVAL;
                }
            }
            // No console to deliver the signal (services, CI) — skip straight
            // to the force kill instead of waiting for a signal never sent.

            // Escalate: force kill the whole process tree.
            force_kill_process(child).await;
        }

        // Confirm the exit — taskkill returns before the process is gone.
        match tokio::time::timeout(PROCESS_WAIT_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            _ => None,
        }
    }

    /// Force kill the whole process tree. Fire-and-forget: taskkill returns
    /// before the process exits — callers confirm via `child.wait()`.
    pub async fn force_kill_process(child: &mut Child) {
        if let Some(pid) = child.id() {
            let _ = tokio::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .output()
                .await;
        }
    }

    /// Ensure the console output mode has the flags required for correct ANSI
    /// escape-code interpretation and newline translation (`\n` → `\r\n`).
    ///
    /// On Windows, Ctrl+C and child processes that write to the shared
    /// console can corrupt the mode, disabling
    /// `ENABLE_VIRTUAL_TERMINAL_PROCESSING` and `ENABLE_PROCESSED_OUTPUT`.
    /// Calling this before every line of coloured output guarantees the
    /// terminal renders ANSI sequences instead of showing raw `←[94m` garbage
    /// followed by a CP437 `◙` for each unprocessed `\n`.
    ///
    /// The flags are set unconditionally rather than gated on
    /// `GetConsoleMode` — the check used to race with child processes that
    /// mutated the mode between our read and write, leaving gaffa's output
    /// garbled during shutdown when children like uvicorn/podman emit their
    /// own terminal sequences.
    pub fn ensure_console_mode() {
        use winapi::shared::minwindef::DWORD;
        use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
        use winapi::um::processenv::GetStdHandle;
        use winapi::um::winbase::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

        const ENABLE_PROCESSED_OUTPUT: DWORD = 0x0001;
        const ENABLE_WRAP_AT_EOL_OUTPUT: DWORD = 0x0002;
        const ENABLE_VIRTUAL_TERMINAL_PROCESSING: DWORD = 0x0004;
        const REQUIRED_FLAGS: DWORD = ENABLE_PROCESSED_OUTPUT
            | ENABLE_WRAP_AT_EOL_OUTPUT
            | ENABLE_VIRTUAL_TERMINAL_PROCESSING;

        unsafe {
            for std_handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let handle = GetStdHandle(std_handle);
                if handle.is_null() {
                    continue;
                }
                let mut mode: DWORD = 0;
                // GetConsoleMode fails for redirected handles — skip those,
                // they are not a real console and SetConsoleMode would fail.
                if GetConsoleMode(handle, &mut mode) != 0 {
                    let _ = SetConsoleMode(handle, mode | REQUIRED_FLAGS);
                }
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub mod unix {
    use super::*;
    use crate::constants::{PROCESS_WAIT_TIMEOUT, TERMINATION_POLL_INTERVAL};
    use std::time::Duration;

    pub fn configure_command(cmd: &mut tokio::process::Command) {
        use std::os::unix::process::CommandExt;
        // Create new process group
        cmd.process_group(0);
    }

    /// No-op on Unix — process groups handle cleanup.
    pub fn assign_child_to_job(_child: &Child) {}

    /// Terminate gracefully (SIGTERM), escalating to SIGKILL.
    /// Waits up to `timeout` for the process to exit before escalating.
    /// Returns the exit status, or `None` if the process is still running.
    pub async fn terminate_process(
        child: &mut Child,
        timeout: Duration,
    ) -> Option<std::process::ExitStatus> {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }

        if let Some(pid) = child.id() {
            // Graceful: SIGTERM to the whole process group (the child is its
            // own group leader thanks to process_group(0)). Fall back to the
            // pid alone if the group kill fails.
            let signal_sent = unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM) == 0
                    || libc::kill(pid as i32, libc::SIGTERM) == 0
            };

            if signal_sent {
                // Poll for exit — returns as soon as the process is gone,
                // within the caller-supplied graceful window.
                let mut waited = Duration::ZERO;
                while waited < timeout {
                    if let Ok(Some(status)) = child.try_wait() {
                        return Some(status);
                    }
                    tokio::time::sleep(TERMINATION_POLL_INTERVAL).await;
                    waited += TERMINATION_POLL_INTERVAL;
                }
            }

            // Escalate: SIGKILL to the whole process group.
            force_kill_process(child).await;
        }

        // Confirm the exit (also reaps the child).
        match tokio::time::timeout(PROCESS_WAIT_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            _ => None,
        }
    }

    /// Force kill the whole process tree. Fire-and-forget: callers confirm
    /// the exit via `child.wait()`.
    pub async fn force_kill_process(child: &mut Child) {
        if let Some(pid) = child.id() {
            unsafe {
                if libc::kill(-(pid as i32), libc::SIGKILL) != 0 {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    }

    /// No-op on Unix — ANSI escape codes are natively supported.
    pub fn ensure_console_mode() {}
}

// Re-export the appropriate module based on the target OS
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(target_os = "windows"))]
pub use unix::*;
