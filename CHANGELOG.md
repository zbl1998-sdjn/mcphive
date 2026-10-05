# Changelog

## [Unreleased]

## [0.2.0] - 2026-10-06

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

### Fixed

- Under `XDG_RUNTIME_DIR`, which Linux usually sets, `MCPHIVE_NAMESPACE` was not
  part of the folder of the sockets, so the shared servers of two namespaces
  could see and stop each other. The folder has the namespace in its name now.
- A socket path may have 103 bytes at most on macOS, and the folder of the
  sockets had one level too many, so a long user name could keep the daemon from
  starting there. There is one level less.
- When the daemon goes away, the shim says why (closed, broken, or standard
  output gone), and the daemon writes a failed read or write of a client to its
  log.

## [0.1.0] - 2026-10-05

### Added

- `mcphive run -- COMMAND` shares one MCP server between every client that runs the
  same command with the same arguments in the same directory, over a named pipe
  (Windows) or a Unix socket. `status`, `stop` and `demo-server`.
- Ids, the handshake, progress, cancellation and the requests of the server are
  kept apart per client; the server and everything it started end with the daemon,
  which stops after the last client has been gone for `--idle` seconds.
