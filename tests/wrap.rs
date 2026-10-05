//! `wrap` and `unwrap` on settings files in a folder of their own: a preview
//! changes nothing, `--apply` copies the file first, and a wrapped entry still
//! runs as a shared server.

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_mcphive");

/// A folder that is removed when it goes out of scope.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("mcphive-wrap-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a folder");
        Self(path)
    }

    fn file(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("folders");
        std::fs::write(&path, content).expect("a file");
        path
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mcphive(args: &[&str]) -> Command {
    let mut command = Command::new(BIN);
    command.args(args);
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("an exit code")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("JSON")
}

const SETTINGS: &str = r#"{
  "theme": "dark",
  "mcpServers": {
    "docs": {"command": "npx", "args": ["-y", "@example/docs"], "env": {"API_KEY": "SECRET_VALUE_1"}},
    "remote": {"type": "http", "url": "https://example.com/mcp"},
    "web": {"command": "npx", "args": ["-y", "@playwright/mcp"]}
  }
}
"#;

#[test]
fn a_preview_says_what_would_change_and_changes_nothing() {
    let dir = Dir::new("preview");
    let path = dir.file("settings.json", SETTINGS);
    let output = mcphive(&["wrap", "--file", path.to_str().expect("utf-8")])
        .output()
        .expect("run");
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
    let shown = text(&output.stdout);
    assert!(shown.contains("docs: wrap"), "{shown}");
    assert!(shown.contains("key-env API_KEY"), "{shown}");
    assert!(shown.contains("remote: skip"), "{shown}");
    assert!(shown.contains("web: skip"), "{shown}");
    assert!(shown.contains("only a preview"), "{shown}");
    // Neither an argument nor a value of the environment is shown.
    assert!(!shown.contains("@example/docs") && !shown.contains("SECRET_VALUE_1"));
    assert_eq!(std::fs::read_to_string(&path).expect("read"), SETTINGS);
    let backups = std::fs::read_dir(&dir.0).expect("dir").count();
    assert_eq!(backups, 1, "only the settings file is there");
}

#[test]
fn apply_copies_the_file_first_and_wrap_then_unwrap_gives_it_back() {
    let dir = Dir::new("apply");
    let path = dir.file("settings.json", SETTINGS);
    let file = path.to_str().expect("utf-8");

    let output = mcphive(&["wrap", "--apply", "--idle", "30", "--file", file])
        .output()
        .expect("run");
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
    let backup = dir.0.join("settings.json.mcphive-backup");
    assert_eq!(std::fs::read_to_string(&backup).expect("backup"), SETTINGS);
    let wrapped = read_json(&path);
    assert_eq!(wrapped["theme"], "dark");
    assert_eq!(wrapped["mcpServers"]["docs"]["command"], BIN);
    assert_eq!(
        wrapped["mcpServers"]["docs"]["args"],
        json!([
            "run",
            "--idle",
            "30",
            "--key-env",
            "API_KEY",
            "--",
            "npx",
            "-y",
            "@example/docs"
        ])
    );
    assert_eq!(
        wrapped["mcpServers"]["docs"]["env"]["API_KEY"],
        "SECRET_VALUE_1"
    );
    // The ones that were skipped are as they were.
    assert_eq!(
        wrapped["mcpServers"]["remote"],
        read_json(&backup)["mcpServers"]["remote"]
    );
    assert_eq!(
        wrapped["mcpServers"]["web"],
        read_json(&backup)["mcpServers"]["web"]
    );

    // A second run finds nothing to wrap, and writes nothing.
    let output = mcphive(&["wrap", "--apply", "--file", file])
        .output()
        .expect("run");
    assert_eq!(code(&output), 0);
    let shown = text(&output.stdout);
    assert!(shown.contains("already runs through mcphive"), "{shown}");
    assert!(!shown.contains("written"), "{shown}");
    assert!(!dir.0.join("settings.json.mcphive-backup-2").exists());

    // Unwrap gives the servers back as they were.
    let output = mcphive(&["unwrap", "--apply", "--file", file])
        .output()
        .expect("run");
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
    assert_eq!(read_json(&path), read_json(&backup));
    // It copied the wrapped file first, under a name of its own.
    assert!(dir.0.join("settings.json.mcphive-backup-2").exists());
}

#[test]
fn a_wrapped_entry_still_starts_a_shared_server() {
    let dir = Dir::new("works");
    let entry = json!({"mcpServers": {"demo": {"command": BIN, "args": ["demo-server"]}}});
    let path = dir.file("settings.json", &entry.to_string());
    let output = mcphive(&["wrap", "--apply", "--file", path.to_str().expect("utf-8")])
        .output()
        .expect("run");
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
    let wrapped = read_json(&path);
    let command = wrapped["mcpServers"]["demo"]["command"]
        .as_str()
        .expect("command");
    let args: Vec<_> = wrapped["mcpServers"]["demo"]["args"]
        .as_array()
        .expect("args")
        .iter()
        .map(|a| a.as_str().expect("text").to_owned())
        .collect();

    // Run it as a client would, in a namespace of its own.
    let namespace = format!("wrap_{}", std::process::id());
    let mut child = Command::new(command)
        .args(&args)
        .env("MCPHIVE_NAMESPACE", &namespace)
        .current_dir(&dir.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the shim starts");
    let mut stdin = child.stdin.take().expect("stdin");
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                      "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "test", "version": "0"}}});
    writeln!(stdin, "{init}").expect("write");
    stdin.flush().expect("flush");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("an answer");
    let answer: Value = serde_json::from_str(&line).expect("JSON");
    assert_eq!(
        answer["result"]["serverInfo"]["name"], "mcphive-demo",
        "{line}"
    );
    drop(stdin);
    let _ = child.wait();
    let _ = Command::new(BIN)
        .args(["stop", "--all"])
        .env("MCPHIVE_NAMESPACE", &namespace)
        .output();
}

#[test]
fn codex_settings_keep_their_comments() {
    let dir = Dir::new("codex");
    let text_in = "# my settings\nmodel = \"gpt\"\n\n[mcp_servers.docs] # the docs\ncommand = \"npx\"\nargs = [\"-y\", \"ctx\"]\n\n[profiles.fast]\nmodel = \"mini\"\n";
    let path = dir.file("config.toml", text_in);
    let file = path.to_str().expect("utf-8");
    let output = mcphive(&["wrap", "--apply", "--file", file])
        .output()
        .expect("run");
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
    let wrapped = std::fs::read_to_string(&path).expect("read");
    assert!(
        wrapped.starts_with("# my settings\nmodel = \"gpt\""),
        "{wrapped}"
    );
    assert!(wrapped.contains("# the docs"), "{wrapped}");
    assert!(wrapped.contains("[profiles.fast]"), "{wrapped}");
    assert!(
        wrapped.contains("\"--\", \"npx\", \"-y\", \"ctx\""),
        "{wrapped}"
    );
    let output = mcphive(&["unwrap", "--apply", "--file", file])
        .output()
        .expect("run");
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
    let restored = std::fs::read_to_string(&path).expect("read");
    assert!(restored.contains("command = \"npx\""), "{restored}");
    assert!(restored.contains("args = [\"-y\", \"ctx\"]"), "{restored}");
}

#[test]
fn the_settings_of_the_clients_are_found_without_naming_them() {
    let dir = Dir::new("found");
    dir.file(
        ".claude.json",
        r#"{"mcpServers": {"a": {"command": "npx"}}}"#,
    );
    dir.file(
        ".cursor/mcp.json",
        r#"{"mcpServers": {"b": {"command": "npx"}}}"#,
    );
    dir.file(".codex/config.toml", "[mcp_servers.c]\ncommand = \"npx\"\n");
    let output = mcphive(&["wrap"])
        .env("HOME", &dir.0)
        .env("USERPROFILE", &dir.0)
        .env("APPDATA", dir.0.join("AppData").join("Roaming"))
        .current_dir(&dir.0)
        .output()
        .expect("run");
    assert_eq!(code(&output), 0, "{}", text(&output.stderr));
    let shown = text(&output.stdout);
    for name in [".claude.json", "mcp.json", "config.toml"] {
        assert!(shown.contains(name), "{name} not found: {shown}");
    }
    assert!(shown.contains("a: wrap") && shown.contains("b: wrap") && shown.contains("c: wrap"));
    // A client that is named is the only one that is looked at.
    let output = mcphive(&["wrap", "--client", "codex"])
        .env("HOME", &dir.0)
        .env("USERPROFILE", &dir.0)
        .current_dir(&dir.0)
        .output()
        .expect("run");
    let shown = text(&output.stdout);
    assert!(
        shown.contains("c: wrap") && !shown.contains("a: wrap"),
        "{shown}"
    );
}

#[test]
fn a_file_that_cannot_be_read_is_reported_and_the_others_still_go_on() {
    let dir = Dir::new("broken");
    let bad = dir.file("bad.json", "{ not json");
    let good = dir.file("good.json", r#"{"mcpServers": {"a": {"command": "npx"}}}"#);
    let output = mcphive(&[
        "wrap",
        "--file",
        bad.to_str().expect("utf-8"),
        "--file",
        good.to_str().expect("utf-8"),
        "--file",
        dir.0.join("missing.json").to_str().expect("utf-8"),
    ])
    .output()
    .expect("run");
    assert_eq!(code(&output), 1);
    assert!(
        text(&output.stderr).contains("not valid JSON"),
        "{}",
        text(&output.stderr)
    );
    assert!(
        text(&output.stdout).contains("a: wrap"),
        "{}",
        text(&output.stdout)
    );
}
