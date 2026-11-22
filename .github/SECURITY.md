# Security policy

## Reporting a vulnerability

Please report it privately, not in a public issue: use [Report a vulnerability](https://github.com/emanuele-em/kanzo/security/advisories/new) on GitHub. Say what an attacker could do, how to reproduce it, and which version you run (`kanzo --version`).

Fixes go into the latest release only.

## What Kanzo defends against

The board has no logins, so it relies on who can reach it:

- By default it listens on `127.0.0.1`, this machine only. With `listen = "0.0.0.0"` in `config.toml`, anyone who can reach the port can read and change the board and use its MCP endpoint.
- It answers only `localhost`, IP addresses, the names listed in `hosts` and, on the network, this machine's own names, so a web page cannot reach it through DNS rebinding.
- Opening files and changing sharing are for this computer only. With `hosts` set, a request on a listed name, or one a proxy marks as forwarded (`Forwarded`, `X-Forwarded-For` or `X-Forwarded-Host`), counts as another computer even from `127.0.0.1`. A proxy in front must add one of those headers, as Caddy and `tailscale serve` do; nginx needs `proxy_set_header X-Forwarded-For $remote_addr;`.
- It refuses browser requests sent from any page other than the board itself.
- A Content Security Policy allows scripts only from the board, so even a bug in the Markdown renderer cannot run injected script.

A way around any of these is a vulnerability.
