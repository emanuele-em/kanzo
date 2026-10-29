<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/img/kanzo-dark.jpg">
    <img alt="kanzo: a tiny kanban board for you and your coding agents" src="docs/img/kanzo-light.jpg">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/emanuele-em/kanzo/actions/workflows/ci.yml"><img src="https://github.com/emanuele-em/kanzo/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://crates.io/crates/kanzo"><img src="https://img.shields.io/crates/v/kanzo" alt="crates.io"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue" alt="AGPL-3.0"></a>
</p>

Kanzo is a kanban board in your browser and an MCP server in one small program, so you and your coding agents work on the same tasks. The whole board is one folder, and a private git repo keeps it in step on every machine you use: no account, no server.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/board-dark.gif">
  <img alt="The Kanzo board: agents move cards while you watch, and ⌘K finds a task in any project" src="docs/img/board-light.gif">
</picture>

## Install

```sh
curl -LsSf https://github.com/emanuele-em/kanzo/releases/latest/download/kanzo-installer.sh | sh
```

For macOS and Linux, including every Raspberry Pi. Every release is signed by GitHub: check a download with `gh attestation verify <file> --repo emanuele-em/kanzo`. Or with Cargo:

```sh
cargo install kanzo
```

## Use

| Command | Does |
| --- | --- |
| `kanzo` | Starts the board at http://localhost:4242 and opens it; `--port` for another port, `--listen 0.0.0.0` to open it to your network, `--no-open` to not open it |
| `kanzo service install` | Keeps the board running and starts it at login; `uninstall` removes it |
| `kanzo mcp` | The MCP server for agents: they start it, not you |
| `kanzo sync` | Commits the board and pulls or pushes it; `--repo <url>` sets the repo, `--keep-local` or `--keep-remote` when both sides changed |
| `kanzo export` | Prints the board as Markdown; `-f json` for JSON, `-p <project>` for one project, `-o <file>` to save it |
| `kanzo update` | Installs the latest release and restarts the board's service; the board offers new versions too |
| `kanzo init` | Creates the data folder; the first run does this too |
| `kanzo --version` | Prints the version |

Every command takes `--home <dir>` to use another data folder.

`n` adds a task, `/` or `⌘K` searches every project, `Tab` moves to the description, `⌘↵` saves and closes, `Esc` cancels.

Need somewhere to park ideas? Turn on the Backlog column in Settings, at the bottom of the sidebar: agents leave its tasks alone until you move them to `todo`.

Paste or drop an image into a task or a comment to attach it, as in ChatGPT. Agents get its file path and can open it.

When a new version is out, the sidebar offers it: one click installs it and restarts the board.

## Connect your agents

```sh
claude mcp add kanzo --scope user -- kanzo mcp   # Claude Code
codex mcp add kanzo -- kanzo mcp                 # Codex
```

Other MCP clients: run `kanzo mcp` (apps like Raycast need the full path, from `which kanzo`), or use the running board at `http://localhost:4242/mcp`.

Then ask: *"take the next task from kanzo and do it."* The agent moves the card to `doing` and comments as it works. It finds the right project from the git remote of the repo it works in; link more repos to a project from its ⋯ menu.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/task-dark.jpg">
  <img alt="A task open on the board, with Claude Code's comments and a reply" src="docs/img/task-light.jpg">
</picture>

## Your board on every machine

A laptop, a desktop, a Raspberry Pi: they all share one board through a private git repo you own.

1. [Create an empty private repo](https://github.com/new?name=kanzo-data&visibility=private) on GitHub, or anywhere git can push.
2. Click Settings at the bottom of the sidebar, choose Sync and paste its URL. Kanzo makes `~/kanzo` a git repo and pushes the board.
3. On each other machine, do the same with the same URL: Kanzo offers to use the board that's already there.

From then on, choose Sync, or run `kanzo sync`, when you switch machines. It commits the board and pulls or pushes it, and it's safe while the board and your agents are running. If both machines changed the board, Kanzo asks which one to keep. To move to another repo, change it in Settings, under Sync.

Sync is for switching machines: work on one, sync, then move to the other. For agents on several machines at once, run one board with `listen = "0.0.0.0"` on a network you trust, and point every agent at `http://<host>:4242/mcp`, passing the project by name.

## Settings

Everything lives in `~/kanzo`: the board in `kanzo.db`, settings in `config.toml`. Use another folder with `--home` or `KANZO_HOME`.

```toml
port = 4242
listen = "127.0.0.1"   # "0.0.0.0" opens the board to your network: no logins, so only on one you trust
mcp_http = true        # MCP at /mcp
```

## On a server, with Docker

`compose.yaml` runs one board for all your machines and agents, on a server on a network you trust. In a clone of this repo:

1. Put the server's name in `.env`: `KANZO_HOSTNAME=myserver.lan`. The board refuses host names it does not know.
2. Run `mkdir -p data && docker compose up -d --build`. The board is in `./data`.
3. Open `http://myserver.lan:4242`, and point your agents at `http://myserver.lan:4242/mcp`. For Claude Code, on each machine:

   ```sh
   claude mcp add --transport http kanzo --scope user http://myserver.lan:4242/mcp
   ```

   Over HTTP the board cannot see the agent's git repo, so agents pass the project by name.

### Sync the server's board

The server's board can sync with your private repo, as in [Your board on every machine](#your-board-on-every-machine). The container has no SSH key, so it signs in to GitHub with a token over HTTPS.

1. [Make a fine-grained token](https://github.com/settings/personal-access-tokens/new). Under Repository access, choose Only select repositories and pick your data repo. Under Repository permissions, set Contents to Read and write. Copy the token.
2. Log in to the server and go to the clone. Then paste only this command. If you paste it together with the `ssh` command, the server's shell can drop it. The command asks for the token and does not show it:

   ```sh
   mkdir -p secrets && (umask 077; printf 'Token: '; stty -echo; read -r t; stty echo; echo
     printf 'https://x-access-token:%s@%s\n' "$t" github.com > secrets/git-credentials)
   ```

   Make `secrets` before the first `docker compose up`. Otherwise Docker makes it, and it belongs to root.
3. Run `docker compose up -d`. Then check that the container can reach the repo: `docker compose exec kanzo git ls-remote https://github.com/you/kanzo-data HEAD` must end without an error. If git asks for a user name, the container cannot see the token: run `docker compose up -d --force-recreate`. This happens when `secrets` was made again after the container started. If git cannot connect, the container's network cannot reach GitHub, even when the server itself can, for example behind a proxy that serves only the server.
4. On the board, choose Settings, then Sync, and paste the repo's `https://` URL. The short `you/kanzo-data` form tries SSH first, which the container cannot use.

When the token expires, sync fails with an authentication error. Regenerate the token on GitHub and do step 2 again. The board reads the file at each sync, so it needs no restart.

## Small and fast

| | |
| --- | --- |
| Program | 4.1 MB, SQLite included |
| Memory | 4.5 MB idle, 8 MB with 3,000 tasks |
| Start | 2 ms |
| Board of 3,000 tasks | 17 ms to load in the browser |
| Agents' tool list | about 900 tokens, once per session |
| Agents' replies | 10 to 60 tokens of plain text |

Measured on an Apple M4.

## License

[AGPL-3.0](LICENSE): use it, change it and share it; if you offer a changed version over a network, share its source too. To help, see [CONTRIBUTING](.github/CONTRIBUTING.md).

<sub>mcp-name: io.github.emanuele-em/kanzo</sub>
