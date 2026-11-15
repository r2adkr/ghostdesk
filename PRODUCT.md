# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

(Desktop app built with Rust + egui for macOS and Windows 10/11; recorded as `web` because it uses its own design language, not native iOS/Android conventions.)

## Users
General users, including non-developers. They hand tasks to AI agents (GhostDesk's built-in Claude agent via "+ 새 에이전트", or an external agent such as Claude app / Claude Code / Codex over MCP) and watch the agents work. First-time users must not get confused.

## Product Purpose
Let AI agents operate websites and apps in an invisible, separate space instead of the user's own screen, so the user keeps their mouse and keyboard. The user watches every agent live in the GhostDesk window, can take over at any time (login, 2FA), and approves sensitive actions.

## Positioning
Agents never touch the visible screen: hidden browsers with cloned logins, hidden app windows, a Windows child session (PiP), or remote VNC — all watched through per-agent virtual cursors in one window.

## Operating Context
- Window often sits beside other work while several agents run at once; the user glances at status (working / waiting for a question / error) and steps in when needed.
- Main surfaces: Home (task composer, the initial screen), All agents (live screen overview), agent detail (live screen, log, next tasks, takeover), new-agent wizard (mode tiles + task), settings, external-connection window, approval dialogs, full-screen "화면 모아보기" overview.
- UI language is Korean.

## Capabilities and Constraints
- Rendered with egui (immediate mode): theming is via `src/app/theme.rs` tokens and widget helpers; no CSS, no web fonts unless bundled.
- Modes available differ by OS (`platform::caps`).
- Approval dialogs for sensitive actions must stay prominent and unambiguous.

## Brand Commitments
None binding. User explicitly allowed the look to change freely (keep features and copy).

## Evidence on Hand
No testimonials, metrics or screenshots to show. Do not fabricate any.

## Product Principles
1. The agent's live screen is the hero; chrome stays quiet.
2. Status must be readable at a glance (working, waiting for the user, error, human takeover).
3. Anything that touches the user's real data or account is shown plainly and needs a clear human yes.
4. A first-time non-developer can start an agent without reading docs.
