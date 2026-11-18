// SPDX-License-Identifier: MIT
//! Bounded Streamable HTTP MCP: explicit endpoints, no redirects, no replay.
use crate::{Result, ToolError, fail, provider};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};
use tokio::io::{AsyncWriteExt, BufReader};

const LIMIT: usize = 1024 * 1024;
pub const VERSION: &str = "2025-11-25";

trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> Stream for T {}

pub struct HttpClient {
    tls: bool,
    host: String,
    port: u16,
    target: String,
    authorization: Option<String>,
    session: RefCell<Option<String>>,
    protocol: RefCell<Option<String>>,
    request_id: Cell<u64>,
    lock: tokio::sync::Mutex<()>,
    closed: Cell<bool>,
}

fn printable(value: &str, low: u8) -> bool {
    value.bytes().all(|c| (low..=126).contains(&c))
}

impl HttpClient {
    pub fn new(url: &str, authorization: Option<&str>) -> Result<Rc<Self>> {
        if url.len() > 4096 {
            return fail("invalid MCP endpoint");
        }
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| ToolError("invalid MCP endpoint".into()))?;
        if rest.contains('#') {
            return fail("MCP endpoint must not contain credentials or fragments");
        }
        let (authority, target) = rest
            .find('/')
            .map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
        let authority = authority.split_once('?').map_or(authority, |(a, _)| a);
        if authority.contains('@') || authority.is_empty() {
            return fail("MCP endpoint must not contain credentials or fragments");
        }
        let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
            let (host, after) = inner
                .split_once(']')
                .ok_or_else(|| ToolError("invalid MCP endpoint".into()))?;
            (
                host.to_owned(),
                after
                    .strip_prefix(':')
                    .map(str::parse::<u16>)
                    .transpose()
                    .map_err(|_| ToolError("invalid MCP endpoint".into()))?,
            )
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (
                    h.to_owned(),
                    Some(
                        p.parse::<u16>()
                            .map_err(|_| ToolError("invalid MCP endpoint".into()))?,
                    ),
                ),
                None => (authority.to_owned(), None),
            }
        };
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "https" && !(scheme == "http" && (host == "127.0.0.1" || host == "::1")) {
            return fail("MCP requires verified HTTPS or literal loopback HTTP");
        }
        if let Some(a) = authorization
            && (a.is_empty() || a.len() > 4096 || !printable(a, 32))
        {
            return fail("invalid explicit MCP authorization");
        }
        let tls = scheme == "https";
        let target = if target.is_empty() {
            "/".to_owned()
        } else {
            target.to_owned()
        };
        if !printable(&target, 33) {
            return fail("invalid MCP endpoint path");
        }
        Ok(Rc::new(Self {
            tls,
            port: port.unwrap_or(if tls { 443 } else { 80 }),
            host,
            target,
            authorization: authorization.map(str::to_owned),
            session: RefCell::new(None),
            protocol: RefCell::new(None),
            request_id: Cell::new(0),
            lock: tokio::sync::Mutex::new(()),
            closed: Cell::new(false),
        }))
    }

    pub async fn start(self: &Rc<Self>) -> Result<()> {
        let result = self.request("initialize", json!({"protocolVersion": VERSION, "capabilities": {}, "clientInfo": {"name": "enzimos-agent-harness", "version": "0.2.0"}})).await?;
        let protocol = result["protocolVersion"].as_str().unwrap_or_default();
        if ![VERSION, "2025-06-18", "2025-03-26"].contains(&protocol) {
            return fail("unsupported HTTP MCP protocol");
        }
        *self.protocol.borrow_mut() = Some(protocol.to_owned());
        self.exchange(
            "POST",
            Some(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
            None,
        )
        .await
        .map(|_| ())
    }

    async fn connect(&self) -> Result<Box<dyn Stream>> {
        let tcp = tokio::net::TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|_| ToolError("unavailable".into()))?;
        if !self.tls {
            return Ok(Box::new(tcp));
        }
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(self.host.clone())
            .map_err(|_| ToolError("unavailable".into()))?;
        Ok(Box::new(
            crate::tls_connector()?
                .connect(name, tcp)
                .await
                .map_err(|_| ToolError("unavailable".into()))?,
        ))
    }

    /// One HTTP exchange; a lost action response is never retried.
    pub fn exchange<'a>(
        &'a self,
        method: &'a str,
        body: Option<&'a Value>,
        request_id: Option<u64>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<Value>>> + 'a>> {
        // Boxed: the TLS and framing state machine is large in debug builds.
        Box::pin(self.exchange_inner(method, body, request_id))
    }

    async fn exchange_inner(
        &self,
        method: &str,
        body: Option<&Value>,
        request_id: Option<u64>,
    ) -> Result<Option<Value>> {
        if self.closed.get() {
            return fail("HTTP MCP session closed");
        }
        let raw = body
            .map(std::string::ToString::to_string)
            .unwrap_or_default();
        if raw.len() > LIMIT {
            return fail("HTTP MCP request exceeds limit");
        }
        let run = async {
            let mut stream = self.connect().await?;
            let host = if self.host.contains(':') {
                format!("[{}]", self.host)
            } else {
                self.host.clone()
            };
            let mut head = format!(
                "{method} {} HTTP/1.1\r\nHost: {host}:{}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n",
                self.target,
                self.port,
                raw.len()
            );
            if let Some(p) = self.protocol.borrow().as_ref() {
                head.push_str(&format!("MCP-Protocol-Version: {p}\r\n"));
            }
            if let Some(s) = self.session.borrow().as_ref() {
                head.push_str(&format!("MCP-Session-Id: {s}\r\n"));
            }
            if let Some(a) = &self.authorization {
                head.push_str(&format!("Authorization: {a}\r\n"));
            }
            head.push_str("\r\n");
            stream
                .write_all(head.as_bytes())
                .await
                .map_err(|_| ToolError("unavailable".into()))?;
            stream
                .write_all(raw.as_bytes())
                .await
                .map_err(|_| ToolError("unavailable".into()))?;
            let mut reader = BufReader::new(stream);
            let (status, headers) = provider::read_head(&mut reader).await?;
            if status == 404 && self.session.borrow().is_some() {
                self.closed.set(true);
                return fail("MCP session expired; reconnect before retrying explicitly");
            }
            if !(200..300).contains(&status) {
                return fail("HTTP MCP request refused");
            }
            if let Some(session) = headers.get("mcp-session-id") {
                if session.is_empty() || session.len() > 4096 || !printable(session, 33) {
                    return fail("invalid MCP session identity");
                }
                if self.session.borrow().as_ref().is_some_and(|s| s != session) {
                    return fail("MCP changed session identity");
                }
                *self.session.borrow_mut() = Some(session.clone());
            }
            let content = headers
                .get("content-type")
                .map(|c| {
                    c.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                })
                .unwrap_or_default();
            if content == "text/event-stream" {
                return self.events(&mut reader, &headers, request_id).await;
            }
            let data = provider::read_body(&mut reader, &headers, 2 * LIMIT).await?;
            if request_id.is_none() {
                if (method == "POST" && status != 202) || !data.is_empty() {
                    return fail("HTTP MCP notification not accepted");
                }
                return Ok(None);
            }
            if content == "application/json" {
                let frame: Value = serde_json::from_slice(&data)
                    .map_err(|_| ToolError("invalid HTTP MCP frame".into()))?;
                if let Some(result) = self.frame(&frame, request_id).await? {
                    return Ok(Some(result));
                }
            }
            // Never replay a tools/call POST after transport uncertainty.
            fail("MCP outcome unavailable; do not automatically replay the action")
        };
        tokio::time::timeout(Duration::from_secs(60), run)
            .await
            .map_err(|_| {
                ToolError("HTTP MCP transport unavailable; action outcome may be unknown".into())
            })?
    }

    /// Server-sent events: each `data:` event is one JSON-RPC frame.
    async fn events<R: tokio::io::AsyncBufRead + Unpin>(
        &self,
        reader: &mut R,
        headers: &std::collections::HashMap<String, String>,
        request_id: Option<u64>,
    ) -> Result<Option<Value>> {
        // Chunked SSE is de-chunked first; other framings stream directly.
        let body = if headers.contains_key("transfer-encoding")
            || headers.contains_key("content-length")
        {
            provider::read_body(reader, headers, 2 * LIMIT).await?
        } else {
            let mut data = Vec::new();
            let mut events = 0;
            loop {
                let mut line = Vec::new();
                let n = provider::read_line(reader, &mut line, 2 * LIMIT - data.len())
                    .await
                    .map_err(|_| ToolError("unavailable".into()))?;
                data.extend_from_slice(&line);
                if data.len() > 2 * LIMIT {
                    return fail("HTTP MCP response exceeds limit");
                }
                if n == 0 || line == b"\n" || line == b"\r\n" {
                    let event = std::mem::take(&mut data);
                    if let Some(result) = self.event(&event, request_id, &mut events).await? {
                        return Ok(Some(result));
                    }
                    if n == 0 {
                        break;
                    }
                }
            }
            return if request_id.is_none() {
                Ok(None)
            } else {
                fail("MCP outcome unavailable; do not automatically replay the action")
            };
        };
        let mut events = 0;
        let text = body.clone();
        let normalized = String::from_utf8_lossy(&text).replace("\r\n", "\n");
        for event in normalized.split("\n\n") {
            if let Some(result) = self
                .event(event.as_bytes(), request_id, &mut events)
                .await?
            {
                return Ok(Some(result));
            }
        }
        if request_id.is_none() {
            Ok(None)
        } else {
            fail("MCP outcome unavailable; do not automatically replay the action")
        }
    }

    async fn event(
        &self,
        raw: &[u8],
        request_id: Option<u64>,
        events: &mut usize,
    ) -> Result<Option<Value>> {
        let text = String::from_utf8_lossy(raw).replace("\r\n", "\n");
        let data = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if text.trim().is_empty() {
            return Ok(None);
        }
        *events += 1;
        if *events > 128 {
            return fail("HTTP MCP event budget exceeded");
        }
        if data.is_empty() {
            return Ok(None);
        }
        let frame: Value =
            serde_json::from_str(&data).map_err(|_| ToolError("invalid HTTP MCP frame".into()))?;
        self.frame(&frame, request_id).await
    }

    async fn frame(&self, body: &Value, request_id: Option<u64>) -> Result<Option<Value>> {
        if body["jsonrpc"] != "2.0" {
            return fail("invalid HTTP MCP frame");
        }
        if body.get("method").is_some() && body.get("id").is_some() {
            // Sampling, roots and elicitation are never silently granted.
            let refusal = json!({"jsonrpc": "2.0", "id": body["id"], "error": {"code": -32601, "message": "Client capability not enabled"}});
            self.exchange("POST", Some(&refusal), None).await?;
        } else if request_id.is_some() && body.get("id") == Some(&json!(request_id)) {
            if body.get("error").is_some() || !body["result"].is_object() {
                return fail("MCP call refused");
            }
            return Ok(Some(body["result"].clone()));
        }
        Ok(None)
    }

    pub async fn request(self: &Rc<Self>, method: &str, params: Value) -> Result<Value> {
        let _guard = self.lock.lock().await;
        let id = self.request_id.get() + 1;
        self.request_id.set(id);
        // If this request is abandoned, tell the server it was cancelled.
        let notify = Cancel {
            client: self.clone(),
            id,
            armed: true,
        };
        let mut notify = notify;
        let result = self
            .exchange(
                "POST",
                Some(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})),
                Some(id),
            )
            .await;
        notify.armed = false;
        result?.ok_or_else(|| {
            ToolError("MCP outcome unavailable; do not automatically replay the action".into())
        })
    }

    pub async fn close(&self) {
        if self.session.borrow().is_some() && !self.closed.get() {
            let _ =
                tokio::time::timeout(Duration::from_secs(2), self.exchange("DELETE", None, None))
                    .await;
        }
        self.closed.set(true);
        *self.session.borrow_mut() = None;
    }
}

struct Cancel {
    client: Rc<HttpClient>,
    id: u64,
    armed: bool,
}
impl Drop for Cancel {
    fn drop(&mut self) {
        if self.armed {
            let (client, id) = (self.client.clone(), self.id);
            tokio::task::spawn_local(async move {
                let note = json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id}});
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    client.exchange("POST", Some(&note), None),
                )
                .await;
            });
        }
    }
}
