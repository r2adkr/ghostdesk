//! 운영체제마다 다른 부분을 한곳에 모은 얇은 계층.
//!
//! 규칙: POSIX API(libc·유닉스 소켓·파일 모드·시그널)는 `cfg(unix)`, Apple 프레임워크와 경로는
//! `cfg(target_os = "macos")`, Win32 는 `cfg(windows)` 로 가른다. 공개 함수·타입은 가능한 한
//! 모든 플랫폼에서 같은 이름으로 제공해, 호출하는 쪽(UI·에이전트·제어 서버)에 cfg 가 번지지 않게 한다.

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
