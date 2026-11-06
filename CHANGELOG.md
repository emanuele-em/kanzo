## 0.3.1 - 2026-11-05

### New features

- **Filter the board:** press `f` and type to keep only the tasks that match; Enter keeps the filter as a chip.
- **Filter by any property:** type `tag:bug`, `status:doing` or `-is:human` in the filter, in search or in an agent's `filter`. Qualifiers turn blue as you type.
- **Fuller search:** `/` now finds words in comments and shows each task's description, tags and comments.
- **A URL on each task:** paste a link in the task's URL row, or ask an agent to set it. The card shows the site's name and opens it.
- **Turn task properties off:** in Settings, under Board, hide one from the board and from what agents get. Nothing is deleted.

### Bug fixes

- **Case in search:** search and filters now ignore case beyond ASCII, so `CAFÉ` finds "café".
- **Tasks created as human:** an agent can no longer take one in the moment before it's marked human.
- **Nested GitLab repos:** each repo in a subgroup now gets its own project, instead of all sharing one.
- **Newer MCP clients on a shared board:** clients that send no `initialize` no longer get "The shared board answered 400."
- **Comment authors:** an HTTP client that gives no name is no longer credited as `rmcp`.
- **Status in `list_tasks`:** it now accepts any case, like `Doing`.
