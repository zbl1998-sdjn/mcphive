//! The daemon of one shared server: it runs the server once and lets every shim
//! that connects talk to it, through a [`Router`].

use std::{
    collections::{HashMap, VecDeque},
    fs::{File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
    time::Instant,
};

use crate::{
    child,
    ipc::{self, Conn, Endpoint},
    router::{Action, ClientId, Event, Router},
};

/// What the daemon runs and for how long it waits for company.
pub struct Options {
    pub key: String,
    pub idle: Duration,
    pub command: Vec<String>,
}

enum Msg {
    /// A shim says hello. The answer is sent once it is registered, so that a
    /// daemon that is about to stop never takes in a client it will then drop.
    Hello {
        id: ClientId,
        tx: mpsc::Sender<String>,
        accepted: oneshot::Sender<()>,
    },
    Event(Event),
    Status(oneshot::Sender<Value>),
    Stop,
    ServerGone,
    /// Something for the log.
    Note(String),
}

/// The folder of the logs.
#[must_use]
pub fn log_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(".local").join("state"))
            })
    };
    base.unwrap_or_else(std::env::temp_dir)
        .join("mcphive")
        .join("logs")
}

fn open_log(key: &str) -> io::Result<File> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{key}.log"));
    // One server's log is not worth more than a few megabytes.
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 5 << 20) {
        std::fs::remove_file(&path)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

fn line_of(message: &Value) -> String {
    let mut line = message.to_string();
    line.push('\n');
    line
}

/// A line that asks the daemon itself something: `{"mcphive": "status"}`. No MCP
/// message has such a key.
fn control_request(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    value.get("mcphive")?.as_str().map(str::to_owned)
}

async fn connection(conn: Conn, id: ClientId, main: mpsc::Sender<Msg>) {
    let (read, mut write) = tokio::io::split(conn);
    let mut lines = BufReader::new(read).lines();
    let Ok(Some(first)) = lines.next_line().await else {
        return;
    };
    let request = control_request(&first);
    if request.as_deref() != Some("hello") {
        let Some(request) = request else { return };
        let answer = if request == "status" {
            let (tx, rx) = oneshot::channel();
            let _ = main.send(Msg::Status(tx)).await;
            rx.await.unwrap_or(Value::Null)
        } else {
            let _ = main.send(Msg::Stop).await;
            json!({"stopping": true})
        };
        let _ = write.write_all(line_of(&answer).as_bytes()).await;
        let _ = write.flush().await;
        return;
    }

    // The only other thing a connection starts with is `hello`; MCP messages
    // come after the daemon has said `welcome`.
    let (tx, mut outgoing) = mpsc::channel::<String>(1024);
    let (accepted, welcome) = oneshot::channel();
    if main
        .send(Msg::Hello {
            id,
            tx: tx.clone(),
            accepted,
        })
        .await
        .is_err()
        || welcome.await.is_err()
    {
        return;
    }
    if write
        .write_all(line_of(&json!({"mcphive": "welcome"})).as_bytes())
        .await
        .is_err()
    {
        let _ = main.send(Msg::Event(Event::ClientDown(id))).await;
        return;
    }
    let writer_main = main.clone();
    let writer = tokio::spawn(async move {
        while let Some(line) = outgoing.recv().await {
            let written = write.write_all(line.as_bytes()).await;
            if let Err(error) = written.and(write.flush().await) {
                let _ = writer_main
                    .send(Msg::Note(format!("client {id}: cannot write: {error}")))
                    .await;
                break;
            }
        }
    });
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                let _ = main
                    .send(Msg::Note(format!("client {id}: cannot read: {error}")))
                    .await;
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(message) = serde_json::from_str::<Value>(&line) {
            if main
                .send(Msg::Event(Event::FromClient(id, message)))
                .await
                .is_err()
            {
                break;
            }
        } else {
            let error = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}});
            let _ = tx.try_send(line_of(&error));
        }
    }
    let _ = main.send(Msg::Event(Event::ClientDown(id))).await;
    writer.abort();
}

/// Run the server and serve its clients until the last one has been gone for
/// `idle`, or until asked to stop. Does nothing when a daemon for this key is
/// already running.
#[allow(
    clippy::too_many_lines,
    reason = "one loop that owns all the state of the daemon"
)]
pub async fn serve(options: Options) -> io::Result<()> {
    let endpoint = Endpoint::new(&options.key);
    let mut listener = match ipc::Listener::bind(&endpoint) {
        Ok(listener) => listener,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let mut log = open_log(&options.key)?;
    // Only the program is written down: its arguments can hold a token.
    writeln!(
        log,
        "--- daemon {} starts {}",
        std::process::id(),
        options.command[0]
    )?;
    let mut server = child::spawn(&options.command, log.try_clone()?)?;
    let server_pid = server.child.id();
    let probe = server.probe();
    let _ = writeln!(log, "server process {server_pid:?}");
    let mut to_server = server
        .child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("no stdin"))?;
    let from_server = server
        .child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no stdout"))?;

    let (main_tx, mut main_rx) = mpsc::channel::<Msg>(4096);
    let (server_tx, mut server_rx) = mpsc::channel::<String>(4096);

    tokio::spawn(async move {
        while let Some(line) = server_rx.recv().await {
            if to_server.write_all(line.as_bytes()).await.is_err()
                || to_server.flush().await.is_err()
            {
                break;
            }
        }
    });
    let reader_tx = main_tx.clone();
    let mut stray = log.try_clone()?;
    tokio::spawn(async move {
        let mut lines = BufReader::new(from_server).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match serde_json::from_str::<Value>(&line) {
                Ok(message) => {
                    if reader_tx
                        .send(Msg::Event(Event::FromServer(message)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                // A server that writes something else to standard output breaks
                // the protocol; keep what it said where somebody can read it.
                Err(_) => {
                    let _ = writeln!(stray, "stdout: {line}");
                }
            }
        }
        let _ = reader_tx.send(Msg::ServerGone).await;
    });
    let acceptor_tx = main_tx.clone();
    tokio::spawn(async move {
        let mut next: ClientId = 1;
        loop {
            match listener.accept().await {
                Ok(conn) => {
                    tokio::spawn(connection(conn, next, acceptor_tx.clone()));
                    next += 1;
                }
                // One connection that went wrong must not end the listening.
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    });

    let started = std::time::Instant::now();
    let mut router = Router::new();
    let mut clients: HashMap<ClientId, mpsc::Sender<String>> = HashMap::new();
    let mut alone_since = Some(Instant::now());
    {
        let exit = server.child.wait();
        tokio::pin!(exit);

        'serving: loop {
            let deadline = alone_since.map(|since| since + options.idle);
            let message = tokio::select! {
                message = main_rx.recv() => message,
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = writeln!(log, "--- no client for {} s: stopping", options.idle.as_secs());
                    break 'serving;
                }
                status = &mut exit => {
                    let _ = writeln!(log, "--- the server exited: {status:?}");
                    break 'serving;
                }
                _ = tokio::signal::ctrl_c() => {
                    let _ = writeln!(log, "--- interrupted");
                    break 'serving;
                }
            };
            let Some(message) = message else {
                let _ = writeln!(log, "--- the message channel closed");
                break;
            };
            let mut queue = VecDeque::new();
            match message {
                Msg::Hello { id, tx, accepted } => {
                    let _ = writeln!(log, "client {id} connected");
                    clients.insert(id, tx);
                    alone_since = None;
                    queue.push_back(Event::ClientUp(id));
                    let _ = accepted.send(());
                }
                Msg::Event(event) => queue.push_back(event),
                Msg::Status(reply) => {
                    let mut info = json!({
                        "key": options.key,
                        "command": options.command,
                        "daemon_pid": std::process::id(),
                        "server_pid": server_pid,
                        "clients": router.clients(),
                        "initialized": router.initialized(),
                        "uptime_secs": started.elapsed().as_secs(),
                    });
                    // Counting the processes of the server and what they use asks
                    // the operating system, so it is done away from the loop.
                    tokio::spawn(async move {
                        let usage = tokio::task::spawn_blocking(move || probe.usage())
                            .await
                            .ok()
                            .flatten();
                        if let Some(usage) = usage {
                            info["server_processes"] = json!(usage.processes);
                            info["server_memory_bytes"] = json!(usage.memory_bytes);
                        }
                        let _ = reply.send(info);
                    });
                }
                Msg::Stop => {
                    let _ = writeln!(log, "--- asked to stop");
                    break 'serving;
                }
                Msg::Note(note) => {
                    let _ = writeln!(log, "{note}");
                }
                Msg::ServerGone => {
                    let _ = writeln!(log, "--- the server closed its output");
                    break 'serving;
                }
            }
            while let Some(event) = queue.pop_front() {
                if let Event::ClientDown(id) = &event {
                    let _ = writeln!(log, "client {id} left");
                    clients.remove(id);
                    if clients.is_empty() {
                        alone_since = Some(Instant::now());
                    }
                }
                for action in router.handle(event) {
                    match action {
                        Action::ToServer(message) => {
                            if server_tx.try_send(line_of(&message)).is_err() {
                                let _ = writeln!(log, "--- cannot write to the server");
                                break 'serving;
                            }
                        }
                        Action::ToClient(id, message) => {
                            // A client that cannot keep up is let go.
                            let sent = clients
                                .get(&id)
                                .is_some_and(|tx| tx.try_send(line_of(&message)).is_ok());
                            if !sent && clients.contains_key(&id) {
                                queue.push_back(Event::ClientDown(id));
                            }
                        }
                    }
                }
            }
        }
    }
    let _ = writeln!(log, "--- daemon {} ends", std::process::id());
    drop(server);
    Ok(())
}
