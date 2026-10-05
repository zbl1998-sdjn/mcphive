# Changelog

## [Unreleased]

### Added

- `mcphive wrap` and `mcphive unwrap`: put `mcphive run --` in front of the
  servers in the settings of Claude Code, Claude Desktop, Cursor and Codex, and
  take it out again. A preview unless `--apply`; a file is copied first, written
  through a temporary file, and left alone if it changed meanwhile. Remote
  servers and servers that keep something for each client (a browser, a shell)
  are skipped; the variables of a server's `env` become `--key-env`.
- `mcphive status` shows the processes of each shared server and their memory,
  and what the clients beyond the first would have cost.
- A table in the README of the servers that have been tried through `mcphive`.

## [0.1.0] - 2026-10-05

### Added

- `mcphive run -- COMMAND` shares one MCP server between every client that runs the
  same command with the same arguments in the same directory, over a named pipe
  (Windows) or a Unix socket. `status`, `stop` and `demo-server`.
- Ids, the handshake, progress, cancellation and the requests of the server are
  kept apart per client; the server and everything it started end with the daemon,
  which stops after the last client has been gone for `--idle` seconds.
