## 0.4.1 - 2026-11-07

### New features

- Agents switch to a new version of Kanzo on their own: after an update, each agent's Kanzo moves to it at its next call and gets the new tools, without reconnecting. On macOS and Linux. ([#40](https://github.com/emanuele-em/kanzo/pull/40))
- The board restarts on a new version within seconds, however you update: from Settings, with `kanzo update`, the installer or `cargo install`. Open pages reload with it. On macOS and Linux. ([#40](https://github.com/emanuele-em/kanzo/pull/40))
- Markdown tables in descriptions and comments show as tables, with column alignment. ([#39](https://github.com/emanuele-em/kanzo/pull/39))

### Bug fixes

- Fixed a board started with `kanzo network on` stopping when an update restarted it. ([#40](https://github.com/emanuele-em/kanzo/pull/40))

### Docs

- Added the README in Chinese. ([7849c68](https://github.com/emanuele-em/kanzo/commit/7849c68))

### Updating from 0.4.0

- What's running now is 0.4.0, which can't switch by itself: reconnect Kanzo once in agent sessions that are already open (`/mcp` in Claude Code), and restart the board unless you updated from Settings or it runs as a service. ([#40](https://github.com/emanuele-em/kanzo/pull/40))
