//! The local connection between the shims and the daemon of a server.
//!
//! One name per shared server: a named pipe on Windows and a Unix socket
//! elsewhere. Both carry what the MCP stdio transport carries, one JSON message
//! per line, so a shim can pass bytes through without reading them.

use std::io;

use tokio::io::{AsyncRead, AsyncWrite};

/// Both directions of a connection.
pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// A connection, from either side.
pub type Conn = Box<dyn Duplex>;

/// The key of a shared server: the same command, arguments, working directory
/// and chosen variables mean the same server. FNV-1a, because it has to give the
/// same answer in every process and every version.
#[must_use]
pub fn key_of(parts: &[String]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.bytes().chain([0u8]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// Who the servers belong to: the user, or what `MCPHIVE_NAMESPACE` says, which
/// keeps separate sets of shared servers apart (the tests use it).
fn user_name() -> String {
    let raw = std::env::var("MCPHIVE_NAMESPACE")
        .or_else(|_| std::env::var("USERNAME"))
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_owned());
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Where a server's daemon listens.
#[derive(Clone, Debug)]
pub struct Endpoint {
    key: String,
}

impl Endpoint {
    #[must_use]
    pub fn new(key: &str) -> Self {
        Self {
            key: key.to_owned(),
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    use super::{Conn, Endpoint, io, user_name};

    const PREFIX: &str = "mcphive-";

    fn pipe_name(key: &str) -> String {
        format!(r"\\.\pipe\{PREFIX}{}-{key}", user_name())
    }

    pub struct Listener {
        name: String,
        next: NamedPipeServer,
    }

    impl Listener {
        /// Fails with `AlreadyExists` or `PermissionDenied` when a daemon is
        /// already listening under this name.
        pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
            let name = pipe_name(&endpoint.key);
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)?;
            Ok(Self { name, next })
        }

        pub async fn accept(&mut self) -> io::Result<Conn> {
            self.next.connect().await?;
            let fresh = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?;
            Ok(Box::new(std::mem::replace(&mut self.next, fresh)))
        }
    }

    pub async fn connect(endpoint: &Endpoint) -> io::Result<Conn> {
        let name = pipe_name(&endpoint.key);
        // ERROR_PIPE_BUSY: every instance has a client for a moment.
        for _ in 0..50 {
            match ClientOptions::new().open(&name) {
                Ok(client) => return Ok(Box::new(client)),
                Err(error) if error.raw_os_error() == Some(231) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the daemon is busy",
        ))
    }

    /// The keys of the daemons that are listening for this user.
    pub fn list() -> Vec<String> {
        let prefix = format!("{PREFIX}{}-", user_name());
        let mut keys = Vec::new();
        if let Ok(entries) = std::fs::read_dir(r"\\.\pipe\") {
            for entry in entries.flatten() {
                if let Some(key) = entry.file_name().to_string_lossy().strip_prefix(&prefix) {
                    keys.push(key.to_owned());
                }
            }
        }
        keys.sort();
        keys
    }
}

#[cfg(unix)]
#[allow(unsafe_code, reason = "flock is a system call")]
mod platform {
    use std::{fs::File, os::fd::AsRawFd, os::unix::fs::PermissionsExt, path::PathBuf};

    use tokio::net::{UnixListener, UnixStream};

    use super::{Conn, Endpoint, io, user_name};

    fn directory() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
            || std::env::temp_dir().join(format!("mcphive-{}", user_name())),
            PathBuf::from,
        );
        base.join("mcphive")
    }

    fn socket_path(key: &str) -> PathBuf {
        directory().join(format!("{key}.sock"))
    }

    /// Take the lock of a key without waiting. Whoever holds it is the daemon of
    /// that key, and it is let go when the process ends, however it ends.
    fn try_lock(file: &File) -> io::Result<bool> {
        // SAFETY: flock takes a file descriptor that is open for as long as `file` is.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(false)
        } else {
            Err(error)
        }
    }

    pub struct Listener {
        socket: UnixListener,
        path: PathBuf,
        /// Held for as long as the daemon lives.
        _lock: File,
    }

    impl Listener {
        pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
            let dir = directory();
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            // Two daemons that start together must not both bind: the one that
            // gets the lock does, and the other finds it taken.
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join(format!("{}.lock", endpoint.key)))?;
            if !try_lock(&lock)? {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            // A socket file that is still there is left from a daemon that
            // crashed: nobody else holds the lock, so nobody listens on it.
            let path = socket_path(&endpoint.key);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let socket = UnixListener::bind(&path)?;
            Ok(Self {
                socket,
                path,
                _lock: lock,
            })
        }

        pub async fn accept(&mut self) -> io::Result<Conn> {
            let (stream, _) = self.socket.accept().await?;
            Ok(Box::new(stream))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub async fn connect(endpoint: &Endpoint) -> io::Result<Conn> {
        Ok(Box::new(
            UnixStream::connect(socket_path(&endpoint.key)).await?,
        ))
    }

    /// The keys of the daemons that answer: a socket file that nobody listens on
    /// is left from a crash, and is left out.
    pub fn list() -> Vec<String> {
        let mut keys: Vec<String> = std::fs::read_dir(directory())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let key = name.strip_suffix(".sock")?.to_owned();
                std::os::unix::net::UnixStream::connect(entry.path())
                    .ok()
                    .map(|_| key)
            })
            .collect();
        keys.sort();
        keys
    }
}

pub use platform::{Listener, connect, list};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_stable_and_depends_on_every_part() {
        let a = key_of(&["npx".into(), "-y".into(), "server".into()]);
        assert_eq!(a, key_of(&["npx".into(), "-y".into(), "server".into()]));
        assert_eq!(a.len(), 16);
        assert_ne!(a, key_of(&["npx".into(), "-y".into(), "other".into()]));
        // Where the parts split matters: "ab" "c" is not "a" "bc".
        assert_ne!(
            key_of(&["ab".into(), "c".into()]),
            key_of(&["a".into(), "bc".into()])
        );
        // Values worked out separately (FNV-1a over each part and a zero byte), so
        // that a change of the function, which would orphan running daemons, is noticed.
        assert_eq!(key_of(&["x".into()]), "08f0d807b58d20e5");
        assert_eq!(a, "bd83b7d4693c97b8");
    }
}
