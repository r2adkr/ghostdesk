# Tour screenshots

These are the images used by the first-run tour (`PAGES` in `src/app/tour.rs`). The app captures them from its own window.
When the UI changes, recapture them as shown below and rebuild. Images are saved as 1280px-wide JPEGs.

1. Start a fake AI server for the demo agent in the workspace scene (a server that answers like the Anthropic Messages API:
   open the search page → click the input box → type the text; delay the final reply so it's captured in the "thinking" state).
2. Turn on capture mode with a separate data folder so your own settings stay untouched (the path must be short enough
   for the control socket to be created):

```bash
GHOSTDESK_DATA_DIR="$HOME/.ghostdesk-tour-tmp" \
GHOSTDESK_NO_AUTOCONNECT=1 \
GHOSTDESK_TOUR_CAPTURE="$PWD/assets/tour" \
GHOSTDESK_DEMO_TASK="제주도 2박 3일 여행 일정을 찾아줘" \
ANTHROPIC_BASE_URL=http://127.0.0.1:<fake server port> ANTHROPIC_API_KEY=test \
./target/debug/ghostdesk
rm -rf "$HOME/.ghostdesk-tour-tmp"
```

- `GHOSTDESK_NO_AUTOCONNECT=1`: keeps the capture run from overwriting the installed copy (~/Applications/GhostDesk.app)
  or changing AI app settings.
- The app closes itself once every scene has been captured.
- Text inside the screenshots is Korean (tour titles and descriptions follow the chosen UI language).
