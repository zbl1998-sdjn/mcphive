//! What a client starts instead of the server: a pipe between the client's
//! standard input and output and the daemon of the server, which it starts when
//! there is none.

use std::{io, process::Stdio, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, Stdout, WriteHalf},
    time::Instant,
};

use crate::ipc::{self, Conn, Endpoint};

/// What the shim is asked to do.
pub struct Options {
    pub idle: Duration,
    /// Variables whose values make two otherwise equal servers different.
    pub key_env: Vec<String>,
    pub command: Vec<String>,
}

/// The key of the server this shim wants: what it runs, where, and with which of
/// the chosen variables.
#[must_use]
pub fn key(options: &Options) -> String {
    let mut parts = options.command.clone();
    parts.push(
        std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    for name in &options.key_env {
        parts.push(format!(
            "{name}={}",
            std::env::var(name).unwrap_or_default()
        ));
    }
    ipc::key_of(&parts)
}

fn start_daemon(key: &str, options: &Options) -> io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("serve")
        .arg("--key")
        .arg(key)
        .arg("--idle")
        .arg(options.idle.as_secs().to_string())
        .arg("--")
        .args(&options.command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        // The client may keep its children in a job that ends them with it, and
        // the daemon has to outlive this client. Not every job allows leaving.
        command.creation_flags(
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB,
        );
        if let Ok(child) = command.spawn() {
            return Ok(child);
        }
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        command.spawn()
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        command.spawn()
    }
}

/// What the shim reads from, and writes to, the daemon.
type Link = (BufReader<ReadHalf<Conn>>, WriteHalf<Conn>);

/// Say hello and wait for the welcome. A daemon that is about to stop does not
/// give one, and the shim then looks for another.
async fn greet(conn: Conn) -> io::Result<Link> {
    let (read, mut write) = tokio::io::split(conn);
    write
        .write_all(
            b"{\"mcphive\":\"hello\"}
",
        )
        .await?;
    write.flush().await?;
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let read = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    if read == 0 || !line.contains("welcome") {
        return Err(io::Error::from(io::ErrorKind::ConnectionAborted));
    }
    Ok((reader, write))
}

async fn connect_or_start(key: &str, options: &Options) -> io::Result<Link> {
    let endpoint = Endpoint::new(key);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut started: Option<(Instant, std::process::Child)> = None;
    loop {
        let last = match ipc::connect(&endpoint).await {
            Ok(conn) => match greet(conn).await {
                Ok(link) => return Ok(link),
                Err(error) => error,
            },
            Err(error) => error,
        };
        // A daemon that quit with an error could not run the server.
        if let Some((_, daemon)) = &mut started {
            if let Ok(Some(status)) = daemon.try_wait() {
                if !status.success() {
                    return Err(io::Error::other(format!(
                        "the server could not be started (see the logs in {})",
                        crate::daemon::log_dir().display()
                    )));
                }
            }
        }
        // Nobody is there, or the one that was is leaving: start one. Two shims
        // may do this at once; the second daemon finds the name taken and quits.
        if started
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(2))
        {
            started = Some((Instant::now(), start_daemon(key, options)?));
        }
        if Instant::now() > deadline {
            return Err(last);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Connect to the daemon (starting it if needed) and pass everything through.
/// Returns the exit status: 0 when the client closed its end, 1 when the daemon
/// went away first.
pub async fn run(options: Options) -> io::Result<i32> {
    let key = key(&options);
    let (mut from_daemon, mut to_daemon) = connect_or_start(&key, &options).await?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let up = async {
        let _ = tokio::io::copy(&mut stdin, &mut to_daemon).await;
        let _ = to_daemon.shutdown().await;
    };
    let down = pass_down(&mut from_daemon, &mut stdout);
    Ok(tokio::select! {
        () = up => 0,
        why = down => {
            eprintln!("mcphive: the shared server went away: {why}");
            1
        }
    })
}

/// Pass what the daemon says on to standard output until something ends, and
/// say what it was.
async fn pass_down(from_daemon: &mut BufReader<ReadHalf<Conn>>, stdout: &mut Stdout) -> String {
    let mut buffer = vec![0; 8192];
    loop {
        match from_daemon.read(&mut buffer).await {
            Ok(0) => return "the daemon closed the connection".to_owned(),
            Ok(read) => {
                let written = stdout.write_all(&buffer[..read]).await;
                if let Err(error) = written.and(stdout.flush().await) {
                    return format!("cannot write to standard output: {error}");
                }
            }
            Err(error) => return format!("the connection broke: {error}"),
        }
    }
}
