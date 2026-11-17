//! Child process management: cleaning up whole process trees, hiding console windows, and
//! launching detached GUIs.
//!
//! - Unix: spawn in a new process group (pgid == pid) and clean up with `killpg`, as before.
//! - Windows: a **Job object** instead of a process group. The child is created suspended, put in
//!   the Job and then resumed, so every grandchild it spawns (Chromium renderers and GPU helpers,
//!   Electron helpers, …) lands in the same Job. The Job is created with `KILL_ON_JOB_CLOSE`, so
//!   no descendants survive even if GhostDesk crashes.

use std::process::{Child, Command};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Keeps a black console window from flashing when launching console programs (cmd,
/// claude.cmd, taskkill, …).
pub fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    cmd
}

/// Spawns so the child and its descendants can be cleaned up together with `kill_group`.
///
/// On Windows this overwrites `creation_flags`; use `spawn_group_with_flags` if other flags are
/// needed.
pub fn spawn_group(cmd: &mut Command) -> std::io::Result<Child> {
    spawn_group_with_flags(cmd, 0)
}

/// Like `spawn_group`, with extra Windows creation flags (ignored on Unix).
pub fn spawn_group_with_flags(cmd: &mut Command, extra_windows_flags: u32) -> std::io::Result<Child> {
    #[cfg(unix)]
    {
        let _ = extra_windows_flags;
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        cmd.spawn()
    }
    #[cfg(windows)]
    {
        win_job::spawn(cmd, extra_windows_flags)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = extra_windows_flags;
        cmd.spawn()
    }
}

/// Force-kills a child spawned with `spawn_group`, descendants included, and reaps it.
///
/// No group signal is sent if the child was already reaped (its pid may have been reused).
pub fn kill_group(child: &mut Child) {
    let running = matches!(child.try_wait(), Ok(None));
    #[cfg(unix)]
    if running {
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        // even if the main process is gone, helpers left in the Job die when the Job is closed
        win_job::terminate(child.id(), running);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Pids of live processes in the group (the Job on Windows). Only `Some` on Windows; on Unix the
/// caller finds them through pgid and parent links.
pub fn group_pids(pid: u32) -> Option<Vec<u32>> {
    #[cfg(windows)]
    {
        win_job::pids(pid)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

/// Checks whether the child has exited without reaping it (Windows: `WaitForSingleObject(…, 0)`).
/// Returns `Some(exit code)` if it has.
#[cfg(windows)]
pub fn peek_exit(child: &Child) -> Option<i32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    let h = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    unsafe {
        if WaitForSingleObject(h, 0) != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0u32;
        if GetExitCodeProcess(h, &mut code) == 0 {
            return Some(-1);
        }
        Some(code as i32)
    }
}

/// Whether a process with this pid is running (a rough check).
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                // access denied (another user, protected process) counts as alive
                return std::io::Error::last_os_error().raw_os_error() == Some(5);
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Launches a GUI process detached from the parent (used when the MCP server opens the GhostDesk
/// window).
///
/// Windows: detaches from the console (DETACHED_PROCESS) and breaks out of any Job the host put
/// the MCP server in (CREATE_BREAKAWAY_FROM_JOB, retried without it if not allowed), so the
/// window doesn't die with the MCP server. The environment is the **user's default environment**
/// (CreateEnvironmentBlock) rather than the one the MCP host trimmed down, the same effect as
/// `open -g` launching through LaunchServices on macOS.
pub fn spawn_detached(cmd: &mut Command) -> std::io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        cmd.spawn()
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        if let Some(env) = super::win::user_environment() {
            // pass GHOSTDESK_* settings (client name and such) through
            let keep: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
                .filter(|(k, _)| k.to_string_lossy().to_ascii_uppercase().starts_with("GHOSTDESK_"))
                .collect();
            cmd.env_clear().envs(env).envs(keep);
        }
        let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        cmd.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            // access denied (5) when the Job doesn't allow breakaway
            Err(e) if e.raw_os_error() == Some(5) => {
                cmd.creation_flags(base);
                cmd.spawn()
            }
            r => r,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        cmd.spawn()
    }
}

#[cfg(windows)]
mod win_job {
    use std::collections::HashMap;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};
    use std::sync::OnceLock;

    use parking_lot::Mutex;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, CREATE_NO_WINDOW, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
    };

    struct Job(HANDLE);
    // kernel handles are safe to move between threads
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}
    impl Drop for Job {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE: closing the last handle ends every remaining process
            unsafe { CloseHandle(self.0) };
        }
    }

    fn jobs() -> &'static Mutex<HashMap<u32, Job>> {
        static J: OnceLock<Mutex<HashMap<u32, Job>>> = OnceLock::new();
        J.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn new_job() -> Option<Job> {
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return None;
            }
            let job = Job(h);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return None;
            }
            Some(job)
        }
    }

    /// Resumes the (only) thread of a process created suspended.
    fn resume_process(pid: u32) -> bool {
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap == INVALID_HANDLE_VALUE {
                return false;
            }
            let mut te: THREADENTRY32 = std::mem::zeroed();
            te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut resumed = false;
            if Thread32First(snap, &mut te) != 0 {
                loop {
                    if te.th32OwnerProcessID == pid {
                        let t = OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32ThreadID);
                        if !t.is_null() {
                            if ResumeThread(t) != u32::MAX {
                                resumed = true;
                            }
                            CloseHandle(t);
                        }
                    }
                    if Thread32Next(snap, &mut te) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
            resumed
        }
    }

    pub fn spawn(cmd: &mut Command, extra_flags: u32) -> std::io::Result<Child> {
        cmd.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW | extra_flags);
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let job = new_job();
        let assigned = match &job {
            Some(j) => unsafe { AssignProcessToJobObject(j.0, child.as_raw_handle() as HANDLE) != 0 },
            None => false,
        };
        if !resume_process(pid) {
            let e = std::io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(e.kind(), format!("일시정지한 프로세스를 재개하지 못했습니다: {e}")));
        }
        if assigned {
            jobs().lock().insert(pid, job.unwrap());
        } else {
            eprintln!("[ghostdesk] 경고: pid {pid} 를 Job 객체에 넣지 못했습니다 (자손 정리는 본 프로세스만)");
        }
        Ok(child)
    }

    pub fn terminate(pid: u32, running: bool) {
        if let Some(job) = jobs().lock().remove(&pid) {
            if running {
                unsafe { TerminateJobObject(job.0, 1) };
            }
            // drop(job) closes the handle and cleans up remaining descendants
        }
    }

    pub fn pids(pid: u32) -> Option<Vec<u32>> {
        let map = jobs().lock();
        let job = map.get(&pid)?;
        // JOBOBJECT_BASIC_PROCESS_ID_LIST: u32 assigned count, u32 actual count, usize[] pids
        let mut buf = vec![0usize; 2 + 1024];
        unsafe {
            let hdr = buf.as_mut_ptr() as *mut u32;
            *hdr = 1024;
            if QueryInformationJobObject(
                job.0,
                JobObjectBasicProcessIdList,
                buf.as_mut_ptr() as *mut _,
                (buf.len() * std::mem::size_of::<usize>()) as u32,
                std::ptr::null_mut(),
            ) == 0
            {
                return None;
            }
            let n = *hdr.add(1) as usize;
            // the ULONG_PTR array follows the two u32 header fields (8-byte aligned on 64-bit, so from buf[1])
            let list = (buf.as_ptr() as *const u8).add(std::mem::size_of::<usize>().max(8)) as *const usize;
            Some((0..n.min(1024)).map(|i| *list.add(i) as u32).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    /// `kill_group` after the child spawned a grandchild must end the grandchild too.
    #[cfg(windows)]
    #[test]
    fn kill_group_kills_grandchildren() {
        // cmd /c → a long-running ping (the grandchild)
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/c", "ping -n 30 127.0.0.1 >NUL"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = spawn_group(&mut cmd).expect("spawn");
        let pid = child.id();
        // wait for the grandchild to appear
        let start = Instant::now();
        let mut pids = vec![];
        while start.elapsed() < Duration::from_secs(10) {
            pids = group_pids(pid).unwrap_or_default();
            if pids.len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(pids.len() >= 2, "Job 에 손자가 들어가야 함: {pids:?}");
        assert!(peek_exit(&child).is_none());
        kill_group(&mut child);
        std::thread::sleep(Duration::from_millis(300));
        for p in pids {
            assert!(!pid_alive(p), "pid {p} 가 남아 있음");
        }
    }

    #[cfg(windows)]
    #[test]
    fn peek_exit_does_not_reap() {
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/c", "exit 3"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = spawn_group(&mut cmd).unwrap();
        let start = Instant::now();
        while peek_exit(&child).is_none() && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(peek_exit(&child), Some(3));
        assert_eq!(child.wait().unwrap().code(), Some(3));
    }
}
