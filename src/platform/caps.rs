//! Which kinds of hidden screens this OS supports.
//!
//! Read by the UI (new-agent dialog), the control server (external `desk_open`, checked
//! **before** asking the user for approval), the MCP tool descriptions and the agent's system
//! prompt.

/// 🌐 Headless browser (CDP). Any OS with a Chromium-based browser installed.
pub const BROWSER: bool = true;

/// 🖥 Remote computer (VNC/RFB). Plain TCP.
pub const REMOTE: bool = true;

/// ⭐ Hidden clone of an Electron app (CDP + Node inspector).
pub const ELECTRON: bool = cfg!(any(target_os = "macos", windows));

/// Other local apps (macOS: ScreenCaptureKit + Accessibility; Windows: off-screen window +
/// PrintWindow + window messages).
pub const NATIVE_APPS: bool = cfg!(any(target_os = "macos", windows));

/// 🧪 Virtual display (CGVirtualDisplay). macOS only.
pub const VDISPLAY: bool = cfg!(target_os = "macos");

/// 🧑‍💻 Separate session. macOS: a dedicated account (session helper + LaunchAgent);
/// Windows: a child session (PiP) of the same account.
pub const SESSION: bool = cfg!(any(target_os = "macos", windows));
