# 사용 설명 사진

처음 켤 때 뜨는 사용 설명(`src/app/tour.rs` 의 `PAGES`)에 들어가는 사진입니다. 앱이 자기 창을 찍어 만듭니다.
화면이 바뀌면 아래처럼 다시 찍고 빌드하세요. 사진은 1280px 너비 JPEG 로 저장됩니다.

1. 작업 화면 장면의 데모 에이전트가 쓸 가짜 AI 서버를 띄웁니다 (Anthropic Messages API 처럼 답하는 서버.
   검색 페이지를 열고 → 입력 칸을 누르고 → 글을 입력한 뒤, 마지막 답은 늦게 주면 '생각 중' 상태로 찍힙니다).
2. 사용자 설정을 건드리지 않게 따로 둔 데이터 폴더(경로가 짧아야 제어 소켓을 만들 수 있음)로 찍기 모드를 켭니다:

```bash
GHOSTDESK_DATA_DIR="$HOME/.ghostdesk-tour-tmp" \
GHOSTDESK_NO_AUTOCONNECT=1 \
GHOSTDESK_TOUR_CAPTURE="$PWD/assets/tour" \
GHOSTDESK_DEMO_TASK="제주도 2박 3일 여행 일정을 찾아줘" \
ANTHROPIC_BASE_URL=http://127.0.0.1:<가짜 서버 포트> ANTHROPIC_API_KEY=test \
./target/debug/ghostdesk
rm -rf "$HOME/.ghostdesk-tour-tmp"
```

- `GHOSTDESK_NO_AUTOCONNECT=1`: 찍는 동안 설치본(~/Applications/GhostDesk.app)을 덮어쓰거나 AI 앱 설정을 바꾸지 않게 합니다.
- 장면을 모두 찍으면 앱이 스스로 닫힙니다.
- 사진 속 글은 한국어입니다 (사용 설명의 제목·설명은 고른 화면 언어로 나옵니다).
