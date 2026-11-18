// SPDX-License-Identifier: MIT
//! MCP transports against a fake stdio server and a loopback HTTP server.
use enzimos_agent_harness::mcp::Mcp;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn fake() -> String {
    let exe = std::env::current_exe().unwrap();
    exe.parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/fake_mcp")
        .to_string_lossy()
        .into_owned()
}

#[tokio::test(flavor = "current_thread")]
async fn stdio_discovery_paginates_aliases_and_refuses_server_requests() {
    tokio::task::LocalSet::new().run_until(async {
        let mcp = Mcp::new(json!({"files": {"enabled": true, "argv": [fake()]}, "off": {"enabled": false, "argv": ["/bin/false"]}})).unwrap();
        mcp.start().await.unwrap();
        let names = mcp.schemas.borrow().iter().map(|s| s["function"]["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert_eq!(names, ["mcp_files_echo", "mcp_files_time"]);
        let result = mcp.call("mcp_files_echo", json!({"x": 1})).await.unwrap();
        assert_eq!(result["content"][0]["text"], "{\"x\":1}");
        let again = mcp.call("mcp_files_echo", json!({})).await.unwrap();
        assert_eq!(again["refusals"], 1, "the sampling request was refused");
        assert!(mcp.call("mcp_files_unknown", json!({})).await.is_err());
        mcp.close().await;
    }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn selection_and_identity_rules() {
    tokio::task::LocalSet::new().run_until(async {
        let selected = Mcp::new(json!({"files": {"enabled": true, "argv": [fake()], "selected_tools": ["time"]}})).unwrap();
        selected.start().await.unwrap();
        assert_eq!(selected.schemas.borrow().len(), 1);
        selected.close().await;
        for config in [json!({"Bad": {"enabled": true, "argv": [fake()]}}), json!({"files": {"enabled": true, "argv": ["relative"]}}),
                       json!({"files": {"enabled": true, "argv": [fake()], "selected_tools": ["missing"]}}),
                       json!({"files": {"enabled": true, "argv": [fake()], "environment": {"LD_PRELOAD": "x"}}})] {
            let mcp = Mcp::new(config.clone());
            assert!(match mcp { Ok(m) => m.start().await.is_err(), Err(_) => true }, "{config}");
        }
    }).await;
}

/// A loopback Streamable HTTP server answering in SSE with a session id.
async fn http_server() -> (u16, std::rc::Rc<std::cell::RefCell<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = log.clone();
    tokio::task::spawn_local(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                data.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&data).into_owned();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&data).into_owned();
            seen.borrow_mut().push(text.clone());
            let body: Value = serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap_or("null"))
                .unwrap_or(Value::Null);
            let response = match body["method"].as_str() {
                Some("initialize") => format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: abc\r\nContent-Length: {0}\r\n\r\n{1}",
                    json!({"jsonrpc":"2.0","id":body["id"],"result":{"protocolVersion":"2025-06-18"}}).to_string().len(), json!({"jsonrpc":"2.0","id":body["id"],"result":{"protocolVersion":"2025-06-18"}})),
                Some("tools/list" | "tools/call") => {
                    let result = if body["method"] == "tools/list" { json!({"tools": [{"name": "add", "inputSchema": {"type": "object"}}]}) } else { json!({"sum": 3}) };
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: message\r\ndata: {}\r\n\r\n", json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
                }
                _ => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n".into(),
            };
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (port, log)
}

#[tokio::test(flavor = "current_thread")]
async fn http_sessions_sse_and_endpoint_rules() {
    tokio::task::LocalSet::new().run_until(async {
        let (port, log) = http_server().await;
        let mcp = Mcp::new(json!({"web": {"enabled": true, "transport": "http", "url": format!("http://127.0.0.1:{port}/mcp"), "authorization": "Bearer t"}})).unwrap();
        mcp.start().await.unwrap();
        assert_eq!(mcp.call("mcp_web_add", json!({"a": 1})).await.unwrap(), json!({"sum": 3}));
        let requests = log.borrow().clone();
        assert!(requests.iter().skip(1).all(|r| r.contains("MCP-Session-Id: abc") && r.contains("MCP-Protocol-Version: 2025-06-18") && r.contains("Authorization: Bearer t")));
        mcp.close().await;
        for url in ["http://example.com/mcp", "https://user:pw@example.com/mcp", "https://example.com/mcp#x"] {
            let bad = Mcp::new(json!({"web": {"enabled": true, "transport": "http", "url": url}})).unwrap();
            assert!(bad.start().await.is_err(), "{url}");
        }
    }).await;
}
