//! `wrap` and `unwrap`: put `mcphive run --` in front of the servers in the
//! settings of a client, and take it out again.
//!
//! The settings are the user's own files, so nothing is written without
//! `--apply`; a preview says what would change. A file is copied before it is
//! changed, written to a temporary file and renamed, and left alone when it
//! changed while this ran. Only the program of a server is ever shown, never
//! its arguments or its environment, which can hold tokens.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
    time::SystemTime,
};

use serde_json::{Map, Value};
use toml_edit::{Array, DocumentMut, Item};

/// Servers that keep something for each client (a browser, a shell). Sharing
/// them would let the clients drive one browser, so they are left alone unless
/// asked for. Skipping is the safe side of a wrong guess.
const STATEFUL: [&str; 10] = [
    "playwright",
    "puppeteer",
    "chrome-devtools",
    "browser",
    "terminal",
    "shell",
    "repl",
    "computer-use",
    "desktop-commander",
    "tmux",
];

/// The clients whose settings are known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Client {
    ClaudeCode,
    ClaudeDesktop,
    Cursor,
    Codex,
}

/// What a run of `wrap` or `unwrap` is asked to do.
pub struct Options {
    pub files: Vec<PathBuf>,
    pub clients: Vec<Client>,
    pub apply: bool,
    pub include_stateful: bool,
    pub idle: Option<u64>,
    /// The program that goes in front; this program when it is not given.
    pub command: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Wrap,
    Unwrap,
}

/// One server as the settings describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Spec {
    command: String,
    args: Vec<String>,
    /// The names of its environment variables.
    env: Vec<String>,
}

/// Why a server is left as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Skip {
    NotStdio,
    NoCommand,
    Unreadable,
    AlreadyWrapped,
    Stateful,
    NotWrapped,
}

impl Skip {
    fn reason(self) -> &'static str {
        match self {
            Self::NotStdio => "not a server that runs as a program",
            Self::NoCommand => "has no command",
            Self::Unreadable => "its arguments are not a list of text",
            Self::AlreadyWrapped => "already runs through mcphive",
            Self::Stateful => {
                "holds something for each client (use --include-stateful to wrap it anyway)"
            }
            Self::NotWrapped => "does not run through mcphive",
        }
    }
}

/// The name of a program without its folder and its `.exe`.
fn program_name(command: &str) -> String {
    let file = command.rsplit(['/', '\\']).next().unwrap_or(command);
    let lower = file.to_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_owned()
}

fn is_mcphive(spec: &Spec) -> bool {
    program_name(&spec.command) == "mcphive" && spec.args.first().map(String::as_str) == Some("run")
}

fn holds_state(name: &str, spec: &Spec) -> bool {
    std::iter::once(name)
        .chain(std::iter::once(spec.command.as_str()))
        .chain(spec.args.iter().map(String::as_str))
        .any(|text| {
            let lower = text.to_lowercase();
            STATEFUL.iter().any(|word| lower.contains(word))
        })
}

/// What `mcphive run` takes in front of the command.
fn wrap_spec(
    name: &str,
    spec: &Spec,
    hive: &str,
    idle: Option<u64>,
    include_stateful: bool,
) -> Result<Spec, Skip> {
    if is_mcphive(spec) {
        return Err(Skip::AlreadyWrapped);
    }
    if !include_stateful && holds_state(name, spec) {
        return Err(Skip::Stateful);
    }
    let mut args = vec!["run".to_owned()];
    if let Some(seconds) = idle {
        args.push("--idle".to_owned());
        args.push(seconds.to_string());
    }
    // A variable of the server is part of what makes it this server: two
    // clients with different keys must not share one.
    for variable in &spec.env {
        args.push("--key-env".to_owned());
        args.push(variable.clone());
    }
    args.push("--".to_owned());
    args.push(spec.command.clone());
    args.extend(spec.args.iter().cloned());
    Ok(Spec {
        command: hive.to_owned(),
        args,
        env: spec.env.clone(),
    })
}

/// The command a wrapped server had.
fn unwrap_spec(spec: &Spec) -> Result<Spec, Skip> {
    if !is_mcphive(spec) {
        return Err(Skip::NotWrapped);
    }
    let mut index = 1;
    while spec.args.get(index).map(String::as_str) != Some("--") {
        let argument = spec.args.get(index).ok_or(Skip::NotWrapped)?;
        index += match argument.as_str() {
            "--idle" | "--key-env" => 2,
            other if other.starts_with("--idle=") || other.starts_with("--key-env=") => 1,
            _ => return Err(Skip::NotWrapped),
        };
    }
    let rest = &spec.args[index + 1..];
    let command = rest.first().ok_or(Skip::NotWrapped)?.clone();
    Ok(Spec {
        command,
        args: rest[1..].to_vec(),
        env: spec.env.clone(),
    })
}

/// What one server turned out to be.
enum Verdict {
    Changed(Spec),
    Skipped(Skip),
}

fn decide(mode: Mode, name: &str, spec: &Spec, options: &Options, hive: &str) -> Verdict {
    let result = match mode {
        Mode::Wrap => wrap_spec(name, spec, hive, options.idle, options.include_stateful),
        Mode::Unwrap => unwrap_spec(spec),
    };
    match result {
        Ok(new) => Verdict::Changed(new),
        Err(skip) => Verdict::Skipped(skip),
    }
}

/// One line of the report.
struct Line {
    scope: String,
    name: String,
    text: String,
}

fn describe(spec: &Spec) -> String {
    let program = program_name(&spec.command);
    format!("{program} (+{} arguments)", spec.args.len())
}

fn report_line(scope: &str, name: &str, mode: Mode, old: &Spec, verdict: &Verdict) -> Line {
    let text = match verdict {
        Verdict::Changed(new) => {
            let keys: Vec<_> = new
                .args
                .windows(2)
                .filter(|pair| pair[0] == "--key-env")
                .map(|pair| pair[1].as_str())
                .collect();
            let note = if mode == Mode::Wrap && !keys.is_empty() {
                format!("  [key-env {}]", keys.join(", "))
            } else {
                String::new()
            };
            let verb = if mode == Mode::Wrap {
                "wrap  "
            } else {
                "unwrap"
            };
            format!("{verb} {}{note}", describe(old))
        }
        Verdict::Skipped(skip) => format!("skip   {}", skip.reason()),
    };
    Line {
        scope: scope.to_owned(),
        name: name.to_owned(),
        text,
    }
}

// --- JSON: Claude Code, Claude Desktop, Cursor ---------------------------

fn json_spec(entry: &Map<String, Value>) -> Result<Spec, Skip> {
    let remote = entry.get("url").is_some()
        || entry
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "stdio");
    if remote {
        return Err(Skip::NotStdio);
    }
    let command = entry
        .get("command")
        .and_then(Value::as_str)
        .ok_or(Skip::NoCommand)?
        .to_owned();
    let args = match entry.get("args") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or(Skip::Unreadable)?,
        Some(_) => return Err(Skip::Unreadable),
    };
    let env = entry
        .get("env")
        .and_then(Value::as_object)
        .map(|variables| variables.keys().cloned().collect())
        .unwrap_or_default();
    Ok(Spec { command, args, env })
}

/// Every `mcpServers` object of a JSON settings file: the one at the top, and
/// the ones Claude Code keeps for each project in `~/.claude.json`.
fn server_maps(root: &mut Value) -> Vec<(String, &mut Map<String, Value>)> {
    let mut maps = Vec::new();
    let Value::Object(top) = root else {
        return maps;
    };
    for (key, value) in top.iter_mut() {
        match (key.as_str(), value) {
            ("mcpServers", Value::Object(servers)) => maps.push((String::new(), servers)),
            ("projects", Value::Object(projects)) => {
                for (path, project) in projects.iter_mut() {
                    if let Some(Value::Object(servers)) = project.get_mut("mcpServers") {
                        maps.push((path.clone(), servers));
                    }
                }
            }
            _ => {}
        }
    }
    maps
}

/// Change the servers of a JSON file in place and say what was done.
fn transform_json(root: &mut Value, mode: Mode, options: &Options, hive: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    for (scope, servers) in server_maps(root) {
        for (name, entry) in servers.iter_mut() {
            let Value::Object(entry) = entry else {
                continue;
            };
            let spec = match json_spec(entry) {
                Ok(spec) => spec,
                Err(skip) => {
                    let verdict = Verdict::Skipped(skip);
                    let blank = Spec {
                        command: String::new(),
                        args: Vec::new(),
                        env: Vec::new(),
                    };
                    lines.push(report_line(&scope, name, mode, &blank, &verdict));
                    continue;
                }
            };
            let verdict = decide(mode, name, &spec, options, hive);
            lines.push(report_line(&scope, name, mode, &spec, &verdict));
            if let Verdict::Changed(new) = verdict {
                entry.insert("command".to_owned(), Value::String(new.command));
                // A server that had no arguments and has none again keeps none.
                if !new.args.is_empty() || entry.contains_key("args") {
                    let args = new.args.into_iter().map(Value::String).collect();
                    entry.insert("args".to_owned(), Value::Array(args));
                }
            }
        }
    }
    lines
}

// --- TOML: Codex ---------------------------------------------------------

fn toml_spec(table: &toml_edit::Table) -> Result<Spec, Skip> {
    if table.contains_key("url") {
        return Err(Skip::NotStdio);
    }
    let command = table
        .get("command")
        .and_then(Item::as_str)
        .ok_or(Skip::NoCommand)?
        .to_owned();
    let args = match table.get("args") {
        None => Vec::new(),
        Some(item) => item
            .as_array()
            .ok_or(Skip::Unreadable)?
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or(Skip::Unreadable)?,
    };
    let mut env: Vec<String> = Vec::new();
    if let Some(variables) = table.get("env") {
        if let Some(variables) = variables.as_table_like() {
            env.extend(variables.iter().map(|(name, _)| name.to_owned()));
        }
    }
    // `env_vars` names variables of the client that are passed on.
    if let Some(names) = table.get("env_vars").and_then(Item::as_array) {
        for value in names {
            let name = value.as_str().or_else(|| {
                value
                    .as_inline_table()
                    .and_then(|table| table.get("name"))
                    .and_then(toml_edit::Value::as_str)
            });
            if let Some(name) = name {
                env.push(name.to_owned());
            }
        }
    }
    Ok(Spec { command, args, env })
}

fn transform_toml(
    document: &mut DocumentMut,
    mode: Mode,
    options: &Options,
    hive: &str,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let Some(servers) = document
        .get_mut("mcp_servers")
        .and_then(Item::as_table_like_mut)
    else {
        return lines;
    };
    for (name, item) in servers.iter_mut() {
        let name = name.to_string();
        let Some(table) = item.as_table_mut() else {
            continue;
        };
        let spec = match toml_spec(table) {
            Ok(spec) => spec,
            Err(skip) => {
                let blank = Spec {
                    command: String::new(),
                    args: Vec::new(),
                    env: Vec::new(),
                };
                lines.push(report_line(
                    "",
                    &name,
                    mode,
                    &blank,
                    &Verdict::Skipped(skip),
                ));
                continue;
            }
        };
        let verdict = decide(mode, &name, &spec, options, hive);
        lines.push(report_line("", &name, mode, &spec, &verdict));
        if let Verdict::Changed(new) = verdict {
            table["command"] = toml_edit::value(new.command);
            if !new.args.is_empty() || table.contains_key("args") {
                let mut args = Array::new();
                for argument in new.args {
                    args.push(argument);
                }
                table["args"] = toml_edit::value(args);
            }
        }
    }
    lines
}

// --- Files ---------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Json,
    Toml,
}

fn format_of(path: &Path) -> Format {
    if path
        .extension()
        .is_some_and(|extension| extension == "toml")
    {
        Format::Toml
    } else {
        Format::Json
    }
}

fn environment(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home() -> Option<PathBuf> {
    environment("USERPROFILE").or_else(|| environment("HOME"))
}

/// The settings files of the clients that exist on this machine.
fn known_files(clients: &[Client]) -> Vec<PathBuf> {
    let home = home();
    let in_home = |parts: &[&str]| {
        home.as_ref().map(|home| {
            parts
                .iter()
                .fold(home.clone(), |path, part| path.join(part))
        })
    };
    let here = std::env::current_dir().ok();
    let in_here = |parts: &[&str]| {
        here.as_ref().map(|here| {
            parts
                .iter()
                .fold(here.clone(), |path, part| path.join(part))
        })
    };
    let mut files = Vec::new();
    for client in clients {
        match client {
            Client::ClaudeCode => {
                files.extend(in_home(&[".claude.json"]));
                files.extend(in_here(&[".mcp.json"]));
            }
            Client::ClaudeDesktop => {
                // The folders that its documentation names; there is none for Linux.
                if cfg!(windows) {
                    files.extend(
                        environment("APPDATA")
                            .map(|dir| dir.join("Claude").join("claude_desktop_config.json")),
                    );
                } else if cfg!(target_os = "macos") {
                    files.extend(in_home(&[
                        "Library",
                        "Application Support",
                        "Claude",
                        "claude_desktop_config.json",
                    ]));
                }
            }
            Client::Cursor => {
                files.extend(in_home(&[".cursor", "mcp.json"]));
                files.extend(in_here(&[".cursor", "mcp.json"]));
            }
            Client::Codex => files.extend(in_home(&[".codex", "config.toml"])),
        }
    }
    files.retain(|path| path.is_file());
    files.dedup();
    files
}

/// What changing one file came to.
struct FileReport {
    lines: Vec<Line>,
    changed: bool,
    written: Option<PathBuf>,
}

fn modified(path: &Path) -> io::Result<SystemTime> {
    fs::metadata(path)?.modified()
}

/// A path without the `\\?\` that Windows puts in front of a resolved one, which
/// is only noise when it is shown. A network path keeps it.
fn plain(path: PathBuf) -> PathBuf {
    let stripped = path
        .to_string_lossy()
        .strip_prefix(r"\\?\")
        .filter(|rest| !rest.starts_with("UNC"))
        .map(PathBuf::from);
    stripped.unwrap_or(path)
}

/// A name for the copy: `file.mcphive-backup`, then `-2`, `-3` ...
fn backup_path(path: &Path) -> PathBuf {
    let name = path.file_name().map_or_else(OsString::new, OsString::from);
    let mut attempt = 1;
    loop {
        let mut candidate = name.clone();
        candidate.push(if attempt == 1 {
            ".mcphive-backup".to_owned()
        } else {
            format!(".mcphive-backup-{attempt}")
        });
        let candidate = path.with_file_name(candidate);
        if !candidate.exists() {
            return candidate;
        }
        attempt += 1;
    }
}

/// Copy the file, write the new text beside it, and rename it into place.
fn write_safely(path: &Path, text: &str, seen: SystemTime) -> io::Result<PathBuf> {
    // A settings file that is a link is changed where it points.
    let path = plain(fs::canonicalize(path)?);
    if modified(&path)? != seen {
        return Err(io::Error::other(
            "the file changed while this ran (is the client open?); nothing was written",
        ));
    }
    let backup = backup_path(&path);
    fs::copy(&path, &backup)?;
    let mut name = path.file_name().map_or_else(OsString::new, OsString::from);
    name.push(".mcphive-tmp");
    let temporary = path.with_file_name(name);
    fs::write(&temporary, text)?;
    fs::set_permissions(&temporary, fs::metadata(&path)?.permissions())?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(backup)
}

fn process_file(
    path: &Path,
    mode: Mode,
    options: &Options,
    hive: &str,
) -> Result<FileReport, String> {
    let shown = path.display();
    let seen = modified(path).map_err(|error| format!("{shown}: {error}"))?;
    let bytes = fs::read(path).map_err(|error| format!("{shown}: {error}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{shown}: not UTF-8 text"))?;
    // Some editors on Windows put a byte order mark at the start.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

    let (lines, new_text) = match format_of(path) {
        Format::Json => {
            let mut root: Value = serde_json::from_str(text)
                .map_err(|error| format!("{shown}: not valid JSON ({error})"))?;
            let lines = transform_json(&mut root, mode, options, hive);
            let mut out =
                serde_json::to_string_pretty(&root).map_err(|e| format!("{shown}: {e}"))?;
            out.push('\n');
            (lines, out)
        }
        Format::Toml => {
            let mut document: DocumentMut = text
                .parse()
                .map_err(|error| format!("{shown}: not valid TOML ({error})"))?;
            let lines = transform_toml(&mut document, mode, options, hive);
            (lines, document.to_string())
        }
    };
    let changed = lines.iter().any(|line| !line.text.starts_with("skip"));
    let written = if changed && options.apply {
        Some(write_safely(path, &new_text, seen).map_err(|error| format!("{shown}: {error}"))?)
    } else {
        None
    };
    Ok(FileReport {
        lines,
        changed,
        written,
    })
}

/// Run `wrap` or `unwrap` over the files that were named or found.
pub fn run(mode: Mode, mut options: Options) -> ExitCode {
    let hive = match options.command.clone() {
        Some(command) => command,
        None => match std::env::current_exe() {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(_) => "mcphive".to_owned(),
        },
    };
    let mut files = std::mem::take(&mut options.files);
    if files.is_empty() {
        let clients = if options.clients.is_empty() {
            vec![
                Client::ClaudeCode,
                Client::ClaudeDesktop,
                Client::Cursor,
                Client::Codex,
            ]
        } else {
            options.clients.clone()
        };
        files = known_files(&clients);
    }
    if files.is_empty() {
        println!("no settings files found: name one with --file, or a client with --client");
        return ExitCode::SUCCESS;
    }

    let mut failed = false;
    let mut any_change = false;
    for path in &files {
        println!("{}", path.display());
        match process_file(path, mode, &options, &hive) {
            Ok(report) => {
                if report.lines.is_empty() {
                    println!("  no servers");
                }
                for line in &report.lines {
                    let scope = if line.scope.is_empty() {
                        String::new()
                    } else {
                        format!(" [project {}]", line.scope)
                    };
                    println!("  {}: {}{scope}", line.name, line.text);
                }
                any_change |= report.changed;
                if let Some(backup) = report.written {
                    println!("  written; the old file is kept as {}", backup.display());
                }
            }
            Err(message) => {
                eprintln!("mcphive: {message}");
                failed = true;
            }
        }
    }
    if any_change && !options.apply {
        let undo = if mode == Mode::Wrap {
            "Run it again with --apply to change the files: each is copied first, and `mcphive unwrap` takes mcphive out again."
        } else {
            "Run it again with --apply to change the files: each is copied first."
        };
        println!("\nThis was only a preview. {undo}");
    } else if any_change {
        println!(
            "\nRestart the clients to use the change. Some of them write their settings back when they close, so close them before you apply."
        );
    }
    ExitCode::from(u8::from(failed))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn options() -> Options {
        Options {
            files: Vec::new(),
            clients: Vec::new(),
            apply: false,
            include_stateful: false,
            idle: None,
            command: None,
        }
    }

    fn spec(command: &str, args: &[&str], env: &[&str]) -> Spec {
        Spec {
            command: command.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: env.iter().map(|e| (*e).to_owned()).collect(),
        }
    }

    #[test]
    fn a_server_gets_mcphive_run_in_front_and_its_variables_as_keys() {
        let old = spec("npx", &["-y", "pkg"], &["GITHUB_TOKEN", "OTHER"]);
        let new = wrap_spec("github", &old, "C:\\bin\\mcphive.exe", None, false).expect("wrapped");
        assert_eq!(new.command, "C:\\bin\\mcphive.exe");
        assert_eq!(
            new.args,
            [
                "run",
                "--key-env",
                "GITHUB_TOKEN",
                "--key-env",
                "OTHER",
                "--",
                "npx",
                "-y",
                "pkg"
            ]
        );
        let with_idle = wrap_spec("docs", &spec("npx", &[], &[]), "mcphive", Some(30), false);
        assert_eq!(
            with_idle.expect("wrapped").args,
            ["run", "--idle", "30", "--", "npx"]
        );
    }

    #[test]
    fn unwrap_gives_back_what_wrap_took() {
        for old in [
            spec("npx", &["-y", "pkg"], &["KEY"]),
            spec("node", &["server.js", "--port", "1"], &[]),
            spec("uvx", &["--", "tool"], &[]),
        ] {
            let wrapped = wrap_spec("x", &old, "mcphive", Some(5), false).expect("wrapped");
            assert_eq!(unwrap_spec(&wrapped).expect("unwrapped"), old);
        }
    }

    #[test]
    fn a_server_that_is_wrapped_already_is_not_wrapped_twice() {
        let wrapped = spec("/usr/bin/mcphive", &["run", "--", "npx"], &[]);
        assert_eq!(
            wrap_spec("x", &wrapped, "mcphive", None, false),
            Err(Skip::AlreadyWrapped)
        );
        let windows = spec("C:\\Tools\\MCPHIVE.EXE", &["run", "--", "npx"], &[]);
        assert_eq!(
            wrap_spec("x", &windows, "mcphive", None, false),
            Err(Skip::AlreadyWrapped)
        );
    }

    #[test]
    fn unwrap_leaves_what_it_does_not_understand() {
        assert_eq!(
            unwrap_spec(&spec("npx", &["-y"], &[])),
            Err(Skip::NotWrapped)
        );
        // mcphive, but not `run`, or `run` with a flag it never wrote, or no command.
        assert_eq!(
            unwrap_spec(&spec("mcphive", &["status"], &[])),
            Err(Skip::NotWrapped)
        );
        assert_eq!(
            unwrap_spec(&spec("mcphive", &["run", "--wat", "--", "npx"], &[])),
            Err(Skip::NotWrapped)
        );
        assert_eq!(
            unwrap_spec(&spec("mcphive", &["run", "--"], &[])),
            Err(Skip::NotWrapped)
        );
        assert_eq!(
            unwrap_spec(&spec("mcphive", &["run"], &[])),
            Err(Skip::NotWrapped)
        );
    }

    #[test]
    fn servers_that_keep_state_for_each_client_are_left_alone_unless_asked() {
        let browser = spec("npx", &["-y", "@playwright/mcp"], &[]);
        assert_eq!(
            wrap_spec("web", &browser, "mcphive", None, false),
            Err(Skip::Stateful)
        );
        assert!(wrap_spec("web", &browser, "mcphive", None, true).is_ok());
        // The name counts as well as the command.
        let named = spec("node", &["server.js"], &[]);
        assert_eq!(
            wrap_spec("my-shell", &named, "mcphive", None, false),
            Err(Skip::Stateful)
        );
        assert!(
            wrap_spec(
                "docs",
                &spec("npx", &["context7"], &[]),
                "mcphive",
                None,
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn json_servers_are_found_at_the_top_and_in_projects() {
        let mut root = json!({
            "numStartups": 3,
            "mcpServers": {
                "docs": {"command": "npx", "args": ["-y", "ctx"], "env": {"KEY": "secret-value"}},
                "remote": {"type": "http", "url": "https://example.com/mcp"},
                "bare": {"command": "server"},
                "broken": {"command": "x", "args": [1]},
                "nothing": {}
            },
            "projects": {
                "/home/me/app": {"mcpServers": {"local": {"command": "node", "args": ["a.js"]}}},
                "/home/me/other": {"allowedTools": []}
            }
        });
        let lines = transform_json(&mut root, Mode::Wrap, &options(), "mcphive");
        let said: Vec<_> = lines
            .iter()
            .map(|l| format!("{}|{}", l.name, l.text))
            .collect();
        assert_eq!(said.len(), 6, "{said:?}");
        assert!(said[0].starts_with("docs|wrap"), "{said:?}");
        assert!(said[0].contains("key-env KEY"), "{said:?}");
        assert!(said[1].contains("not a server that runs as a program"));
        assert!(said[2].starts_with("bare|wrap"));
        assert!(said[3].contains("arguments are not a list"));
        assert!(said[4].contains("has no command"));
        assert!(said[5].starts_with("local|wrap"));
        assert_eq!(lines[5].scope, "/home/me/app");
        // Nothing the report says holds an argument or an environment value.
        assert!(!said.join("\n").contains("secret-value"));
        // The other keys and their order are untouched; the entry changed in place.
        assert_eq!(root["numStartups"], 3);
        assert_eq!(root["mcpServers"]["docs"]["command"], "mcphive");
        assert_eq!(root["mcpServers"]["docs"]["env"]["KEY"], "secret-value");
        assert_eq!(
            root["mcpServers"]["remote"]["url"],
            "https://example.com/mcp"
        );
        // A server without arguments gets them, because the command needs them.
        assert_eq!(
            root["mcpServers"]["bare"]["args"],
            json!(["run", "--", "server"])
        );
        let keys: Vec<_> = root.as_object().expect("object").keys().cloned().collect();
        assert_eq!(keys, ["numStartups", "mcpServers", "projects"]);
        // And back again.
        let back = transform_json(&mut root, Mode::Unwrap, &options(), "mcphive");
        assert!(back.iter().filter(|l| l.text.starts_with("unwrap")).count() >= 3);
        assert_eq!(root["mcpServers"]["docs"]["command"], "npx");
        assert_eq!(root["mcpServers"]["docs"]["args"], json!(["-y", "ctx"]));
    }

    #[test]
    fn codex_settings_keep_their_comments_and_their_other_tables() {
        let text = r#"# my settings
model = "gpt"

[mcp_servers.docs] # the docs server
command = "npx"
args = ["-y", "ctx"]
env_vars = ["API_KEY", { name = "OTHER" }]

[mcp_servers.docs.env]
TOKEN = "secret-value"

[mcp_servers.web]
url = "https://example.com/mcp"

[profiles.fast]
model = "mini"
"#;
        let mut document: DocumentMut = text.parse().expect("toml");
        let lines = transform_toml(&mut document, Mode::Wrap, &options(), "mcphive");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].text.contains("key-env"), "{}", lines[0].text);
        assert!(
            lines[1]
                .text
                .contains("not a server that runs as a program")
        );
        let out = document.to_string();
        assert!(out.starts_with("# my settings\nmodel = \"gpt\""));
        assert!(out.contains("# the docs server"));
        assert!(out.contains("[profiles.fast]"));
        assert!(out.contains(r#"command = "mcphive""#));
        for name in ["TOKEN", "API_KEY", "OTHER"] {
            assert!(out.contains(&format!("\"{name}\"")), "{name}: {out}");
        }
        assert!(
            out.contains("secret-value"),
            "the file keeps its own values"
        );
        assert!(!lines.iter().any(|l| l.text.contains("secret-value")));
        // Back again.
        let mut again: DocumentMut = out.parse().expect("toml");
        transform_toml(&mut again, Mode::Unwrap, &options(), "mcphive");
        let restored = again.to_string();
        assert!(restored.contains(r#"command = "npx""#), "{restored}");
        assert!(restored.contains(r#"args = ["-y", "ctx"]"#), "{restored}");
    }
}
