# Changelog

## [Unreleased]

### Added

- `mcphive run -- COMMAND` shares one MCP server between every client that runs the
  same command with the same arguments in the same directory, over a named pipe
  (Windows) or a Unix socket. `status`, `stop` and `demo-server`.
- Ids, the handshake, progress, cancellation and the requests of the server are
  kept apart per client; the server and everything it started end with the daemon,
  which stops after the last client has been gone for `--idle` seconds.
