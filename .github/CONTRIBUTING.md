# Contributing to Kanzo

Thanks for helping. Kanzo is small on purpose: every change is weighed by how light and fast it keeps the board, and how few tokens it costs agents.

- **A bug or an idea:** open an issue first for anything bigger than a small fix, so we can agree on the approach.
- **A security problem:** don't open an issue; see [SECURITY](SECURITY.md).
- **License:** Kanzo is under the [AGPL-3.0](../LICENSE), and so is what you contribute.
- **Conduct:** be kind; see the [code of conduct](CODE_OF_CONDUCT.md).

## Build and test

```sh
cargo test                               # store, config, export, web and service tests
cargo run                                # the board, with data in ~/kanzo
cargo run -- --home /tmp/k --port 4300   # a throwaway board
```

Before sending a change, run what CI runs, on the latest stable Rust:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

For a change to the board, also try it in a browser, in light and dark mode.

## What a good change looks like

- **Light.** No new dependency without a strong reason. If it could cost speed or memory, measure before and after: CI comments on every pull request with what it changes in size, memory, speed and agents' tokens, and `python3 .github/resources/measure.py --help` runs the same measurements on your machine.
- **Few tokens.** MCP replies stay short plain text; say what a change does to their size.
- **Plain web.** The board is HTML, CSS and JavaScript with no build step, and runs on Safari 13.1, Chrome 80 and Firefox 78.
- **Small commits.** One change per commit, with a short imperative subject and a body that says why.

## The code

Where things are:

| File | Does |
| --- | --- |
| `src/main.rs` | Command line |
| `src/home.rs` | Data folder and `config.toml` |
| `src/schema.sql` | The database schema |
| `src/store.rs` | All SQL |
| `src/mcp.rs` | The MCP tools, over stdio and HTTP (`rmcp`, pinned on purpose: it still changes often) |
| `src/web.rs` | The board's HTTP API, and `/mcp` |
| `src/index.html`, `src/app.js`, `src/logo.png` | The board UI: plain HTML, CSS and JavaScript, built into the binary |
| `src/sync.rs` | `kanzo sync` |
| `src/export.rs` | `kanzo export` |
| `src/service.rs` | `kanzo service` |
| `Dockerfile`, `compose.yaml` | One board in a container on a server |

## How it stays light

- **One thread** runs the server; database work goes to at most 8 helper threads, which exit when idle.
- **One short SQLite connection per operation,** with a 5-second busy timeout, so the board and any number of agents can write at once, and `kanzo sync` can safely swap the file between operations. The database uses a rollback journal instead of WAL, because with short connections WAL created and deleted two files on every request. Now a read touches no extra files, and each operation is about 3 times faster.
- **Cheap polling:** the board asks for a change token every 2 seconds, only while its tab is visible, and waits for each answer before asking again. Database triggers bump the token on every write.
- **Small pages:** the board loads only the newest 50 done tasks until you ask for more or search, so a board that has piled up thousands of done tasks reloads as fast as a new one.
- **Few tokens for agents:** MCP replies are short plain text, and the tool list agents read at the start of every session is kept small (about 3 KB).
- **Release profile:** `opt-level = "s"` with full LTO measured 30% smaller than the default at the same speed.

Measured on a 64-bit ARM Linux container limited to 64 MB of memory and half a CPU: 3.5 MB of memory and one thread when idle, 4.3 MB after 200 tasks.

## Releases and dependencies

Releases are built by [cargo-dist](https://github.com/axodotdev/cargo-dist): push a tag like `v0.1.0` and GitHub Actions builds every target and the installer, on Ubuntu 26.04 LTS and macOS 26. `.github/workflows/release.yml` is generated: change `dist-workspace.toml` and run `dist generate`, never edit the file by hand.

Dependabot opens a weekly pull request when a crate or a GitHub Action has a new version.
