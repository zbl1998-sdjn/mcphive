//! A small MCP server to try mcphive with, and to test it against.
//!
//! Tools: `echo` (returns its `text`), `pid` (the id of this process, which is
//! the same for every client when the server is shared) and `slow` (three steps
//! of progress, when the caller asks for them).

use std::{
    io::{self, BufRead, Write},
    time::Duration,
};

use serde_json::{Value, json};

fn send(out: &mut impl Write, message: &Value) -> io::Result<()> {
    writeln!(out, "{message}")?;
    out.flush()
}

fn text_result(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": false})
}

fn tools() -> Value {
    json!([
        {"name": "echo", "description": "Return the text that was given.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
        {"name": "pid", "description": "The id of the server process.", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "slow", "description": "Three steps of progress, then done.", "inputSchema": {"type": "object", "properties": {}}},
    ])
}

/// Serve on standard input and output until the input closes.
pub fn run() -> io::Result<()> {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (Some(method), id) = (
            message.get("method").and_then(Value::as_str),
            message.get("id"),
        ) else {
            continue;
        };
        let Some(id) = id else {
            continue; // a notification
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or_else(|| json!("2025-06-18")),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "mcphive-demo", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => match params.get("name").and_then(Value::as_str) {
                Some("echo") => {
                    let text = params
                        .pointer("/arguments/text")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    Ok(text_result(text))
                }
                Some("pid") => Ok(text_result(&std::process::id().to_string())),
                Some("slow") => {
                    let token = params.pointer("/_meta/progressToken").cloned();
                    for step in 1..=3 {
                        if let Some(token) = &token {
                            send(
                                &mut out,
                                &json!({"jsonrpc": "2.0", "method": "notifications/progress",
                                        "params": {"progressToken": token, "progress": step, "total": 3}}),
                            )?;
                        }
                        std::thread::sleep(Duration::from_millis(30));
                    }
                    Ok(text_result("done"))
                }
                _ => Err((-32602, "unknown tool")),
            },
            _ => Err((-32601, "method not found")),
        };
        let reply = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        send(&mut out, &reply)?;
    }
    Ok(())
}
