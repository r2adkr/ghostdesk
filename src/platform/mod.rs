//! A thin layer that collects per-OS differences in one place.
//!
//! POSIX APIs (libc, Unix sockets, file modes, signals) go behind `cfg(unix)`, Apple frameworks
//! and paths behind `cfg(target_os = "macos")`, and Win32 behind `cfg(windows)`. Public
//! functions and types keep the same names on every platform where possible, so callers (the
//! UI, the agent, the control server) don't have to sprinkle `cfg` around.

pub mod awake;
pub mod caps;
pub mod fsx;
pub mod input;
pub mod ipc;
pub mod paths;
pub mod proc;
pub mod text;
#[cfg(windows)]
pub mod win;

/// Hands memory the allocator has freed but kept back to the OS (screenshots, decoded images and
/// request bodies come and go; without this the process keeps their peak as its footprint).
pub fn trim_memory() {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone means "all zones"; goal 0 means "as much as possible".
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    }
    #[cfg(all(unix, not(target_os = "macos"), target_env = "gnu"))]
    // SAFETY: malloc_trim only returns free pages to the OS.
    unsafe {
        libc::malloc_trim(0);
    }
    #[cfg(windows)]
    // SAFETY: compacts the process heap; it only coalesces free blocks.
    unsafe {
        use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapCompact};
        HeapCompact(GetProcessHeap(), 0);
    }
}
