//! 에이전트가 일하는 동안 컴퓨터가 잠자기에 들어가지 않게 한다 (화면은 꺼지거나 잠겨도 된다).
//!
//! 화면이 잠기는 것 자체는 작업을 멈추지 않는다: 숨긴 브라우저·앱·원격 화면은 사용자 화면과 따로 돈다.
//! 멈추게 하는 것은 (1) 일정 시간 뒤의 시스템 잠자기와 (2) macOS 가 뒤에 있는 앱을 느리게 하는 App Nap 이다.
//! - macOS: `NSProcessInfo beginActivity(UserInitiated)` — 잠자기와 App Nap 을 함께 막고, 디스플레이 잠자기·잠금은 허용
//! - Windows: `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` — 잠자기만 막고 화면 끄기·잠금은 허용
//!   (부른 스레드에 묶이므로 늘 살아 있는 GUI 스레드에서 만들고 없앤다)
//!
//! 값을 들고 있는 동안만 막고, drop 하면 원래대로 돌아간다.

pub struct KeepAwake {
    #[cfg(target_os = "macos")]
    activity: objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>,
}

impl KeepAwake {
    /// 잠자기를 막기 시작한다. `reason` 은 macOS 의 `pmset -g assertions` 등에 보이는 설명.
    pub fn start(reason: &str) -> Self {
        #[cfg(target_os = "macos")]
        {
            use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
            let activity = NSProcessInfo::processInfo().beginActivityWithOptions_reason(NSActivityOptions::UserInitiated, &NSString::from_str(reason));
            KeepAwake { activity }
        }
        #[cfg(windows)]
        {
            let _ = reason;
            use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};
            unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
            KeepAwake {}
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = reason;
            KeepAwake {}
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            objc2_foundation::NSProcessInfo::processInfo().endActivity(&self.activity);
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        }
    }
}
