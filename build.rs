//! Windows(MSVC): 실행 파일에 매니페스트(windows/ghostdesk.exe.manifest)를 넣고, 정적 가져오기 DLL 을
//! System32 에서만 찾게 한다. 다른 OS 에서는 아무것도 하지 않는다.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && env == "msvc" {
        let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        let manifest = dir.join("windows").join("ghostdesk.exe.manifest");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
        // DLL 심기 방지: 내려받은 폴더(Downloads 등)에 놓인 version.dll 같은 가짜 DLL 을 정적 가져오기로 읽지 않게
        // 실행 파일의 정적 가져오기를 System32 에서만 찾는다 (LOAD_LIBRARY_SEARCH_SYSTEM32, Windows 10 1607+).
        // 실행 중에 LoadLibrary 로 읽는 DLL(그래픽 드라이버 등)은 main 의 SetDefaultDllDirectories 가 맡는다.
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
}
