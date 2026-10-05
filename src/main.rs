//! `mcphive`: run each MCP server once and share it between all your agents.

mod child;
mod daemon;
mod demo;
mod ipc;
mod router;
mod shim;

use std::{io, process::ExitCode, time::Duration};

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Run each MCP server once and share it between all your agents.
///
/// Put `mcphive run --` in front of the command of a server in the settings of
/// Claude Code, Claude Desktop, Codex or Cursor. The first client starts the
/// server, the others use the same process, and it stops when the last one has
/// been gone for a while.
#[derive(Parser)]
#[command(name = "mcphive", version, max_term_width = 100)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a server through a shared process. This is what goes in a client's settings.
    Run {
        /// Seconds the shared server stays up with no client.
        #[arg(long, default_value_t = 120)]
        idle: u64,
        /// An environment variable whose value makes the server a different one
        /// (repeat it for more). By default only the command, its arguments and
        /// the working directory tell servers apart.
        #[arg(long = "key-env", value_name = "NAME")]
        key_env: Vec<String>,
        /// The command of the server and its arguments.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// List the shared servers that are running.
    Status,
    /// Stop shared servers: those with the given keys, or all of them.
    Stop {
        /// Stop every shared server.
        #[arg(long)]
        all: bool,
        /// The keys shown by `status`.
        keys: Vec<String>,
    },
    /// A small MCP server to try mcphive with.
    DemoServer,
    #[command(hide = true)]
    Serve {
        #[arg(long)]
        key: String,
        #[arg(long)]
        idle: u64,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

/// Ask a daemon something and read its one-line answer.
async fn ask(key: &str, request: &str) -> io::Result<Value> {
    let conn = ipc::connect(&ipc::Endpoint::new(key)).await?;
    let (read, mut write) = tokio::io::split(conn);
    write
        .write_all(format!("{}\n", json!({"mcphive": request})).as_bytes())
        .await?;
    write.flush().await?;
    let mut lines = BufReader::new(read).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
    serde_json::from_str(&line).map_err(io::Error::other)
}

async fn status() -> ExitCode {
    let keys = ipc::list();
    if keys.is_empty() {
        println!("no shared servers are running");
        return ExitCode::SUCCESS;
    }
    println!(
        "{:<18} {:>7} {:>7} {:>7} {:>6}  server",
        "key", "daemon", "process", "clients", "up (s)"
    );
    for key in keys {
        match ask(&key, "status").await {
            Ok(info) => {
                let number = |name: &str| {
                    info[name]
                        .as_u64()
                        .map_or_else(|| "?".to_owned(), |n| n.to_string())
                };
                let command = info["command"]
                    .as_array()
                    .map_or_else(String::new, |parts| {
                        let program = parts.first().and_then(Value::as_str).unwrap_or("?");
                        let name = std::path::Path::new(program).file_name().map_or_else(
                            || program.to_owned(),
                            |name| name.to_string_lossy().into_owned(),
                        );
                        format!("{name} (+{} arguments)", parts.len().saturating_sub(1))
                    });
                println!(
                    "{:<18} {:>7} {:>7} {:>7} {:>6}  {command}",
                    key,
                    number("daemon_pid"),
                    number("server_pid"),
                    number("clients"),
                    number("uptime_secs"),
                );
            }
            Err(_) => println!("{key:<18} (not answering)"),
        }
    }
    ExitCode::SUCCESS
}

async fn stop(all: bool, keys: Vec<String>) -> ExitCode {
    let keys = if all { ipc::list() } else { keys };
    if keys.is_empty() {
        eprintln!("mcphive: give the keys to stop, or --all");
        return ExitCode::from(2);
    }
    let mut failed = false;
    for key in keys {
        match ask(&key, "stop").await {
            Ok(_) => println!("stopped {key}"),
            Err(error) => {
                eprintln!("mcphive: {key}: {error}");
                failed = true;
            }
        }
    }
    ExitCode::from(u8::from(failed))
}

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run {
            idle,
            key_env,
            command,
        } => {
            let options = shim::Options {
                idle: Duration::from_secs(idle),
                key_env,
                command,
            };
            match shim::run(options).await {
                // Leave at once. The thread that waits for the client's standard
                // input may be stuck in a read that only the client can end, and
                // a normal return would wait for it.
                Ok(code) => std::process::exit(code),
                Err(error) => {
                    eprintln!("mcphive: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Status => status().await,
        Command::Stop { all, keys } => stop(all, keys).await,
        Command::DemoServer => match demo::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("mcphive: {error}");
                ExitCode::FAILURE
            }
        },
        Command::Serve { key, idle, command } => {
            let options = daemon::Options {
                key,
                idle: Duration::from_secs(idle),
                command,
            };
            match daemon::serve(options).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    // Nobody is reading this, so it goes where the logs are.
                    let _ = std::fs::create_dir_all(daemon::log_dir());
                    let _ = std::fs::write(
                        daemon::log_dir().join("last-error.txt"),
                        format!("{error}\n"),
                    );
                    ExitCode::FAILURE
                }
            }
        }
    }
}
