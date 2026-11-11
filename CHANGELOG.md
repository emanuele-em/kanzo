## 0.4.2

### Bug fixes

- Fixed `kanzo update` and the board's Update button always failing on Windows (thanks @Bortlesboat). ([#43](https://github.com/emanuele-em/kanzo/pull/43))
- Fixed an agent's Kanzo tools no longer answering after an update, when the agent had cancelled a call before it (thanks @JYunth). ([#45](https://github.com/emanuele-em/kanzo/pull/45))
- Fixed agents that never send `initialize`, like clients on MCP 2026-07-28, staying on the old version after an update (thanks @JYunth). ([#50](https://github.com/emanuele-em/kanzo/pull/50))
- `kanzo export` now stops with a message on a computer that has joined another board, instead of quietly writing its own old board (thanks @Suyog007). ([#49](https://github.com/emanuele-em/kanzo/pull/49))

### Dependencies

- Updated rmcp to 3.9.0 and toml to 1.2.0. ([#44](https://github.com/emanuele-em/kanzo/pull/44))
