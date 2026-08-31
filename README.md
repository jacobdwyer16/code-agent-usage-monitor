# Code Agent Usage Monitor

Personal side project: `Code Agent Usage Monitor` is a lightweight Windows taskbar widget for tracking the shared Claude and Codex account rate limits in real time.

This repository is a respectful fork of Code Zeno Pty Ltd's `Claude Code Usage Monitor`.

I did not create the original app. This side project builds on that work, keeps the original MIT license, and adds my changes for multi-agent monitoring and project-specific packaging.

It embeds into the taskbar, stays out of the way, and shows rolling-window utilization plus reset countdowns for both providers. The counters include work performed through Claude Code, Claude Cowork, Codex, and ChatGPT Work.

![Windows](https://img.shields.io/badge/platform-Windows-blue)
![Rust](https://img.shields.io/badge/language-Rust-orange)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## Features

- Four compact taskbar progress bars
- `Cl 5h` and `Cl 7d` for Claude session and weekly usage
- `Cx 5h` and `Cx 7d` for Codex session and weekly usage
- Live countdown text beside each bar
- Embedded Win32 taskbar rendering with dark/light mode support
- Drag-to-reposition behavior inside the taskbar
- Configurable polling interval from the context menu
- Automatic settings persistence in `%APPDATA%\CodeAgentUsageMonitor\settings.json`

## Data sources

### Claude

Claude usage is fetched from the first current local Claude OAuth credential:

1. Read Claude Desktop's encrypted `oauth:tokenCacheV2` from `%APPDATA%\Claude\config.json`, decrypting it in memory with the current Windows user's Chromium key
2. Also check `~/.claude/.credentials.json` on Windows and inside accessible WSL distros when Claude Code is installed
3. Query Anthropic's OAuth usage endpoint for 5-hour and 7-day utilization
4. Fall back to rate-limit headers from the Messages API if the usage endpoint is unavailable

Anthropic applies one usage allocation across Claude product surfaces, including Claude Code and Claude Desktop. Cowork therefore changes the same server-side counters shown by the Claude bars. Claude Code is not required.

### Codex

Codex usage is fetched from the same account-backed endpoint used by the ChatGPT Codex usage page:

1. Read `~/.codex/auth.json` for the ChatGPT access token and account id shared by the Windows ChatGPT desktop app and native Codex clients
2. Query `https://chatgpt.com/backend-api/wham/usage?platform=codex`
3. Render the primary 5-hour and secondary 7-day windows from that response

If the online usage call is unavailable, the app falls back to the most recent local Codex session snapshot from `~/.codex/sessions/**/*.jsonl`.

ChatGPT Work and Codex share usage, credits, and limits. Work performed in the ChatGPT desktop application therefore changes the same server-side counters shown by the Codex bars. Codex CLI is not required.

### Desktop application setup

No CLI installation or CLI login is needed:

1. Sign into Claude Desktop with the Claude account used by Cowork.
2. Sign into the ChatGPT desktop app and select the workspace used by ChatGPT Work.
3. Start or refresh the monitor.

Claude Desktop credentials are decrypted only in memory under the same Windows user account and are never written by the monitor. The ChatGPT desktop app already caches its authentication in the shared `%USERPROFILE%\.codex` directory.

## Requirements

- Windows 10 or Windows 11
- Rust toolchain with the MSVC target
- Claude Desktop signed into the account used by Cowork if you want Claude and Cowork usage populated
- The ChatGPT desktop app signed into the workspace used by ChatGPT Work if you want Codex and Work usage populated

Notes:

- Claude Code and Codex CLI credentials remain supported when present, but neither CLI is required.
- If you use Claude Code inside WSL2, its authenticated account remains an additional Claude credential source.

## Build

```bash
cargo build --release
```

The release binary is:

```text
target/release/code-agent-usage-monitor.exe
```

## Run

Launch the executable and it will attach to the Windows taskbar.

- Drag the left divider to reposition it
- Right-click for `Refresh`, `Update Frequency`, `Settings`, and `Exit`

## Project layout

```text
src/
|- main.rs            # entry point
|- models.rs          # shared usage data structures
|- poller.rs          # Claude polling, Codex session parsing, countdown formatting
|- window.rs          # Win32 window, painting, layout, message loop
|- native_interop.rs  # Win32 helpers
\- theme.rs           # Windows dark/light mode detection
```

## Credit

This side project is directly derived from the original `Claude Code Usage Monitor` by Code Zeno Pty Ltd.

Original upstream repository:

- https://github.com/CodeZeno/Claude-Code-Usage-Monitor

This fork extends that work to support multiple code-agent sources, including local Codex rate-limit monitoring, while keeping the same lightweight taskbar-widget approach.

## License

MIT. See [LICENSE](LICENSE).
