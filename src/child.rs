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
    job: Job,
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
        Ok(Server { child, job })
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

/// What all the processes of a server use together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub processes: u64,
    /// Resident memory on Unix and the working set on Windows, added up. Pages
    /// that processes share are counted for each of them.
    pub memory_bytes: u64,
}

/// Enough to ask for the usage of a server without holding on to it. It is only
/// meaningful while the server it came from is alive.
#[derive(Clone, Copy)]
pub struct Probe {
    #[cfg(unix)]
    group: i32,
    #[cfg(windows)]
    job: usize,
}

impl Server {
    pub fn probe(&self) -> Probe {
        Probe {
            #[cfg(unix)]
            group: self.group,
            #[cfg(windows)]
            job: self.job.0 as usize,
        }
    }
}

#[cfg(unix)]
impl Probe {
    /// The processes of the group, from `ps`, which Linux and macOS both have.
    pub fn usage(self) -> Option<Usage> {
        let output = std::process::Command::new("ps")
            .args(["-axo", "pgid=,rss="])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let (mut processes, mut kibibytes) = (0, 0_u64);
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.split_whitespace();
            let (Some(group), Some(rss)) = (fields.next(), fields.next()) else {
                continue;
            };
            if group.parse::<i32>().ok() == Some(self.group) {
                processes += 1;
                kibibytes += rss.parse::<u64>().unwrap_or(0);
            }
        }
        (processes > 0).then_some(Usage {
            processes,
            memory_bytes: kibibytes * 1024,
        })
    }
}

#[cfg(windows)]
impl Probe {
    /// The processes of the job object, and the working set of each.
    pub fn usage(self) -> Option<Usage> {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::{
                JobObjects::{
                    JOBOBJECT_BASIC_PROCESS_ID_LIST, JobObjectBasicProcessIdList,
                    QueryInformationJobObject,
                },
                ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
                Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
            },
        };
        const CAPACITY: usize = 1024;
        // Two `u32`, then the list: eight bytes of header and one word per id.
        let mut buffer = vec![0_usize; 1 + CAPACITY];
        let size = u32::try_from(buffer.len() * std::mem::size_of::<usize>()).ok()?;
        // SAFETY: the buffer is aligned for the structure and large enough for
        // `CAPACITY` ids, the job handle is open while the server lives, and the
        // slice is cut to the number of ids the call says it wrote.
        unsafe {
            let queried = QueryInformationJobObject(
                self.job as _,
                JobObjectBasicProcessIdList,
                buffer.as_mut_ptr().cast(),
                size,
                std::ptr::null_mut(),
            );
            if queried == 0 {
                return None;
            }
            let list = &*buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            let count = (list.NumberOfProcessIdsInList as usize).min(CAPACITY);
            let ids = std::slice::from_raw_parts(list.ProcessIdList.as_ptr(), count);
            let mut memory_bytes = 0_u64;
            for id in ids {
                let Ok(id) = u32::try_from(*id) else { continue };
                let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, id);
                if process.is_null() {
                    continue;
                }
                let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
                counters.cb = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>())
                    .expect("a small structure");
                if GetProcessMemoryInfo(process, &raw mut counters, counters.cb) != 0 {
                    memory_bytes += counters.WorkingSetSize as u64;
                }
                CloseHandle(process);
            }
            (count > 0).then_some(Usage {
                processes: count as u64,
                memory_bytes,
            })
        }
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
