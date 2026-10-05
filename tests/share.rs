//! The shims and the daemon against a real server (the demo server of this very
//! binary). Each test has a namespace of its own, so tests and the user's own
//! shared servers cannot meet.

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_mcphive");
const WAIT: Duration = Duration::from_secs(15);

fn namespace(test: &str) -> String {
    format!("t{}_{test}", std::process::id())
}

fn workdir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mcphive-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a working directory");
    dir
}

/// `mcphive` with the namespace of the test.
fn hive(ns: &str) -> Command {
    let mut command = Command::new(BIN);
    command.env("MCPHIVE_NAMESPACE", ns);
    command
}

/// A client: a shim with its standard input and output in hand.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    id: u64,
}

impl Client {
    fn start(ns: &str, idle: u64, cwd: &Path) -> Self {
        let mut child = hive(ns)
            .current_dir(cwd)
            .args(["run", "--idle", &idle.to_string(), "--", BIN, "demo-server"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the shim starts");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
            id: 0,
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("still open");
        writeln!(stdin, "{message}").expect("write");
        stdin.flush().expect("flush");
    }

    fn recv(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(WAIT)
            .expect("a message within the time");
        serde_json::from_str(&line).expect("the shim only writes JSON")
    }

    /// Send a request with an id of the caller's choice and read until its
    /// answer, keeping the notifications that came before it.
    fn call_with_id(&mut self, id: &Value, method: &str, params: &Value) -> (Value, Vec<Value>) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut notes = Vec::new();
        loop {
            let message = self.recv();
            if message.get("id") == Some(id) && message.get("method").is_none() {
                return (message, notes);
            }
            notes.push(message);
        }
    }

    fn call(&mut self, method: &str, params: &Value) -> Value {
        self.id += 1;
        let id = json!(self.id);
        self.call_with_id(&id, method, params).0
    }

    fn handshake(&mut self) -> Value {
        let answer = self.call("initialize", &json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}));
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        answer
    }

    fn tool(&mut self, name: &str, arguments: &Value) -> String {
        let answer = self.call("tools/call", &json!({"name": name, "arguments": arguments}));
        answer["result"]["content"][0]["text"]
            .as_str()
            .expect("a text result")
            .to_owned()
    }

    fn leave(mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

fn status(ns: &str) -> String {
    let output = hive(ns).arg("status").output().expect("status runs");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stop_all(ns: &str) {
    let _ = hive(ns).args(["stop", "--all"]).output();
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < WAIT, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn process_exists(pid: u32) -> bool {
    if cfg!(windows) {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("tasklist runs");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

#[test]
fn two_clients_use_one_server_process() {
    let ns = namespace("share");
    let cwd = workdir("share");
    let mut a = Client::start(&ns, 2, &cwd);
    let mut b = Client::start(&ns, 2, &cwd);
    let (init_a, init_b) = (a.handshake(), b.handshake());
    assert_eq!(init_a["result"]["serverInfo"]["name"], "mcphive-demo");
    assert_eq!(
        init_a["result"], init_b["result"],
        "both get the same handshake"
    );
    assert_eq!(a.tool("echo", &json!({"text": "from a"})), "from a");
    assert_eq!(b.tool("echo", &json!({"text": "from b"})), "from b");
    let (pid_a, pid_b) = (a.tool("pid", &json!({})), b.tool("pid", &json!({})));
    assert_eq!(pid_a, pid_b, "one process serves both");
    // A direct run is a different process, so the numbers really tell them apart.
    let mut direct = Client::start(&ns, 2, &workdir("share-other"));
    direct.handshake();
    assert_ne!(direct.tool("pid", &json!({})), pid_a);
    let listed = status(&ns);
    assert_eq!(
        listed
            .lines()
            .filter(|l| l.contains("(+1 arguments)"))
            .count(),
        2,
        "two servers: {listed}"
    );
    a.leave();
    b.leave();
    direct.leave();
    stop_all(&ns);
}

#[test]
fn ids_and_progress_stay_with_the_client_that_asked() {
    let ns = namespace("progress");
    let cwd = workdir("progress");
    let mut a = Client::start(&ns, 2, &cwd);
    let mut b = Client::start(&ns, 2, &cwd);
    a.handshake();
    b.handshake();
    // Both use the id 1 and the progress token "t", at the same time.
    let params = json!({"name": "slow", "arguments": {}, "_meta": {"progressToken": "t"}});
    let request =
        |id: i64| json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params});
    a.send(&request(1));
    b.send(&request(1));
    for client in [&a, &b] {
        let mut progress = 0;
        loop {
            let message = client.recv();
            if message["method"] == "notifications/progress" {
                assert_eq!(
                    message["params"]["progressToken"], "t",
                    "the client's own token"
                );
                progress += 1;
            } else {
                assert_eq!(message["id"], 1, "the client's own id");
                assert_eq!(message["result"]["content"][0]["text"], "done");
                break;
            }
        }
        assert_eq!(progress, 3, "each client sees its own three steps, not six");
    }
    a.leave();
    b.leave();
    stop_all(&ns);
}

#[test]
fn a_client_that_leaves_does_not_disturb_the_others() {
    let ns = namespace("leave");
    let cwd = workdir("leave");
    let mut a = Client::start(&ns, 5, &cwd);
    let mut b = Client::start(&ns, 5, &cwd);
    a.handshake();
    b.handshake();
    a.send(&json!({"jsonrpc": "2.0", "id": 99, "method": "tools/call", "params": {"name": "slow", "arguments": {}}}));
    a.leave();
    assert_eq!(b.tool("echo", &json!({"text": "still here"})), "still here");
    b.leave();
    stop_all(&ns);
}

#[test]
fn the_server_stops_when_the_last_client_has_been_gone_for_the_idle_time() {
    let ns = namespace("idle");
    let cwd = workdir("idle");
    let mut a = Client::start(&ns, 1, &cwd);
    a.handshake();
    let pid: u32 = a.tool("pid", &json!({})).parse().expect("a process id");
    assert!(process_exists(pid));
    a.leave();
    wait_until("the daemon to stop", || {
        status(&ns).contains("no shared servers")
    });
    wait_until("the server process to end with it", || !process_exists(pid));
}

#[test]
fn stop_ends_a_shared_server_at_once_and_a_new_client_starts_another() {
    let ns = namespace("stop");
    let cwd = workdir("stop");
    let mut a = Client::start(&ns, 60, &cwd);
    a.handshake();
    let first: u32 = a.tool("pid", &json!({})).parse().expect("a process id");
    let output = hive(&ns)
        .args(["stop", "--all"])
        .output()
        .expect("stop runs");
    assert!(output.status.success());
    wait_until("the server process to end", || !process_exists(first));
    // The client sees its server go away, the way it would if the server had crashed.
    wait_until("the shim to exit", || {
        a.child.try_wait().ok().flatten().is_some()
    });
    assert_eq!(a.child.wait().expect("exit").code(), Some(1));
    let mut again = Client::start(&ns, 60, &cwd);
    again.handshake();
    let second: u32 = again.tool("pid", &json!({})).parse().expect("a process id");
    assert_ne!(first, second);
    again.leave();
    stop_all(&ns);
}

#[test]
fn a_server_that_cannot_start_is_reported_to_the_client() {
    let ns = namespace("broken");
    let cwd = workdir("broken");
    let mut shim = hive(&ns)
        .current_dir(&cwd)
        .args(["run", "--idle", "1", "--", "mcphive-no-such-program"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the shim starts");
    wait_until("the shim to give up", || {
        shim.try_wait().ok().flatten().is_some()
    });
    assert_ne!(shim.wait().expect("exit").code(), Some(0));
    stop_all(&ns);
}

#[test]
fn a_client_that_arrives_while_the_server_retires_is_not_dropped() {
    // The daemon stops one idle second after its last client. A shim that comes
    // at that moment must get a server, not a connection that is cut off, so
    // come back at times around the second and expect an answer each time.
    let ns = namespace("retire");
    let cwd = workdir("retire");
    for delay in [900, 950, 1000, 1030, 1060, 1100, 1150] {
        let mut leaving = Client::start(&ns, 1, &cwd);
        leaving.handshake();
        leaving.leave();
        std::thread::sleep(Duration::from_millis(delay));
        let mut arriving = Client::start(&ns, 1, &cwd);
        arriving.handshake();
        assert_eq!(
            arriving.tool("echo", &json!({"text": "ok"})),
            "ok",
            "after {delay} ms"
        );
        arriving.leave();
    }
    stop_all(&ns);
}
