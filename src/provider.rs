// SPDX-License-Identifier: MIT
//! Explicit OpenAI-compatible chat tool calls through the local AI gateway.
use crate::{Result, ToolError, fail, uid};
use serde_json::{Value, json};
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::PathBuf,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

const LIMIT: usize = 2 * 1024 * 1024;

pub struct Gateway {
    pub path: PathBuf,
}

/// Read one line of at most `limit` bytes; a longer line is cut at `limit + 1`
/// so the caller's bound check fails without buffering an unbounded line.
pub async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<usize> {
    (&mut *reader)
        .take(limit as u64 + 1)
        .read_until(b'\n', line)
        .await
}

/// Read one HTTP response body: chunked, length-delimited or until EOF.
pub async fn read_body<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    headers: &std::collections::HashMap<String, String>,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    read_body_with(reader, headers, limit, &mut |bytes| {
        data.extend_from_slice(bytes);
        Ok(())
    })
    .await?;
    Ok(data)
}

/// Read one HTTP response body as it arrives, handing each piece to `sink`;
/// at most `limit` bytes are read in total.
pub async fn read_body_with<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    headers: &std::collections::HashMap<String, String>,
    limit: usize,
    sink: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let invalid = || ToolError("invalid response framing".into());
    let mut total = 0usize;
    let mut piece = Vec::new();
    if let Some(transfer) = headers.get("transfer-encoding") {
        if !transfer.eq_ignore_ascii_case("chunked") || headers.contains_key("content-length") {
            return fail("unsupported transfer encoding");
        }
        for _ in 0..4096 {
            let mut line = Vec::new();
            read_line(reader, &mut line, 128)
                .await
                .map_err(|_| invalid())?;
            if line.len() > 128 {
                return fail("chunk header exceeds limit");
            }
            let text = String::from_utf8_lossy(&line);
            let size = usize::from_str_radix(text.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| invalid())?;
            if size == 0 {
                let mut trailers = 0;
                loop {
                    let mut trailer = Vec::new();
                    let n = read_line(reader, &mut trailer, 8192 - trailers)
                        .await
                        .map_err(|_| invalid())?;
                    trailers += n;
                    if n == 0 || trailers > 8192 {
                        return Err(invalid());
                    }
                    if trailer == b"\r\n" {
                        return Ok(());
                    }
                }
            }
            if size > limit - total {
                return fail("response exceeds limit");
            }
            total += size;
            piece.resize(size, 0);
            reader.read_exact(&mut piece).await.map_err(|_| invalid())?;
            let mut end = [0u8; 2];
            reader.read_exact(&mut end).await.map_err(|_| invalid())?;
            if &end != b"\r\n" {
                return Err(invalid());
            }
            sink(&piece)?;
        }
        return fail("chunk count exceeds limit");
    }
    if let Some(length) = headers.get("content-length") {
        let length = length.parse::<usize>().map_err(|_| invalid())?;
        if length > limit {
            return fail("response exceeds limit");
        }
        piece.resize(length, 0);
        reader.read_exact(&mut piece).await.map_err(|_| invalid())?;
        return sink(&piece);
    }
    let mut chunk = [0u8; 65536];
    loop {
        let n = reader.read(&mut chunk).await.map_err(|_| invalid())?;
        if n == 0 {
            return Ok(());
        }
        total += n;
        if total > limit {
            return fail("response exceeds limit");
        }
        sink(&chunk[..n])?;
    }
}

/// Text a streamed reply adds, by kind: `"reasoning"` or `"content"`.
pub type Delta = std::rc::Rc<dyn Fn(&str, &str)>;

/// Assembles an OpenAI-format chat completion stream (`data:` lines of
/// `chat.completion.chunk`) into the one assistant message it describes.
#[derive(Default)]
pub struct Assembler {
    line: Vec<u8>,
    content: String,
    reasoning: String,
    calls: Vec<Value>,
    bytes: usize,
    /// The stream's end marker arrived; a stream cut short has none.
    pub done: bool,
}

impl Assembler {
    /// Feed received bytes; returns the reasoning and content text they added.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(String, String)> {
        let (mut reasoning, mut content) = (String::new(), String::new());
        for &byte in bytes {
            if byte != b'\n' {
                if self.line.len() >= LIMIT {
                    return fail("invalid provider response");
                }
                self.line.push(byte);
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line = std::str::from_utf8(&line)
                .map_err(|_| ToolError("invalid provider response".into()))?
                .trim_end_matches('\r');
            let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                continue;
            };
            if data == "[DONE]" {
                self.done = true;
                continue;
            }
            let chunk: Value = serde_json::from_str(data)
                .map_err(|_| ToolError("invalid provider response".into()))?;
            let delta = &chunk["choices"][0]["delta"];
            for key in ["reasoning_content", "reasoning"] {
                if let Some(text) = delta[key].as_str() {
                    self.reasoning.push_str(text);
                    reasoning.push_str(text);
                    self.bytes += text.len();
                }
            }
            if let Some(text) = delta["content"].as_str() {
                self.content.push_str(text);
                content.push_str(text);
                self.bytes += text.len();
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                let index = call["index"]
                    .as_u64()
                    .map_or(self.calls.len(), |i| i as usize);
                if index > self.calls.len() || index >= 8 {
                    return fail("provider tool batch exceeds limit");
                }
                if index == self.calls.len() {
                    self.calls.push(json!({"id": "", "type": "function", "function": {"name": "", "arguments": ""}}));
                }
                let slot = &mut self.calls[index];
                if let Some(id) = call["id"].as_str() {
                    slot["id"] = json!(id);
                }
                for key in ["name", "arguments"] {
                    if let Some(text) = call["function"][key].as_str() {
                        let joined =
                            format!("{}{text}", slot["function"][key].as_str().unwrap_or(""));
                        slot["function"][key] = json!(joined);
                        self.bytes += text.len();
                    }
                }
            }
            if self.bytes > 256 * 1024 {
                return fail("invalid provider response");
            }
        }
        Ok((reasoning, content))
    }

    /// The assistant message the stream described; reasoning stays out of it.
    pub fn finish(self) -> Value {
        let mut message = json!({"role": "assistant", "content": if self.content.is_empty() && !self.calls.is_empty() { Value::Null } else { json!(self.content) }});
        if !self.calls.is_empty() {
            message["tool_calls"] = Value::Array(self.calls);
        }
        message
    }
}

/// Status code and lowercase headers (duplicates refused) up to the blank line.
pub async fn read_head<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<(u16, std::collections::HashMap<String, String>)> {
    let invalid = || ToolError("invalid response head".into());
    let mut head = Vec::new();
    loop {
        let room = 16384 - head.len();
        let n = read_line(reader, &mut head, room)
            .await
            .map_err(|_| invalid())?;
        if n == 0 || head.len() > 16384 {
            return Err(invalid());
        }
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&head).map_err(|_| invalid())?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(invalid)?;
    let mut headers = std::collections::HashMap::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (key, value) = line.split_once(':').ok_or_else(invalid)?;
        if headers
            .insert(key.to_ascii_lowercase(), value.trim().to_owned())
            .is_some()
        {
            return fail("duplicate response header");
        }
    }
    Ok((status, headers))
}

impl Gateway {
    /// One completion, streamed when `body` asks for it; streamed text reaches
    /// `delta` as it arrives. Returns the assistant message.
    pub async fn complete(&self, body: Value, delta: Delta) -> Result<Value> {
        let info = std::fs::symlink_metadata(&self.path)
            .map_err(|_| ToolError("private owned gateway socket required".into()))?;
        let parent = self
            .path
            .parent()
            .and_then(|p| std::fs::metadata(p).ok())
            .ok_or_else(|| ToolError("private owned gateway socket required".into()))?;
        if !info.file_type().is_socket()
            || info.uid() != uid()
            || info.mode() & 0o077 != 0
            || parent.uid() != uid()
            || parent.mode() & 0o077 != 0
        {
            return fail("private owned gateway socket required");
        }
        let raw = body.to_string();
        if raw.len() > LIMIT {
            return fail("provider request exceeds limit");
        }
        let exchange = async {
            let stream = UnixStream::connect(&self.path)
                .await
                .map_err(|_| ToolError("gateway unavailable".into()))?;
            if stream.peer_cred().map(|c| c.uid()).ok() != Some(uid()) {
                return fail("gateway peer refused");
            }
            let (reader, mut writer) = stream.into_split();
            let request = format!(
                "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{raw}",
                raw.len()
            );
            writer
                .write_all(request.as_bytes())
                .await
                .map_err(|_| ToolError("gateway unavailable".into()))?;
            let mut reader = BufReader::new(reader);
            let (status, headers) = read_head(&mut reader).await?;
            if status != 200 {
                return fail("selected provider unavailable");
            }
            let streamed = headers
                .get("content-type")
                .is_some_and(|t| t.starts_with("text/event-stream"));
            if streamed {
                let mut assembler = Assembler::default();
                read_body_with(&mut reader, &headers, LIMIT, &mut |bytes| {
                    let (reasoning, content) = assembler.feed(bytes)?;
                    if !reasoning.is_empty() {
                        delta("reasoning", &reasoning);
                    }
                    if !content.is_empty() {
                        delta("content", &content);
                    }
                    Ok(())
                })
                .await?;
                if !assembler.done {
                    return fail("selected provider response unavailable");
                }
                return Ok(assembler.finish());
            }
            let data = read_body(&mut reader, &headers, LIMIT).await?;
            let response: Value = serde_json::from_slice(&data)
                .map_err(|_| ToolError("invalid provider response".into()))?;
            response["choices"][0]
                .get("message")
                .cloned()
                .ok_or_else(|| ToolError("invalid provider response".into()))
        };
        tokio::time::timeout(Duration::from_secs(180), exchange)
            .await
            .map_err(|_| ToolError("selected provider response unavailable".into()))?
            .map_err(|_| ToolError("selected provider response unavailable".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunked() -> std::collections::HashMap<String, String> {
        [("transfer-encoding".to_owned(), "chunked".to_owned())].into()
    }

    #[test]
    fn a_stream_assembles_text_and_tool_call_fragments() {
        let mut stream = Assembler::default();
        let chunk = |delta: Value| format!("data: {}\n\n", json!({"choices": [{"delta": delta}]}));
        let first = chunk(json!({"role": "assistant", "reasoning_content": "think"}))
            + &chunk(json!({"content": "Hi"}));
        // A line may be split across reads.
        let (split_at, rest) = first.split_at(first.len() - 5);
        assert_eq!(
            stream.feed(split_at.as_bytes()).unwrap(),
            ("think".to_owned(), String::new())
        );
        assert_eq!(
            stream.feed(rest.as_bytes()).unwrap(),
            (String::new(), "Hi".to_owned())
        );
        let calls = chunk(
            json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "file_read", "arguments": "{\"pa"}}]}),
        ) + &chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "th\":\"a\"}"}}]}),
        ) + "data: [DONE]\n\n";
        assert!(!stream.done);
        stream.feed(calls.as_bytes()).unwrap();
        assert!(stream.done);
        assert_eq!(
            stream.finish(),
            json!({"role": "assistant", "content": "Hi", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "file_read", "arguments": "{\"path\":\"a\"}"}}]})
        );
        let mut skipping = Assembler::default();
        let gap = chunk(json!({"tool_calls": [{"index": 1, "id": "c2"}]}));
        assert!(skipping.feed(gap.as_bytes()).is_err());
    }

    #[tokio::test]
    async fn hostile_framing_is_refused_without_unbounded_buffering() {
        // A head line that never ends stops at the limit instead of growing.
        let endless = vec![b'a'; 64 * 1024];
        assert!(read_head(&mut BufReader::new(&endless[..])).await.is_err());
        // A chunk size near usize::MAX after data neither wraps nor panics.
        let overflow = b"1\r\na\r\nffffffffffffffff\r\n".to_vec();
        assert!(
            read_body(&mut BufReader::new(&overflow[..]), &chunked(), 1024)
                .await
                .is_err()
        );
        let ok = b"3\r\nabc\r\n0\r\n\r\n".to_vec();
        assert_eq!(
            read_body(&mut BufReader::new(&ok[..]), &chunked(), 1024)
                .await
                .unwrap(),
            b"abc"
        );
    }
}
