# mcphive

Run each MCP server once and share it between all your AI agents.

Claude Code, Claude Desktop, Codex and Cursor each start their own copy of every
MCP server in their settings, for every session. Ten sessions with ten servers
are a hundred processes, and each of them is usually a Node or Python launcher
that starts another. `mcphive` sits in front of a server: the first client starts
it, the others use the same process, and it stops when the last one has been gone
for a while. One small Rust binary, Windows, macOS and Linux.

```sh
$ mcphive status
key                 daemon process clients up (s)  procs      MB  server
96903e17d4fe4161      1768  112592       3      3      5     288  npx (+2 arguments)

1 shared server for 3 clients. Without mcphive each client would have started its own copy: about 10 more processes and 576 MB more (an estimate from the size of each shared server).
```

**Status: version 0.1.** It works against the official MCP TypeScript client and
the reference server (see [How it is tested](#how-it-is-tested)), and it has had
little use. Read [What to share](#what-to-share) before you put every server
behind it.

## Install

Download the binary for Linux, macOS or Windows from the
[releases](https://github.com/zbl1998-sdjn/mcphive/releases/latest) (each has a
`.sha256` file) and put it on your `PATH`, or:

```sh
cargo install mcphive
cargo binstall mcphive      # the same binary, without building it
```

## Use

`mcphive wrap` does the editing for you. It shows what it would change, and
changes nothing until you add `--apply`:

```sh
mcphive wrap              # a preview, for the settings of every client it finds
mcphive wrap --apply      # change them; each file is copied first
mcphive unwrap --apply    # put the servers back as they were
```

It knows the settings of Claude Code (`~/.claude.json` and `.mcp.json`), Claude
Desktop (Windows and macOS), Cursor (`~/.cursor/mcp.json` and `.cursor/mcp.json`)
and Codex (`~/.codex/config.toml`); `--client` picks one and `--file` names a
file. It wraps the servers that run as a program, adds `--key-env` for every
variable in the `env` of a server, and leaves remote servers alone. It also
leaves alone the servers that look as if they keep something for each client (a
browser, a shell; `--include-stateful` wraps them anyway). The preview shows the
program of each server, never its arguments or its environment. Close the
clients before you apply: some of them write their settings back when they
close. A JSON file is written again with two-space indentation and its keys in
the same order; a TOML file keeps its comments.

To do it by hand, put `mcphive run --` in front of the command of a server.

Claude Code (`.mcp.json`, or `claude mcp add`; see the
[MCP documentation](https://code.claude.com/docs/en/mcp)):

```json
{
  "mcpServers": {
    "docs": {
      "command": "mcphive",
      "args": ["run", "--", "npx", "-y", "@upstash/context7-mcp"]
    }
  }
}
```

```sh
claude mcp add docs -- mcphive run -- npx -y @upstash/context7-mcp
```

Claude Desktop and Cursor use the same `mcpServers` object in their own settings
files. Codex (`~/.codex/config.toml`, see the
[MCP page](https://learn.chatgpt.com/docs/extend/mcp)):

```toml
[mcp_servers.docs]
command = "mcphive"
args = ["run", "--", "npx", "-y", "@upstash/context7-mcp"]
```

Two clients share a server when they run the same command with the same
arguments in the same working directory. The environment of the server is the one
of the client that started it; if a variable should make two otherwise equal
servers different (an API key, say), name it: `mcphive run --key-env GITHUB_TOKEN
-- ...`.

| Command | |
|---|---|
| `mcphive run [--idle SECONDS] [--key-env NAME]... -- COMMAND [ARGS]...` | what goes in the settings of a client; the server stays up 120 seconds after the last client by default |
| `mcphive wrap [--apply]`, `mcphive unwrap [--apply]` | put `mcphive run --` in front of the servers in the settings of your clients, or take it out; a preview without `--apply` |
| `mcphive status` | the shared servers that are running, how many clients each has, how many processes and how much memory (the working set on Windows, the resident size elsewhere, shared pages counted for each process), and what the clients beyond the first would have cost |
| `mcphive stop --all` or `mcphive stop KEY...` | stop them now |
| `mcphive demo-server` | a small MCP server to try this with: `echo`, `pid` (the same number for every client when shared) and `slow` (progress) |

The logs of the servers (what they write to standard error) are in
`%LOCALAPPDATA%\mcphive\logs` on Windows and `~/.local/state/mcphive/logs`
elsewhere.

## What to share

An MCP session is a conversation, and a server may keep something for each one.
`mcphive` keeps the conversations apart where the protocol has the means to (ids,
progress, cancellation, the handshake), but it cannot give two clients two
separate browsers.

- **Good to share:** servers that look something up or call an API for each
  request: documentation lookup, search, a read-only view of a service or a
  database.
- **Do not share:** servers that hold state a client builds up: browser
  automation (Playwright, Chrome DevTools: all clients would drive one browser),
  a shell, a REPL.
- **Roots, sampling, elicitation.** A request from the server to a client goes to
  the client that spoke last, and a client's change of roots is not passed on.
  Servers that rely on them are better left alone.
- Only servers that speak MCP over standard input and output. JSON-RPC batches are
  refused (the current protocol has no batches).
- The server runs as you and is reachable by you only (a named pipe or a Unix
  socket in a folder that is yours).

## Tried with

Two clients at once, directly and through `mcphive`, on Windows 10 with Node 24
(2026-10-06). The tools that both clients saw were the same as without
`mcphive`, and so were the results of a call.

| Server | Result |
|---|---|
| `@modelcontextprotocol/server-memory` | the same 9 tools and results; the knowledge graph is one for all clients |
| `@modelcontextprotocol/server-filesystem` | the same 14 tools and results |
| `@modelcontextprotocol/server-sequential-thinking` | the same tool and result; the history of thoughts is one for all clients |
| `@upstash/context7-mcp` | the same 2 tools; no call was made |
| `@modelcontextprotocol/server-everything` | the official TypeScript client against it, in CI on Windows and Linux |

Each of the four `npx` servers ran as 4 processes for the two clients, where two
direct clients start 8, and nothing was left after the last client went away. Only
the demo server and `server-everything` have been tried on Linux and macOS, and
`npx` through `mcphive` only on Windows.

## How it works

`mcphive run` is a small program that the client starts instead of the server. It
connects to the daemon of that server over a named pipe (Windows) or a Unix
socket, and starts the daemon if there is none. The daemon runs the server once
and gives each shim its own view of it:

- every request gets an id of its own on the way to the server and its original
  id back, so two clients that both send request 1 do not meet;
- the `initialize` handshake happens once, and later clients get the answer the
  first one got;
- a progress token is replaced by one that belongs to the request, so progress
  goes to the client that asked for it;
- a cancellation names the request it means; a client that leaves has its running
  requests cancelled;
- notifications of the server (a list that changed, log messages) go to everyone.

The daemon runs the server inside a job object (Windows) or a process group
(Unix), so that the launcher and everything it started go with it, and it stops
`--idle` seconds after the last client has left. A shim that connects as the
daemon is about to stop is told so and starts a new one instead.

## How it is tested

- `src/router.rs` holds all of the above as a state machine with no I/O, with 15
  tests of ids, handshakes, progress, cancellation and the requests of the server.
- `tests/share.rs` runs real shims and daemons against the demo server: one
  process for two clients, progress and ids that stay with their client, a client
  that leaves, the idle stop, `stop`, a server that cannot start, and clients that
  arrive while the daemon is stopping.
- `tests/wrap.rs` runs `wrap` and `unwrap` on settings files in a folder of their
  own: the preview that changes nothing, the copy that `--apply` makes first,
  the way back, the comments of a TOML file, and a wrapped entry that is started
  and answers as a shared server.
- `tests/oracle` drives the official MCP TypeScript client (SDK 1.32.0) against the
  official reference server (`server-everything` 2026.8.31) once directly and once
  through `mcphive` with two clients at the same time, and compares what they see:
  the tools, prompts and resources, the results of calls, an error, progress. It
  needs Node. Run it with `npm ci && node oracle.mjs ../../target/debug/mcphive`
  in `tests/oracle`.

## License

MIT or Apache-2.0, at your option.
