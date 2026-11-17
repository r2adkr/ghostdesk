//! Keeps the computer from sleeping while agents work. The display may still turn off or lock.
//!
//! A locked screen doesn't stop anything by itself: hidden browsers, apps and remote screens run
//! independently of the user's display. What does stop them is (1) idle system sleep and (2)
//! App Nap throttling background apps on macOS.
//!
//! - macOS: `NSProcessInfo beginActivity(UserInitiated)` prevents both sleep and App Nap while
//!   still allowing display sleep and locking.
//! - Windows: `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` prevents sleep only.
//!   The state is tied to the calling thread, so it is created and dropped on the GUI thread,
//!   which lives for the whole session.
//!
//! Sleep is blocked for as long as the value is held; dropping it restores the default.

pub struct KeepAwake {
    #[cfg(target_os = "macos")]
    activity: objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>,
}

impl KeepAwake {
    /// Starts blocking sleep. `reason` is what macOS shows in `pmset -g assertions` and similar.
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
