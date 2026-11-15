//! 자식 프로세스 관리: 자손까지 한 번에 정리하기, 콘솔 창 숨기기, GUI 를 분리해서 띄우기.
//!
//! - Unix: 새 프로세스 그룹(pgid == pid)으로 띄우고 `killpg` 로 정리한다 (기존 동작 그대로).
//! - Windows: 프로세스 그룹 대신 **Job 객체**를 쓴다. 일시정지 상태로 띄워 Job 에 넣은 뒤 재개하므로
//!   자식이 만드는 손자 프로세스(Chromium 렌더러·GPU 헬퍼, Electron 헬퍼 …)도 빠짐없이 같은 Job 에 들어간다.
//!   Job 은 `KILL_ON_JOB_CLOSE` 로 만들어 GhostDesk 가 비정상 종료되어도 자손이 남지 않는다.

use std::process::{Child, Command};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// 콘솔 프로그램(cmd·claude.cmd·taskkill …)을 띄울 때 검은 콘솔 창이 깜빡이지 않게 한다.
pub fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    cmd
}

/// 자손까지 함께 정리할 수 있게 띄운다. `kill_group` 으로 정리한다.
/// (Windows 에서는 creation_flags 를 덮어쓰므로, 다른 플래그가 필요하면 `spawn_group_with_flags` 를 쓴다.)
pub fn spawn_group(cmd: &mut Command) -> std::io::Result<Child> {
    spawn_group_with_flags(cmd, 0)
}

/// `spawn_group` 과 같되 Windows 생성 플래그를 더한다 (Unix 에서는 무시).
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

/// `spawn_group` 으로 띄운 자식을 자손까지 강제 종료하고 회수한다.
/// 이미 회수한 자식이면(pid 재사용 위험) 그룹 신호는 보내지 않는다.
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
        // 본 프로세스가 이미 끝났어도 Job 에 남은 자손(헬퍼)은 Job 을 닫으며 정리한다
        win_job::terminate(child.id(), running);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// 그룹(Windows: Job)에 속한 살아 있는 프로세스 pid 목록. Windows 에서만 Some.
/// Unix 에서는 호출하는 쪽이 pgid·부모 관계로 직접 찾는다.
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

/// 자식을 회수하지 않고 끝났는지만 본다 (Windows: WaitForSingleObject(…, 0)).
/// 끝났으면 Some(종료 코드).
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

/// 실행 중인 프로세스인가 (pid 기준, 대략적인 확인)
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
                // 접근 거부면 (다른 사용자·보호 프로세스) 살아 있는 것으로 본다
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

/// 부모와 분리된 GUI 프로세스로 띄운다 (MCP 서버가 GhostDesk 창을 띄울 때).
///
/// Windows: 콘솔과 분리(DETACHED_PROCESS)하고, 호스트가 MCP 서버를 Job 에 넣어 두었더라도
/// 창이 MCP 서버와 함께 죽지 않게 Job 에서 빠져나온다(CREATE_BREAKAWAY_FROM_JOB — 허용되지 않으면 없이 재시도).
/// 환경변수는 MCP 호스트가 줄여 놓은 것이 아니라 **사용자의 기본 환경**(CreateEnvironmentBlock)을 쓴다:
/// macOS 의 `open -g` 가 LaunchServices 환경으로 띄우는 것과 같은 효과다.
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
            // GHOSTDESK_* 설정(클라이언트 이름 등)은 그대로 넘긴다
            let keep: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
                .filter(|(k, _)| k.to_string_lossy().to_ascii_uppercase().starts_with("GHOSTDESK_"))
                .collect();
            cmd.env_clear().envs(env).envs(keep);
        }
        let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        cmd.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            // Job 이 breakaway 를 허용하지 않으면 접근 거부(5)
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
    // 커널 핸들은 스레드 사이에서 옮겨도 안전하다
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}
    impl Drop for Job {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE: 마지막 핸들이 닫히면 남은 프로세스가 모두 끝난다
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

    /// 일시정지 상태로 만든 새 프로세스의 (유일한) 스레드를 재개한다
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
            // drop(job) → 핸들을 닫아 남은 자손도 정리
        }
    }

    pub fn pids(pid: u32) -> Option<Vec<u32>> {
        let map = jobs().lock();
        let job = map.get(&pid)?;
        // JOBOBJECT_BASIC_PROCESS_ID_LIST: u32 할당 수, u32 실제 수, usize[] pid
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
            // 헤더 두 u32 다음에 ULONG_PTR 배열이 온다 (64비트에서는 8바이트 정렬 = buf[1] 부터)
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

    /// 자식이 손자를 띄운 뒤 kill_group 하면 손자까지 끝나야 한다
    #[cfg(windows)]
    #[test]
    fn kill_group_kills_grandchildren() {
        // cmd /c → ping (손자) 을 오래 돌린다
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/c", "ping -n 30 127.0.0.1 >NUL"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = spawn_group(&mut cmd).expect("spawn");
        let pid = child.id();
        // 손자가 생길 때까지 기다린다
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
