// SPDX-License-Identifier: MIT
//! Test double for a stdio MCP server: two catalog pages, an echo tool, and a
//! sampling request before each call (which the client must refuse).
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut refusals = 0;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let message: serde_json::Value = serde_json::from_str(&line).unwrap();
        let id = message["id"].clone();
        let reply = match message["method"].as_str() {
            Some("initialize") => {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2025-11-25", "capabilities": {}}})
            }
            Some("tools/list") if message["params"]["cursor"].is_null() => {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                {"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}], "nextCursor": "p2"}})
            }
            Some("tools/list") => {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                {"name": "time", "description": "Time", "inputSchema": {"type": "object"}}]}})
            }
            Some("tools/call") => {
                writeln!(out, "{}", serde_json::json!({"jsonrpc": "2.0", "id": "s1", "method": "sampling/createMessage", "params": {}})).unwrap();
                out.flush().unwrap();
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": message["params"]["arguments"].to_string()}], "refusals": refusals}})
            }
            None if message["error"]["code"] == -32601 => {
                refusals += 1;
                continue;
            }
            _ => continue,
        };
        writeln!(out, "{reply}").unwrap();
        out.flush().unwrap();
    }
}
