//! The MCP server that a daemon runs, and the promise that it goes away with it.
//!
//! Servers are started by launchers (`npx`, `uvx`, a shell script) that start the
//! real thing, so killing the process that was started is not enough: the whole
//! tree has to go. On Windows that is a job object that kills its members when it
//! closes, on Unix a process group.
#![allow(
    unsafe_code,
    reason = "Windows job objects and Unix process groups are system calls"
)]

use std::{fs::File, io, process::Stdio};

use tokio::process::{Child, Command};

/// A running server and the means to end all of it.
pub struct Server {
    pub child: Child,
    #[cfg(windows)]
    _job: Job,
    #[cfg(unix)]
    group: i32,
}

/// Start `command` with its standard input and output piped and its standard
/// error written to `log`.
pub fn spawn(command: &[String], log: File) -> io::Result<Server> {
    let program = which::which(&command[0]).unwrap_or_else(|_| command[0].clone().into());
    let mut process = Command::new(program);
    process
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // No console window for a server that was started without a console.
        process.creation_flags(0x0800_0000);
    }
    #[cfg(unix)]
    {
        process.process_group(0);
    }
    let child = process.spawn()?;

    #[cfg(windows)]
    {
        let job = Job::new()?;
        if let Some(handle) = child.raw_handle() {
            job.assign(handle)?;
        }
        Ok(Server { child, _job: job })
    }
    #[cfg(unix)]
    {
        let group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .unwrap_or(0);
        Ok(Server { child, group })
    }
}

#[cfg(unix)]
impl Drop for Server {
    fn drop(&mut self) {
        if self.group > 0 {
            // SAFETY: killpg takes a process group id and a signal number.
            unsafe {
                libc::killpg(self.group, libc::SIGTERM);
            }
        }
    }
}

#[cfg(windows)]
struct Job(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a job object handle may be used from any thread.
#[cfg(windows)]
unsafe impl Send for Job {}

#[cfg(windows)]
impl Job {
    fn new() -> io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // SAFETY: plain calls with a null name and attributes, and a zeroed
        // structure of the type the information class asks for.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size = u32::try_from(std::mem::size_of_val(&info)).expect("a small structure");
            let set = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                size,
            );
            if set == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    fn assign(&self, process: std::os::windows::io::RawHandle) -> io::Result<()> {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        // SAFETY: both handles are open; the process handle is the child's.
        let assigned = unsafe { AssignProcessToJobObject(self.0, process) };
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and is closed once. Closing it ends every
        // process in the job.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
