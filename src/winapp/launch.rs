//! 앱 실행·연결·종료: 숨김 상태로 CreateProcessW + Job 객체, 실행 전 거부 규칙(스토어 앱·실행 별칭·콘솔·관리자 권한).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use windows_sys::Win32::Foundation::{GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, OpenProcess, ResumeThread, TerminateProcess, WaitForInputIdle, WaitForSingleObject,
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, STARTUPINFOW,
};

use super::w32::{self, Handle};
use crate::platform::paths;
use crate::platform::win as pw;

const STARTF_USESHOWWINDOW: u32 = 0x0000_0001;
/// 앱이 뜨는 동안 사용자의 마우스 커서가 "작업 중" 모양으로 바뀌지 않게
const STARTF_FORCEOFFFEEDBACK: u32 = 0x0000_0080;
const ERROR_ELEVATION_REQUIRED: u32 = 740;
const STILL_ACTIVE: u32 = 259;

/// Mode A Job 의 UI 제한: Job 안의 프로세스(앱과 자손)만 막고 사용자·GhostDesk 는 그대로다.
/// - 시스템 클립보드 읽기·쓰기 (앱 자체의 복사·붙여넣기·툴바 단추가 사용자의 클립보드를 읽거나 바꾸지 못하게)
/// - 시스템 설정(SystemParametersInfo)·화면 설정(ChangeDisplaySettings)·로그오프/종료(ExitWindows)·데스크톱 전환
/// 핸들(UILIMIT_HANDLES)·전역 아톰(UILIMIT_GLOBALATOMS) 제한은 DDE·공용 대화상자를 깨뜨릴 수 있어 쓰지 않는다.
pub const JOB_UI_LIMITS: u32 = JOB_OBJECT_UILIMIT_READCLIPBOARD
    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
    | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
    | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
    | JOB_OBJECT_UILIMIT_EXITWINDOWS
    | JOB_OBJECT_UILIMIT_DESKTOP;

const SEM_FAILCRITICALERRORS: u32 = 0x0001;
const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;
/// 숨김 실행하는 앱이 물려받을 오류 모드: 로더 오류(DLL 없음 등)·치명적 오류·충돌 대화상자를 띄우지 않는다
pub const QUIET_ERROR_MODE: u32 = SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX;

// windows-sys 기능 목록(Win32_System_Diagnostics_Debug)에 없는 함수 — Cargo.toml 을 건드리지 않으려고 직접 선언
#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn SetErrorMode(mode: u32) -> u32;
    fn GetErrorMode() -> u32;
}

/// 이 프로세스의 오류 모드 (자식 프로세스는 CreateProcess 때 이것을 물려받는다)
pub fn process_error_mode() -> u32 {
    unsafe { GetErrorMode() }
}

/// 숨김 실행 전에 한 번: 프로세스 전체 오류 모드에 [`QUIET_ERROR_MODE`] 를 더한다.
///
/// 오류 대화상자(로더의 "시스템 오류", 충돌 창)는 앱 프로세스가 아니라 csrss·WER 가 사용자 화면에 띄우므로 감시기가
/// 화면 밖으로 옮길 수 없다. CreateProcess 는 부모의 오류 모드를 물려주므로(CREATE_DEFAULT_ERROR_MODE 를 쓰지 않을 때)
/// 여기서 켜 두면 앱과 그 자손에서 그런 대화상자가 뜨지 않는다. 실패는 종료 코드와 err_exit_message 로 알린다.
fn quiet_error_mode_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        SetErrorMode(GetErrorMode() | QUIET_ERROR_MODE);
    });
}

/// 우리가 띄운(또는 붙은) 프로세스
pub struct Proc {
    pub pid: u32,
    pub process: Handle,
    /// Mode A: 앱과 자손을 담은 Job (KILL_ON_JOB_CLOSE — GhostDesk 가 죽어도 앱이 남지 않는다)
    pub job: Option<Arc<Handle>>,
}

impl Proc {
    pub fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT }
    }

    pub fn exit_code(&self) -> Option<u32> {
        let mut c = 0u32;
        if unsafe { GetExitCodeProcess(self.process.0, &mut c) } == 0 || c == STILL_ACTIVE {
            return None;
        }
        Some(c)
    }

    /// 앱이 받은 입력을 다 처리하고 다음 입력을 기다릴 때까지 (최대 ms). 붙은 앱(권한 부족)이면 바로 돌아온다.
    pub fn wait_idle(&self, ms: u32) {
        unsafe {
            WaitForInputIdle(self.process.0, ms);
        }
    }

    /// 끝날 때까지 최대 ms 기다린다. 끝났으면 true.
    pub fn wait_exit(&self, ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.process.0, ms) == WAIT_OBJECT_0 }
    }

    /// 앱 프로세스 목록 (Job 이 있으면 Job 의 모든 프로세스)
    pub fn pids(&self) -> Vec<u32> {
        match &self.job {
            Some(j) => {
                let v = job_pids(j.0);
                if v.is_empty() && self.alive() {
                    vec![self.pid]
                } else {
                    v
                }
            }
            None => {
                if self.alive() {
                    vec![self.pid]
                } else {
                    vec![]
                }
            }
        }
    }

    /// Job 째로 강제 종료 (Mode A 전용)
    pub fn terminate(&self) {
        match &self.job {
            Some(j) => unsafe {
                TerminateJobObject(j.0, 1);
            },
            None => unsafe {
                TerminateProcess(self.process.0, 1);
            },
        }
    }
}

pub fn job_pids(job: HANDLE) -> Vec<u32> {
    // JOBOBJECT_BASIC_PROCESS_ID_LIST: u32 할당 수, u32 실제 수, ULONG_PTR[] pid
    let mut buf = vec![0usize; 2 + 512];
    unsafe {
        let hdr = buf.as_mut_ptr() as *mut u32;
        *hdr = 512;
        if QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            buf.as_mut_ptr() as *mut _,
            (buf.len() * std::mem::size_of::<usize>()) as u32,
            std::ptr::null_mut(),
        ) == 0
        {
            return vec![];
        }
        let n = (*hdr.add(1) as usize).min(512);
        (0..n).map(|i| buf[1 + i] as u32).collect()
    }
}

fn new_job(ui_limits: u32) -> Option<Handle> {
    unsafe {
        let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if h.is_null() {
            return None;
        }
        let job = Handle(h);
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
        // 실패해도 Job(종료 시 정리)은 그대로 쓴다 — 이름 기반 차단(복사·붙여넣기 단추·메뉴)은 남아 있다
        if ui_limits == 0 {
            return Some(job);
        }
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS { UIRestrictionsClass: ui_limits };
        if SetInformationJobObject(
            job.0,
            JobObjectBasicUIRestrictions,
            &ui as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        ) == 0
        {
            eprintln!(
                "[ghostdesk] 경고: Job 에 UI 제한(클립보드·시스템 설정)을 걸지 못했습니다: {}",
                std::io::Error::last_os_error()
            );
        }
        Some(job)
    }
}

/// Job 의 UI 제한 (JOB_OBJECT_UILIMIT_*). job 이 null 이면 이 프로세스가 속한 (가장 안쪽) Job. 알 수 없으면 None.
pub fn job_ui_limits(job: HANDLE) -> Option<u32> {
    let mut ui = JOBOBJECT_BASIC_UI_RESTRICTIONS { UIRestrictionsClass: 0 };
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicUIRestrictions,
            &mut ui as *mut _ as *mut _,
            std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(ui.UIRestrictionsClass)
}

/// MSVC 명령줄 규칙에 맞게 인자 하나를 따옴표 처리한다
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"', '\n']) {
        return a.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0usize;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

pub fn command_line(exe: &Path, args: &[String], files: &[PathBuf]) -> String {
    let mut parts = vec![format!("\"{}\"", exe.display())];
    parts.extend(args.iter().map(|a| quote_arg(a)));
    parts.extend(files.iter().map(|f| quote_arg(&f.to_string_lossy())));
    parts.join(" ")
}

/// 숨김(SW_HIDE) 상태로, 일시정지한 채 새 Job 에 넣어 띄운다. 돌려준 스레드 핸들을 [`resume`] 해야 실행된다.
pub fn spawn_suspended_hidden(exe: &Path, args: &[String], files: &[PathBuf]) -> Result<(Proc, Handle)> {
    spawn_suspended(exe, args, files, None, job_ui_limits_for(exe))
}

/// 실행 파일에 맞는 Job UI 제한: Chromium 계열은 UI 제한이 하나라도 있으면 렌더러가 뜨지 않아 0
pub fn job_ui_limits_for(exe: &Path) -> u32 {
    if super::hdesk::is_chromium_family(exe) {
        0
    } else {
        JOB_UI_LIMITS
    }
}

/// 일시정지한 채 새 Job 에 넣어 띄운다. `desktop` 이 있으면 그 숨은 데스크톱에서 보통 창으로(SW_SHOWNORMAL),
/// 없으면 사용자 데스크톱에서 숨김(SW_HIDE)으로 시작한다. 돌려준 스레드 핸들을 [`resume`] 해야 실행된다.
pub fn spawn_suspended(
    exe: &Path,
    args: &[String],
    files: &[PathBuf],
    desktop: Option<&super::hdesk::HiddenDesktop>,
    ui_limits: u32,
) -> Result<(Proc, Handle)> {
    // 콘솔 서브시스템 실행 파일(자체 시험 대상만 통과)은 콘솔 창 없이
    quiet_error_mode_once();
    let console_flags = if read_head(exe, 4096).as_deref().and_then(pe_subsystem) == Some(3) { CREATE_NO_WINDOW } else { 0 };
    let mut cmd: Vec<u16> = command_line(exe, args, files).encode_utf16().chain(std::iter::once(0)).collect();
    let app = pw::wide(exe);
    // 오래된 앱은 작업 폴더가 실행 파일 폴더라고 가정하는 경우가 많다
    let cwd = exe.parent().map(pw::wide);
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESHOWWINDOW | STARTF_FORCEOFFFEEDBACK;
    si.wShowWindow = 0; // SW_HIDE: 앱의 첫 ShowWindow(및 WinMain 의 nCmdShow)가 '숨김'이 된다
    let mut desk_path = desktop.map(|d| d.startup_path());
    if let Some(p) = desk_path.as_mut() {
        // 숨은 데스크톱은 사용자에게 보이지 않으므로 평소처럼 띄운다 (화면 밖으로 옮길 필요도 없다)
        si.lpDesktop = p.as_mut_ptr();
        si.wShowWindow = 1; // SW_SHOWNORMAL
    }
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            // CREATE_DEFAULT_ERROR_MODE 는 쓰지 않는다: 위에서 켠 조용한 오류 모드를 앱이 물려받게
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | console_flags,
            std::ptr::null(),
            cwd.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null()),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        let e = unsafe { GetLastError() };
        if e == ERROR_ELEVATION_REQUIRED {
            bail!(
                "{} 은(는) 관리자 권한이 필요한 앱이라 실행하지 않았습니다 — UAC 확인 창이 사용자 화면에 뜨고, \
                 권한이 높은 앱의 창은 GhostDesk 가 메시지로 조작할 수 없습니다 (UIPI).",
                exe.display()
            );
        }
        bail!("{} 을(를) 실행하지 못했습니다: {}", exe.display(), std::io::Error::from_raw_os_error(e as i32));
    }
    let process = Handle(pi.hProcess);
    let thread = Handle(pi.hThread);
    let job = new_job(ui_limits);
    let assigned = match &job {
        Some(j) => unsafe { AssignProcessToJobObject(j.0, process.0) != 0 },
        None => false,
    };
    if !assigned {
        eprintln!("[ghostdesk] 경고: pid {} 를 Job 객체에 넣지 못했습니다 (종료 시 본 프로세스만 정리)", pi.dwProcessId);
    }
    Ok((Proc { pid: pi.dwProcessId, process, job: if assigned { job.map(Arc::new) } else { None } }, thread))
}

pub fn resume(thread: &Handle) {
    unsafe {
        ResumeThread(thread.0);
    }
}

/// 실행 중인 다른 프로세스를 (SYNCHRONIZE | 조회) 권한으로 연다 — 붙기(Mode B) 용
pub fn open_process(pid: u32) -> Option<Handle> {
    let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        None
    } else {
        Some(Handle(h))
    }
}

/// 같은 실행 파일로 실행 중인 프로세스 (사용자의 인스턴스)
pub fn running_instances(exe: &Path) -> Vec<u32> {
    let Some(name) = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()) else { return vec![] };
    let me = std::process::id();
    pw::processes()
        .into_iter()
        .filter(|(pid, _, n)| *pid != me && n.to_lowercase() == name)
        .filter(|(pid, _, _)| pw::process_path(*pid).map(|p| paths::same_path(&p, exe)).unwrap_or(false))
        .map(|(pid, _, _)| pid)
        .collect()
}

/// 프로세스가 관리자 권한(높은 무결성)으로 실행 중인가. 알 수 없으면 None.
pub fn is_elevated(pid: u32) -> Option<bool> {
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    unsafe {
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if p.is_null() {
            return None;
        }
        let p = Handle(p);
        let mut t: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(p.0, TOKEN_QUERY, &mut t) == 0 {
            // 일반 권한에서 관리자 프로세스의 토큰은 열 수 없다
            return Some(true);
        }
        let t = Handle(t);
        let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        if GetTokenInformation(t.0, TokenElevation, &mut e as *mut _ as *mut _, std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut len) == 0 {
            return None;
        }
        Some(e.TokenIsElevated != 0)
    }
}

// ───────────────────────── 실행 전 거부 규칙 ─────────────────────────

/// 스토어(패키지) 앱을 띄우는 System32 의 실행기들 (Windows 11)
const PACKAGED_STUBS: [(&str, &str); 5] = [
    ("calc.exe", "계산기"),
    ("mspaint.exe", "그림판"),
    ("pbrush.exe", "그림판"),
    ("snippingtool.exe", "캡처 도구"),
    ("write.exe", "워드패드"),
];

fn reg_dword(root: windows_sys::Win32::System::Registry::HKEY, sub: &str, value: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_DWORD};
    let (s, v) = (pw::wide(sub), pw::wide(value));
    let mut out = 0u32;
    let mut len = 4u32;
    let r = unsafe {
        RegGetValueW(root, s.as_ptr(), v.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut out as *mut u32 as *mut _, &mut len)
    };
    (r == 0).then_some(out)
}

/// Image File Execution Options 의 "실행 별칭 리디렉션" 필터에 걸린 경로인가 (Win11 의 notepad.exe 처럼
/// 실행하면 스토어 앱으로 넘어가는 실행 파일)
pub fn is_alias_redirect(exe: &Path) -> bool {
    let Some(name) = exe.file_name().map(|n| n.to_string_lossy().to_string()) else { return false };
    let base = format!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\{name}");
    for sub in pw::reg_subkeys(pw::HKEY_LOCAL_MACHINE, &base) {
        let key = format!(r"{base}\{sub}");
        let Some(fp) = pw::reg_string(pw::HKEY_LOCAL_MACHINE, &key, Some("FilterFullPath")) else { continue };
        if paths::same_path(Path::new(fp.trim()), exe) && reg_dword(pw::HKEY_LOCAL_MACHINE, &key, "AppExecutionAliasRedirect").unwrap_or(0) != 0 {
            return true;
        }
    }
    false
}

/// PE 헤더의 서브시스템 (2 = GUI, 3 = 콘솔). 실행 파일이 아니면 None.
pub fn pe_subsystem(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 0x40 || &bytes[0..2] != b"MZ" {
        return None;
    }
    let pe = u32::from_le_bytes(bytes[0x3C..0x40].try_into().ok()?) as usize;
    if bytes.len() < pe + 24 + 70 || &bytes[pe..pe + 4] != b"PE\0\0" {
        return None;
    }
    // 선택적 헤더의 Subsystem 은 PE32/PE32+ 모두 오프셋 68
    let opt = pe + 24;
    Some(u16::from_le_bytes(bytes[opt + 68..opt + 70].try_into().ok()?))
}

fn read_head(p: &Path, n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).ok()?;
    let mut buf = vec![0u8; n];
    let got = f.read(&mut buf).ok()?;
    buf.truncate(got);
    Some(buf)
}

fn system_dirs() -> Vec<PathBuf> {
    let win = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    vec![win.join("System32"), win.join("SysWOW64"), win.clone()]
}

/// 자체 시험(`--selftest-native`)이 띄우는 시험 대상: GhostDesk 자신 + 시험 환경 변수일 때만
fn is_selftest_target(exe: &Path) -> bool {
    std::env::var_os("GHOSTDESK_WINAPP_SELFTEST").is_some()
        && std::env::current_exe().map(|c| paths::same_path(&c, exe)).unwrap_or(false)
}

/// 실행하면 안 되는 대상이면 사용자에게 보여 줄 이유와 함께 오류
pub fn preflight(exe: &Path) -> Result<()> {
    let lower = exe.to_string_lossy().to_lowercase().replace('/', "\\");
    if lower.contains("\\windowsapps\\") {
        bail!(
            "{} 은(는) Microsoft Store(패키지) 앱 또는 그 실행 별칭입니다. 패키지 앱은 실행할 때 시작 화면과 창이 \
             사용자 화면 앞에 나타나 숨길 수 없어 지원하지 않습니다 — 같은 기능의 일반(Win32) 프로그램을 쓰세요.",
            exe.display()
        );
    }
    if !exe.is_file() {
        bail!("실행 파일이 없습니다: {}", exe.display());
    }
    if exe.extension().map(|e| !e.eq_ignore_ascii_case("exe")).unwrap_or(true) {
        bail!("실행 파일(.exe)만 실행할 수 있습니다: {}", exe.display());
    }
    if is_alias_redirect(exe) {
        bail!(
            "{} 은(는) 실행하면 Microsoft Store 앱으로 넘어가는 실행 별칭입니다 (Windows 11 의 메모장 등). \
             스토어 앱은 창이 사용자 화면에 나타나 숨길 수 없어 지원하지 않습니다.",
            exe.display()
        );
    }
    let name = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if let Some(l) = shell_launcher_name(exe) {
        bail!(
            "{}({l}) 은(는) Windows 셸 또는 시스템 실행기·관리 도구라 지원하지 않습니다 — 셸에 붙으면 작업 표시줄·바탕 화면이 \
             화면 밖으로 옮겨지고, 새로 띄우면 실행 중인 셸·다른 프로그램이 창을 사용자 화면에 엽니다.",
            exe.display()
        );
    }
    if system_dirs().iter().any(|d| exe.parent().map(|p| paths::same_path(p, d)).unwrap_or(false)) {
        if let Some((_, label)) = PACKAGED_STUBS.iter().find(|(n, _)| *n == name) {
            bail!(
                "{}({name}) 은(는) Microsoft Store 앱({label})을 띄우는 실행기라 지원하지 않습니다 — \
                 스토어 앱은 창이 사용자 화면에 나타나 숨길 수 없습니다.",
                exe.display()
            );
        }
    }
    if let Some(sub) = read_head(exe, 4096).as_deref().and_then(pe_subsystem) {
        if sub == 3 && !is_selftest_target(exe) {
            bail!(
                "{} 은(는) 명령줄(콘솔) 프로그램입니다. 콘솔 창은 터미널 앱이 사용자 화면에 띄울 수 있어 지원하지 않습니다.",
                exe.display()
            );
        }
    }
    Ok(())
}

/// Windows 셸·시스템 실행기([`super::apps::SHELL_LAUNCHERS`])면 그 이름. Windows 폴더 안의 그 이름이거나,
/// 다른 곳에 복사·이름을 바꾼 것이라도 VERSIONINFO 의 원래 파일 이름이 그것인 Microsoft 실행 파일이면 해당한다.
pub fn shell_launcher_name(exe: &Path) -> Option<String> {
    let launchers = &super::apps::SHELL_LAUNCHERS;
    let name = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if launchers.contains(&name.as_str()) && system_dirs().iter().any(|d| exe.parent().map(|p| paths::same_path(p, d)).unwrap_or(false)) {
        return Some(name);
    }
    let v = pw::version_info(exe)?;
    let orig = v.original_filename?.trim().to_lowercase();
    let orig = orig.strip_suffix(".mui").unwrap_or(&orig).to_string();
    let microsoft = v.company_name.map(|c| c.to_lowercase().contains("microsoft")).unwrap_or(false);
    (microsoft && launchers.contains(&orig.as_str())).then_some(orig)
}

/// Windows 셸(작업 표시줄·바탕 화면) 프로세스들 — 붙으면 안 된다
pub fn shell_pids() -> Vec<u32> {
    let mut v = Vec::new();
    for h in [w32::find_window_class("Shell_TrayWnd"), w32::shell_window()] {
        if h != 0 {
            let pid = w32::thread_pid(h).1;
            if pid != 0 && !v.contains(&pid) {
                v.push(pid);
            }
        }
    }
    v
}

/// 실행 중 프로세스가 GhostDesk 보다 높은 권한이면 조작할 수 없다 (UIPI)
pub fn check_attachable(pid: u32) -> Result<()> {
    if shell_pids().contains(&pid) {
        bail!(
            "pid {pid} 는 Windows 셸(탐색기: 작업 표시줄·바탕 화면)이라 붙지 않습니다 — 붙으면 작업 표시줄과 바탕 화면이 화면 밖으로 옮겨집니다"
        );
    }
    if is_elevated(pid) == Some(true) && is_elevated(std::process::id()) != Some(true) {
        bail!("pid {pid} 는 관리자 권한으로 실행 중이라 조작할 수 없습니다 (권한이 높은 창에는 메시지·UI Automation 이 막힘)");
    }
    Ok(())
}

pub fn display_name(exe: &Path) -> String {
    pw::version_info(exe)
        .and_then(|v| v.file_description.or(v.product_name))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| exe.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default())
}

pub fn err_exit_message(name: &str, code: Option<u32>, others: &[u32]) -> anyhow::Error {
    if !others.is_empty() {
        anyhow!(
            "{name} 이(가) 창을 열지 않고 바로 종료했습니다 — 이미 실행 중인 인스턴스(pid {})로 요청을 넘기는 단일 인스턴스 앱으로 보입니다. \
             사용자의 실행 중인 앱에서 작업하려면 attach 동의(Mode B)가 필요합니다.",
            others.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
        )
    } else {
        anyhow!(
            "{name} 이(가) 창을 열지 않고 종료했습니다{} — 다른 프로그램을 띄우고 끝나는 실행기이거나, 실행 인자가 필요한 프로그램일 수 있습니다.",
            code.map(|c| format!(" (종료 코드 {c})")).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("abc"), "abc");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg(r"C:\dir with space\"), "\"C:\\dir with space\\\\\"");
        let cl = command_line(Path::new(r"C:\x y\app.exe"), &["-n".into()], &[PathBuf::from(r"C:\d\a b.txt")]);
        assert_eq!(cl, r#""C:\x y\app.exe" -n "C:\d\a b.txt""#);
    }

    #[test]
    fn pe_header() {
        let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let gui = read_head(&PathBuf::from(&sys).join(r"System32\charmap.exe"), 4096);
        let con = read_head(&PathBuf::from(&sys).join(r"System32\cmd.exe"), 4096);
        if let (Some(g), Some(c)) = (gui, con) {
            assert_eq!(pe_subsystem(&g), Some(2));
            assert_eq!(pe_subsystem(&c), Some(3));
        }
        assert_eq!(pe_subsystem(b"not a pe"), None);
    }

    #[test]
    fn preflight_rules() {
        let sys = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()));
        // 콘솔 프로그램 거부
        assert!(preflight(&sys.join(r"System32\cmd.exe")).is_err());
        // 스토어 앱 실행기 거부
        if sys.join(r"System32\calc.exe").is_file() {
            let e = preflight(&sys.join(r"System32\calc.exe")).unwrap_err().to_string();
            assert!(e.contains("계산기"), "{e}");
        }
        // 실행 별칭 리디렉션 (이 PC 처럼 IFEO 필터가 있을 때만)
        if is_alias_redirect(&sys.join(r"System32\notepad.exe")) {
            assert!(preflight(&sys.join(r"System32\notepad.exe")).is_err());
        }
        assert!(preflight(Path::new(r"C:\Program Files\WindowsApps\x\y.exe")).is_err());
        // 일반 Win32 앱은 통과
        if sys.join(r"System32\charmap.exe").is_file() {
            preflight(&sys.join(r"System32\charmap.exe")).unwrap();
            assert_eq!(shell_launcher_name(&sys.join(r"System32\charmap.exe")), None);
        }
        // Windows 셸·시스템 실행기 거부
        for p in [r"explorer.exe", r"System32\control.exe", r"System32\mmc.exe", r"System32\rundll32.exe", r"System32\mshta.exe", r"regedit.exe"] {
            let p = sys.join(p);
            if p.is_file() {
                let e = preflight(&p).unwrap_err().to_string();
                assert!(e.contains("셸"), "{p:?}: {e}");
            }
        }
    }

    #[test]
    fn shell_launcher_copy_is_recognized() {
        // 다른 폴더로 복사하고 이름을 바꾼 explorer.exe 도 VERSIONINFO 로 알아본다
        let sys = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()));
        let src = sys.join("explorer.exe");
        if !src.is_file() || pw::version_info(&src).and_then(|v| v.original_filename).is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ghostdesk-shell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("files.exe");
        if std::fs::copy(&src, &copy).is_ok() {
            assert_eq!(shell_launcher_name(&copy).as_deref(), Some("explorer.exe"));
            assert!(preflight(&copy).is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shell_pid_is_not_attachable() {
        for pid in shell_pids() {
            let e = check_attachable(pid).unwrap_err().to_string();
            assert!(e.contains("셸"), "{e}");
        }
    }

    #[test]
    fn job_has_ui_restrictions() {
        let job = new_job(JOB_UI_LIMITS).expect("Job");
        assert_eq!(job_ui_limits(job.0), Some(JOB_UI_LIMITS));
        assert_eq!(JOB_UI_LIMITS & 0x0001, 0, "UILIMIT_HANDLES 는 쓰지 않는다");
        assert_eq!(JOB_UI_LIMITS & 0x0020, 0, "UILIMIT_GLOBALATOMS 는 쓰지 않는다");
        assert_ne!(JOB_UI_LIMITS & 0x0002, 0);
        assert_ne!(JOB_UI_LIMITS & 0x0004, 0);
    }
}
