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
