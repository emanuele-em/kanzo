## 0.3.1 - 2026-11-05

### New features

- Press `f` to filter the board, and Enter to keep a filter as a chip beside the box
- Filter and search by any task property, like `tag:bug` or `is:human`
- Search shows a task's description, tags and comments
- Link pull requests to tasks, and open them from the board

### Bug fixes

- Search and filters ignore case beyond ASCII
- Agents can no longer take a task while it's being created as human
- Repos in nested GitLab groups no longer share one project
- Shared boards work with MCP clients that never initialize
- `list_tasks` accepts a status in any case
