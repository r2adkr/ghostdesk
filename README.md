# 👻 GhostDesk

AI 에이전트가 **지금 보고 계신 화면이 아니라 보이지 않는 별도 공간**에서 웹사이트와 앱을 조작하게 해 주는
Rust + egui 데스크톱 앱입니다 (**macOS · Windows 10/11**). 마우스·키보드를 빼앗지 않고, 에이전트가 하는 일은 GhostDesk 창에서
**가상 마우스**(에이전트별 색 화살표, 미끄러지는 이동, 클릭 파문, 입력 말풍선)로 실시간으로 지켜볼 수 있습니다.

> Windows 에서의 차이(지원하는 화면 종류, 설치·설정 파일 위치, 빌드 방법)는 아래 **[Windows](#-windows)** 절에 모아 두었습니다.

## Download and run (Windows)

1. Download [GhostDesk v0.2.0 for Windows x64](https://github.com/r2adkr/ghostdesk/releases/download/v0.2.0/GhostDesk-0.2.0-windows-x64.exe) and double-click it. No installer, Rust, or separate runtime installation is required. Later versions show an **update notice** in the sidebar; click **Update**, then **Restart to update**.
2. **Home (홈)** opens first. Enter a task to configure a new agent. **All agents (모든 에이전트)** shows the agents' actual live screens; click a screen to open its details.
3. Use your own API key (Claude, OpenAI, Gemini, OpenRouter, … or a local Ollama) for the built-in agent and pick the model under the home composer, or connect an external agent through MCP. GhostDesk automatically installs a stable copy and registers its MCP server for supported clients; clients may need a restart to load the connection.

The UI is available in Korean, English, Japanese, and Chinese (Settings → Language). Windows 10/11 x64 is supported; a supported Chromium browser such as Edge is required for web workspaces. The executable is unsigned, so Windows may show a security warning. Windows child sessions remain experimental, require an administrator account, and have not been verified with real capture/input.

## 두 가지 사용법

| | 여기서 바로 실행 | 다른 앱의 에이전트가 사용 (MCP) |
|---|---|---|
| 누가 일하나 | GhostDesk 안의 Claude 에이전트 | Claude 앱 · Claude Code · Codex · Kimi Code · OpenCode · pi 등 |
| 필요한 것 | Claude API 키 (⚙ 설정) | 없음 (연결만) |
| 시작 | **+ 새 에이전트** → 무엇을 조작할지 고르고 할 일 입력 | 상단의 **외부 연결** → 모두 연결 (또는 설정 붙여 넣기) |

```bash
cargo build --release
./target/release/ghostdesk            # GUI
./target/release/ghostdesk --mcp      # 외부 에이전트용 MCP 서버 (stdio, GUI 를 자동으로 띄움)
```

빌드에는 **Rust 1.95 이상**이 필요합니다. rustup 을 사용하면 `rust-toolchain.toml` 에 지정된 1.95.0 을 자동으로 설치·선택합니다.

## 에이전트가 일하는 "보이지 않는 화면" 종류 (macOS)

| 화면 | 방식 | 로그인 상태 | 사용자 화면 영향 |
|---|---|---|---|
| 🌐 웹사이트 | Chrome/Edge/Brave/Whale 프로필을 복제한 headless 브라우저, CDP 로만 입력 | 원하면 내 로그인 복제 | 없음 |
| 🍎 Mac 앱 — Electron (VS Code·Antigravity·OpenCode 등, ⭐ 표시) | 별도 인스턴스를 숨김 실행(메인 프로세스 주입: 투명·클릭 통과·포커스 없음·Dock 없음), CDP 로만 입력 | 원하면 앱 데이터 복제 | 없음 (검증: 커서·포커스 변화 0, 보이는 창 0) |
| 🍎 Mac 앱 — 그 밖의 앱 (TextEdit·계산기·카카오톡 등) | 숨긴 창을 ScreenCaptureKit 으로 캡처, 클릭은 접근성(AX) 동작으로, 입력은 해당 프로세스에만 전달 | 같은 앱 (실행 중인 내 앱을 쓰려면 동의 필요 — 작업 중 숨겨짐) | 없음 (단, 캡처 중 메뉴 막대 화면 기록 표시) |
| 🧪 Mac 앱 — 가상 디스플레이 (실험, 동의 필요) | 보이지 않는 가상 모니터를 잠시 추가하고 앱 창을 그 위에 띄움 → 실제 픽셀 클릭·드래그·우클릭, 입력은 해당 프로세스에만 | 같은 앱 | 가상 모니터가 배치 가장 바깥에 추가됨(커서가 넘어갈 수 있음), 끝나면 제거 |
| 🧑‍💻 별도 계정 세션 (RDP 방식) | 전용 macOS 계정의 백그라운드 로그인 세션 전체를 세션 헬퍼로 조작 | 공유 안 됨 (그 계정에서 따로 로그인) | 없음 |
| 🖥 원격 컴퓨터 | VNC(RFB) 접속 — 에이전트 전용 Mac·VM·Linux | 대상 컴퓨터 것 | 없음 (이 Mac 자신의 화면 공유에는 접속 거부) |

### 연속 작업
- 마우스가 순간이동하지 않고 실제 경로를 따라 미끄러지며 이동합니다(실제 mouseMoved 연속 전송 + 화면 표시 동기화).
- 상세 화면의 **다음 작업으로** 로 작업을 쌓아 두면, 지금 작업이 끝나는 즉시 멈추지 않고 이어서 실행합니다.
- 고급 설정의 **최대 단계 → 무제한** 이면 단계 제한 없이 계속 진행합니다.
- **직접 조작** 으로 언제든 사람이 넘겨받아(로그인·2단계 인증 등) 도울 수 있습니다.
- 상단의 **화면 모아보기** 는 목록·패널 없이 모든 에이전트의 화면만 창 가득 크게 모아 보여 줍니다 (F11 — macOS 는 ⌃⌘F — 전체 화면, Esc 닫기). 질문을 기다리거나 오류가 난 에이전트는 노랑·빨강 테두리로 표시됩니다.

## 외부 에이전트 연결 (Claude 앱 / Claude Code / Codex / Kimi Code / OpenCode / pi) — 자동

GhostDesk 를 켜면 자동으로 `~/Applications/GhostDesk.app` 에 설치되고, 이 Mac 에 있는 **Claude Code · Claude 앱 · Codex · Kimi Code · OpenCode · pi 에 MCP 서버로 등록**됩니다.
그다음부터는 그 에이전트에게 **"GhostDesk 로 …해줘"** 라고만 말하면 됩니다 (Claude 앱은 한 번 재시작 필요).

- 등록 방법: Claude Code 는 `claude mcp add --scope user`, Claude 앱은 `claude_desktop_config.json`, Codex 는 `~/.codex/config.toml`, Kimi Code 는 `~/.kimi-code/mcp.json`, OpenCode 는 `~/.config/opencode/opencode.json`(`opencode.jsonc` 만 있으면 그 파일, 주석이 있으면 고치지 않음), pi 는 `~/.pi/agent/mcp.json` (서식·주석·다른 서버 보존, 고치기 전 `.ghostdesk-backup` 백업)
- **pi 는 MCP 를 기본 지원하지 않습니다.** 등록 뒤 한 번 `pi install npm:pi-mcp-adapter` 로 MCP 확장을 설치하고 pi 를 다시 시작하세요. 이 확장은 쉬는 서버를 끊는데 연결이 끊기면 GhostDesk 화면도 닫히므로, 항목에 `"lifecycle": "lazy-keep-alive"` 를 넣어 둡니다.
- 상태 확인·해제·자동 연결 끄기: 상단의 **외부 연결** 창
- 터미널: `ghostdesk --connect` / `ghostdesk --disconnect`
- **목록에 없는 AI 앱** (Cursor · VS Code · Gemini CLI · Windsurf · Cline · Zed 등 MCP 를 지원하는 앱): **외부 연결** 창의 **다른 AI 앱** 에서
  - **MCP 설정 복사** → 그 앱의 MCP 서버 설정에 붙여 넣기 (표준 `mcpServers` 형식)
  - **연결 패키지 만들기** → 내려받기 폴더에 `GhostDesk 연결 패키지` 를 만든다: `ghostdesk.mcp.json`(MCP 설정) · `skills/ghostdesk/SKILL.md`(스킬) · `claude-plugin/`(MCP 설정 + 스킬을 묶은 Claude 형식 플러그인)
  - 터미널: `ghostdesk --export-kit [폴더]`

직접 설정하려면 (고급):

```bash
claude mcp add ghostdesk -e GHOSTDESK_CLIENT_NAME="Claude Code" -- "/Applications/GhostDesk.app/Contents/MacOS/ghostdesk" --mcp
```

```json
"ghostdesk": { "command": "/Applications/GhostDesk.app/Contents/MacOS/ghostdesk", "args": ["--mcp"] }
```

```toml
[mcp_servers.ghostdesk]
command = "/Applications/GhostDesk.app/Contents/MacOS/ghostdesk"
args = ["--mcp"]
```

Kimi Code (`~/.kimi-code/mcp.json`) · pi (`~/.pi/agent/mcp.json`) 는 위 JSON 과 같은 `mcpServers` 형식이고, OpenCode 는 `mcp` 아래에 명령을 배열로 씁니다:

```json
"mcp": { "ghostdesk": { "type": "local", "command": ["/Applications/GhostDesk.app/Contents/MacOS/ghostdesk", "--mcp"], "enabled": true, "timeout": 30000 } }
```

제공 도구: `desk_open`(browser / app / session / remote), `desk_action`(computer-use 동작: 클릭·입력·스크롤… 결과로 스크린샷), `desk_browser`(주소 이동·탭), `desk_app`(앱 메뉴·접근성 요소), `desk_list`, `desk_close`.
**내 로그인 복제, 실행 중인 내 앱 사용, 원격 접속은 GhostDesk 창에서 사용자가 "허용"을 눌러야만** 진행됩니다.
외부 에이전트가 만든 화면은 그리드에 🔌 로 나타나고 가상 마우스로 보입니다. 제어 소켓은 `~/Library/Application Support/GhostDesk/control.sock`(권한 0600, 같은 사용자만)입니다.

## 외부 에이전트 보안 규칙
- 외부 에이전트가 화면을 열 때 **내 로그인 복제, Mac 앱 실행(격리되지 않음), 실행 중인 내 앱 사용, 가상 디스플레이, 원격 접속**은 모두 GhostDesk 창의 승인 창에서 허용해야 진행됩니다 (Dock 아이콘이 알림).
- 터미널·스크립트 편집기·Automator·단축어·키체인·암호·시스템 설정·Finder·활성 상태 보기·GhostDesk 자신 같은 셸·보안 계열 앱은 외부 요청으로 열 수 없습니다.
- 화면은 그것을 만든 연결만 조작할 수 있고, 연결이 끊기면 자동으로 닫힙니다. 사람이 '직접 조작'·'일시정지' 중이면 외부 에이전트 입력은 기다립니다.
- 로그인 복제 작업공간과 빈 작업공간은 이름 공간이 분리되어, 승인 없이 복제본을 다시 여는 것이 불가능합니다.
- Electron 화면에서 에이전트의 cmd+c/x/v 는 막혀 있습니다 (시스템 클립보드는 사용자 것).

## 권한 (macOS)
- 🌐 웹사이트·⭐ Electron 앱·원격·외부 연결: 추가 권한 없음
- 🍎 그 밖의 Mac 앱: 시스템 설정 → 개인정보 보호 및 보안 → **손쉬운 사용**, **화면 및 시스템 오디오 기록** 에 GhostDesk 허용
- 🧑‍💻 별도 계정 세션: 앱 안의 **📖 처음 설정 방법 보기** 참고 (전용 계정 생성, 헬퍼 LaunchAgent, 그 계정 안에서 권한 허용)

GhostDesk.app 으로 묶어 고정 위치(/Applications)에 두는 것을 권장합니다. 권한과 외부 연결 설정이 실행 파일 경로에 묶이기 때문입니다.

## 데이터
- 작업공간(복제된 쿠키·앱 데이터 포함): `~/Library/Application Support/GhostDesk/workspaces/<이름>` — 필요 없으면 지우세요.
- API 키는 저장하지 않습니다(메모리 + `ANTHROPIC_API_KEY` 환경변수).

## 테스트

```bash
cargo test                                   # 단위 테스트
./target/debug/ghostdesk --selftest          # 브라우저 백엔드 (빈 임시 프로필)
./target/debug/ghostdesk --selftest-electron # VS Code 빈 프로필 숨김 실행 + 비간섭 샘플러
./target/debug/ghostdesk --selftest-native   # 숨긴 TextEdit/계산기 + 비간섭 샘플러
./target/debug/ghostdesk --selftest-session  # 세션 헬퍼 RPC (가짜 플랫폼)
./target/debug/ghostdesk --selftest-vdisplay --yes   # ⚠ 실제 가상 모니터를 만들어 TextEdit 로 시험 (동의할 때만)
ANTHROPIC_BASE_URL=http://127.0.0.1:<가짜서버> ./target/debug/ghostdesk --agent-test "작업"   # 에이전트 루프
```

## 한계
- Electron 숨김 실행은 Node 인스펙터 퓨즈가 켜진 앱만 가능합니다(Discord·Claude·Unity Hub 불가).
- 그 밖의 Mac 앱은 숨긴 창이라 드래그·우클릭·팝업 메뉴 열기가 안 되고, Cmd 단축키는 메뉴 실행으로 대체됩니다.
- 별도 계정 세션은 실제 백그라운드 세션에서의 캡처·입력이 아직 실기 검증되지 않았습니다(가짜 플랫폼으로만 검증).
- 원격(VNC)의 macOS 화면 공유 서버는 Apple 세션 선택 단계를 구현하지 않아 에이전트 전용 Mac/VM 에만 쓰세요.
- 에이전트는 실제 계정으로 행동합니다. 메시지 전송·결제·삭제 같은 동작은 확인을 받도록 했지만, 중요한 계정에서는 지켜보면서 쓰세요.

## 🪟 Windows

Windows 10/11 에서 같은 GUI·MCP 서버가 동작합니다 (Windows 11 x64 에서 검증, ARM64 는 빌드 설정만 있고 미검증). macOS 와 다른 점만 적습니다.

### 빌드·실행

1. Rust **1.95 이상** (https://rustup.rs, 기본 `x86_64-pc-windows-msvc` 도구 모음) 와 **Visual Studio Build Tools** 의 "C++ 를 사용한 데스크톱 개발" 이 필요합니다.
2. 빌드:
   ```powershell
   cargo build --release
   .\target\release\ghostdesk.exe              # GUI
   .\target\release\ghostdesk.exe --connect    # 설치 + Claude Code · Claude 앱 · Codex · Kimi Code · OpenCode · pi 연결
   ```
- C 런타임을 정적으로 링크하므로(`.cargo/config.toml`) 다른 PC 에 `ghostdesk.exe` 하나만 복사해도 실행됩니다.
- `ghostdesk.exe` 는 MCP(stdio)·명령줄 모드 때문에 콘솔 프로그램이지만, 탐색기에서 더블클릭하면 콘솔 창 없이 GUI 만 뜹니다
  (Windows 11 24H2 이상은 실행 파일 매니페스트 `consoleAllocationPolicy=detached`, 그 이전은 콘솔 없이 스스로 다시 실행).
- 창은 사용자당 하나만 뜹니다. 다시 실행하면 이미 떠 있는 창이 앞으로 옵니다.
- 서명하지 않은 실행 파일이라 인터넷에서 받은 경우 SmartScreen 경고가 나올 수 있습니다.

### 화면 종류 (Windows)

| 화면 | 방식 | 로그인 상태 | 사용자 화면 영향 |
|---|---|---|---|
| 🌐 웹사이트 | Chrome·Edge(Beta/Dev/Canary 포함)·Brave·Whale·Vivaldi·Chromium 프로필을 복제한 headless 브라우저, CDP 로만 입력 | 원하면 내 로그인 복제 (**원본 브라우저를 완전히 종료한 뒤** — 실행 중이면 쿠키 파일이 잠겨 복제가 실패하고, 잡고 있는 프로그램 이름을 알려 줍니다) | 없음 |
| ⭐ Windows 앱 — Electron (VS Code·Cursor 등) | 별도 인스턴스를 화면 밖·작업 표시줄 없음·포커스 없음으로 숨김 실행, CDP 로만 입력. 앱 안의 끌어서 놓기는 페이지 안에서 끝나 사용자 화면으로 나가지 않음. 앱이 띄운 콘솔 창은 숨기고, 그 밖의 창이 화면에 나타나면 감시자가 즉시 종료 | 원하면 앱 데이터 복제 (앱을 종료한 뒤) | 없음 |
| 🪟 Windows 앱 — 그 밖의 Win32 앱 (카카오톡·Character Map 등) | 창을 화면 밖에 두고 PrintWindow 로 캡처, 클릭·입력은 그 창에만 메시지로 전달, UI Automation 으로 요소 조작, Ctrl 단축키는 메뉴 가속키로 실행. 새로 띄운 앱은 Job 객체 제한으로 시스템 클립보드를 아예 읽고 쓸 수 없음 | 같은 앱 (실행 중인 내 앱을 쓰려면 동의 필요 — 작업 중 화면 밖으로 옮겼다가 끝나면 되돌림) | 없음 |
| 🖥 원격 컴퓨터 | VNC(RFB) 접속 — 에이전트 전용 PC·VM | 대상 컴퓨터 것 | 없음 (이 PC 자신의 VNC 서버에는 '에이전트 전용' 확인이 있어야 접속하고, 외부 에이전트 요청이면 승인 창에 '이 PC 자신' 경고가 나옴. 실제로 연결된 주소로 다시 확인) |

- **자식 세션(PiP)**: 같은 PC·같은 계정의 별도 Windows 화면 전체를 조작하는 모드입니다. 새 에이전트에서 자식 세션을 선택하고, 처음에는 '자식 세션 켜기'의 관리자 승인(UAC)이 필요합니다. 시스템 전체에서 연결된 자식 세션은 동시에 1개입니다. 기존 앱 설치·파일을 사용하지만, 브라우저 프로필 동시 사용이나 모든 앱의 로그인 유지는 보장하지 않습니다.
- 현재 자식 세션 헬퍼는 HKCU `RunOnce`로 시작하므로 **관리자 계정(일반 권한으로 실행하는 split-token 계정 포함)**이 필요합니다. 표준 계정은 다른 관리자에게 UAC 승인을 받아 기능을 켜도 헬퍼가 시작되지 않습니다 ([Microsoft 문서](https://learn.microsoft.com/en-us/troubleshoot/windows-client/shell-experience/standard-user-cannot-run-commnad-via-runonce)). 실제 자식 세션의 화면 캡처·입력은 아직 실기 검증되지 않았습니다.
- **가상 디스플레이(🧪)**는 macOS 전용입니다. Windows에서 별도 머신이 필요하면 Hyper-V VM 등의 VNC 서버에 🖥 원격 컴퓨터로 접속할 수 있습니다.
- Windows 앱 모드에서 안 되는 것: Microsoft Store(UWP/MSIX) 앱 — Windows 11 의 새 메모장·계산기·그림판 포함 —, 콘솔 프로그램, 관리자 권한이 필요한 앱,
  탐색기·제어판·작업 관리자 같은 셸·시스템 도구, 우클릭, 콤보 상자·날짜 선택 펼치기(F4 포함 — `app_control` 의 choose 사용), 스크롤 막대·슬라이더 밖의 드래그,
  메뉴 가속키가 없는 Ctrl 단축키, 모달 대화 상자가 열려 있는 동안 뒤 창의 메뉴·단축키. 복사·붙여넣기는 GhostDesk 내부 클립보드로 처리되어 시스템 클립보드를 건드리지 않습니다.
- '실행 중인 내 앱에 연결'(동의 필요)로 쓴 창은 끝나면 원래 자리·상태(스냅 배치, '복원하면 최대화' 포함)로 돌아갑니다. GhostDesk 가 비정상 종료되면 다음 실행 때 되돌립니다.
- Electron 숨김 실행은 Node 인스펙터 퓨즈가 켜진 앱만 가능합니다(Discord 불가). Microsoft Store 로 설치한 Electron 앱(Skype·Claude 앱 등)도 불가.
- 단축키는 Ctrl 기반입니다. 에이전트가 `cmd+…` 를 보내면 로컬 화면(웹사이트·Electron·Windows 앱)에서는 `ctrl+…` 로 바뀝니다 (원격 VNC 는 그대로).
  Electron·Windows 앱에서 메뉴를 여는 키(Alt 단독, F10, Shift+F10, 다른 수정키 없는 Alt+글자, Alt+Space, 메뉴 키)는 막혀 있으니 `desk_app` 의 메뉴 기능을 쓰세요.
  Shift+Alt+F 같은 조합은 그대로 앱에 전달됩니다.
- 웹사이트 화면에서 `file://서버/공유` 나 `\\서버\공유` 같은 네트워크 경로는 열지 않습니다 (Windows 자격 증명이 그 서버로 전송되지 않게). 외부 요청으로 여는 앱 경로도 마찬가지입니다.

### 현재 계정 로그인 사용 · 숨은 화면 (Windows)

**+ 새 에이전트 → Windows 앱 → 현재 계정 로그인 사용 · 숨은 화면**을 켜고 앱을 고르세요.
같은 Windows 계정으로 숨은 데스크톱에 앱을 새로 실행해 앱의 **원본 프로필·저장된 로그인**을 사용합니다.
Electron 앱도 이 모드를 명시하면 데이터 복제/CDP 대신 Windows 앱 백엔드로 실행합니다.

- **실제 계정 데이터에 반영됩니다.** 로그인 정보를 별도 작업공간에 복사하거나 비밀번호를 추출하지 않습니다.
- 같은 앱이 이미 실행 중이면 시작하지 않습니다. 창뿐 아니라 트레이까지 직접 종료하고 다시 시도하세요. 기존 앱을 자동 종료하거나 그 창에 붙지 않습니다.
- 숨은 데스크톱에서 실행하지 못하면 오류를 알립니다. 화면 밖 창으로 자동 전환하지 않습니다.
- **모든 앱·세션의 로그인 공유를 보장하지 않습니다.** Store/UWP·관리자 권한·보호 화면·일부 GPU 앱은 지원하지 않으며, 다른 Windows 사용자의 세션이나 메모리에만 있던 로그인은 가져오지 않습니다. 앱이 재인증을 요구할 수 있습니다.

MCP에서도 `desk_open`에 다음 인자를 사용합니다 (GhostDesk의 원본 데이터 사용 승인 필요):

```json
{"mode":"app_current_session", "app":"Visual Studio Code"}
```

기존 외부 요청의 앱 차단·승인 규칙은 그대로 적용됩니다.

MCP `desk_open`은 `mode` 하나로 화면 종류를 고릅니다: `web` · `web_login` · `app` · `app_login` · `app_current_session` · `app_running` · `session`(자식 세션) · `remote` (macOS는 `app_current_session` 대신 `app_vdisplay`). 외부 에이전트는 작업 전에 사용자에게 어떤 모드로 할지 물어봅니다.

### 외부 에이전트 연결 (Windows)

GhostDesk 를 켜면(또는 `ghostdesk.exe --connect`) `%LOCALAPPDATA%\Programs\GhostDesk\ghostdesk.exe` 로 설치하고 다음 위치에 등록합니다.

| 클라이언트 | 등록 위치 |
|---|---|
| Claude Code | `claude mcp add --scope user` (CLI 가 없거나 실패하면 `%USERPROFILE%\.claude.json` 직접 수정, `CLAUDE_CONFIG_DIR` 지원) |
| Claude 앱 | Microsoft Store 판: `%LOCALAPPDATA%\Claude-Data\claude_<id>\claude_desktop_config.json`, 설치 프로그램 판: `%APPDATA%\Claude\claude_desktop_config.json` — **트레이 아이콘에서 완전히 종료한 뒤** 다시 실행해야 적용 |
| Codex | `%CODEX_HOME%\config.toml` 또는 `%USERPROFILE%\.codex\config.toml` |
| Kimi Code | `%KIMI_CODE_HOME%\mcp.json` 또는 `%USERPROFILE%\.kimi-code\mcp.json` |
| OpenCode | `%XDG_CONFIG_HOME%\opencode\opencode.json` 또는 `%USERPROFILE%\.config\opencode\opencode.json` (`opencode.jsonc` 만 있으면 그 파일) |
| pi | `%PI_CODING_AGENT_DIR%\mcp.json` 또는 `%USERPROFILE%\.pi\agent\mcp.json` — `pi install npm:pi-mcp-adapter` 필요 |

- 실행 중인 `ghostdesk.exe` 위에 새 버전을 설치하면 기존 파일을 `ghostdesk.exe.old-*` 로 밀어 두고 바꿉니다(다음 설치 때 정리).
- 제어 연결은 유닉스 소켓 대신 이름 있는 파이프 `\\.\pipe\GhostDesk-control-<사용자 SID>` 입니다 (같은 사용자만 접근하도록 ACL 설정, 원격 접속 거부, 양쪽에서 상대 사용자 확인,
  낮은 무결성 수준(샌드박스) 프로세스가 같은 이름의 파이프를 먼저 만들어 가로채는 것도 거부).
- `ghostdesk.exe` 는 DLL 을 시스템 폴더에서만 읽습니다 (다운로드 폴더 등에 놓인 같은 이름의 DLL 을 불러오지 않게).
- PowerShell 에서 npm 의 `claude.ps1` 은 `--` 를 삼키므로 직접 등록할 때는 `claude.cmd` 나 `claude.exe` 를 쓰세요:
  ```powershell
  claude.cmd mcp add --scope user ghostdesk -e "GHOSTDESK_CLIENT_NAME=Claude Code" -- "$env:LOCALAPPDATA\Programs\GhostDesk\ghostdesk.exe" --mcp
  ```
- 외부 요청으로 열 수 없는 앱(Windows): 명령 프롬프트·PowerShell·터미널·WSL, 스크립트 호스트와 악용 가능한 시스템 도구(mshta·rundll32·regsvr32·certutil 등),
  레지스트리 편집기·제어판·설정·서비스·작업 관리자·탐색기, 원격 지원 도구, 암호 관리자, GhostDesk 자신.

### 데이터 (Windows)
- 작업공간·설정: `%LOCALAPPDATA%\GhostDesk\` (`workspaces\<이름>`, `settings.json`). 로밍 프로필(%APPDATA%)에는 두지 않습니다.
- 일반 브라우저·앱 모드에는 별도 권한 요청이 없습니다. 자식 세션 기능을 처음 켤 때는 관리자 승인(UAC)이 필요합니다.

### 테스트 (Windows)

```powershell
cargo test
.\target\debug\ghostdesk.exe --selftest --browser=edge     # 브라우저 (빈 임시 프로필, 이름 또는 실행 파일 경로)
.\target\debug\ghostdesk.exe --selftest-native             # 화면 밖 문자표 + 내장 시험 창, 커서·포커스·클립보드 비간섭 확인
.\target\debug\ghostdesk.exe --selftest-session            # 자식 세션 상태 조회 + mock 헬퍼 명명 파이프 왕복 (설정 변경·실제 입력 없음)
.\target\debug\ghostdesk.exe --selftest-electron           # VS Code 빈 프로필 숨김 실행 (VS Code 필요)
```

## 구조
| 파일 | 역할 |
|---|---|
| `src/app.rs`, `src/app/theme.rs` | egui UI (처음 화면, 새 에이전트 창, 그리드/상세, 화면 모아보기, 설정, 외부 연결, 승인 창) · 테마(색·간격·공통 위젯) |
| `src/cursor.rs` | 가상 마우스 (글라이드 이동, 궤적, 클릭·드래그·스크롤·입력 효과) |
| `src/agent.rs` | 에이전트 루프, 작업 대기열, 화면 준비 |
| `src/desk.rs` | 화면 백엔드 공통 인터페이스 |
| `src/browser.rs`, `src/cdp.rs` | 헤드리스 브라우저 (CDP) |
| `src/electron.rs` | Electron 앱 숨김 복제 인스턴스 |
| `src/native.rs`, `src/macos/*` | 숨긴 macOS 앱 (SCK 캡처, AX, 프로세스 단위 입력) |
| `src/winapp/*` | Windows 앱 (화면 밖 창, PrintWindow 캡처, 메시지 입력, UI Automation, 메뉴 가속키, 감시자) |
| `src/platform/*` | 운영체제 차이 흡수 (경로, 프로세스 그룹/Job, 제어 IPC 소켓/파이프, 단축키 변환, 기능 지원 여부, Win32 도우미) |
| `src/session.rs`, `src/winapp/child.rs` | macOS 전용 계정·Windows 자식 세션의 헬퍼 RPC, 화면 캡처·입력, 명명 파이프/소켓 연결 |
| `src/vdisplay.rs` | 실험: CGVirtualDisplay 가상 모니터 위 앱 조작 |
| `src/install.rs` | ~/Applications (Windows: %LOCALAPPDATA%\Programs\GhostDesk) 설치 + Claude Code·Claude 앱·Codex·Kimi Code·OpenCode·pi 자동 연결 |
| `src/update.rs` | GitHub 최신 릴리스 확인 → 사이드바 새 버전 알림 → (Windows x64) 내려받아 SHA256 확인 후 실행 중인 exe 와 바꿔치기 · 다시 시작 |
| `.github/workflows/release.yml` | `v*` 태그를 올리면 Windows x64 exe 와 SHA256SUMS.txt 를 빌드해 릴리스로 올림 (설명은 태그 메시지) |
| `build.rs`, `windows/ghostdesk.exe.manifest` | Windows 실행 파일 매니페스트 (탐색기 실행 시 콘솔 없음) |
| `src/rfb.rs` | VNC(RFB) 클라이언트 |
| `src/control.rs`, `src/mcp.rs` | 외부 에이전트용 제어 소켓과 MCP 서버 |
| `src/kit.rs` | 목록에 없는 에이전트용 연결 패키지 (MCP 설정 · 스킬 · 플러그인, 스킬 내용은 `mcp.rs` 의 안내문·도구 목록에서 만든다) |
| `src/llm.rs` | Claude Messages API (computer_toolset_20260801) |
