## 0.4.1 - 2026-11-07

### New features

- **Updates reach your agents:** on macOS and Linux, after an update each agent's Kanzo switches to the new version at its next call and gets the new tools, without reconnecting.
- **The board follows updates:** on macOS and Linux, however you update, from Settings, with `kanzo update`, the installer or `cargo install`, the running board restarts on the new version within seconds, and open pages reload.
- **Once, for this update:** what is already running is the old version, which can't switch by itself. Reconnect Kanzo in open agent sessions (in Claude Code, `/mcp`), and restart the board unless you updated from Settings or it runs as a service.

### Bug fixes

- **Updating a shared board:** a board started with `kanzo network on` no longer stops when an update restarts it.

## 0.4.0 - 2026-11-06

### New features

- **Make a task wait on others:** in a task's panel, type in Waits on to find tasks and pick them, or ask an agent to pass `waits_on`. `next_task` skips it until they're done, and the filter `is:waiting` finds it.
- **Select several tasks:** `⌘`-click them or drag on empty board space, then move, tag or delete them from the bar at the bottom.
- **Drag them together:** drag one selected task and the rest follow, stacked behind it, spreading out where you drop.
- **List view:** click the list icon at the top of the board to see the same tasks as rows, then sort and group them.
- **Install the board as an app:** in Chrome or Edge, click Install app at the bottom of the sidebar. In the app, `⌘1`–`⌘8` pick a project, `⌘9` the last one and `⌘0` all of them.
- **Open files from tasks:** a link like `[name](file:///path)` in a description or comment opens in your editor. Set `editor` in `config.toml` to choose which one.
- **Resize the task panel:** drag its left edge, or expand it to the whole window. Double-click the edge to reset it.
- **Fold the sidebar:** the panel button beside search folds it to its icons.
- **Reorder projects:** drag a project in the sidebar. On a shared board, the order changes for everyone.
- **Resume an agent's session:** when a task reaches `review`, the agent's comment says how to go back to its session.
- **What's new:** Settings, under About, shows the notes for this version.
- **The board in Chinese:** it follows your browser's language, or pick one in Settings, under Board.

### Bug fixes

- **Settings that spread through sync:** `kanzo sync` no longer carries `config.toml` between machines, so one machine's `listen = "0.0.0.0"` can't open another's board to the network. A machine that already received it keeps it: check `listen` in its `config.toml`.
- **Esc in a task:** it now keeps what you typed in a description or tags, instead of discarding it.
- **Empty descriptions:** an empty one no longer shows as a sliver when you open it.
