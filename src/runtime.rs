// SPDX-License-Identifier: MIT
//! Sessions, exact expiring tool approvals and a provider-independent bounded tool loop.
use crate::{
    Result, ToolError, fail,
    mcp::Mcp,
    tools::{self, Workspace},
};
use serde_json::{Map, Value, json};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

pub use crate::provider::Delta;

/// One provider completion: the request body and where streamed text goes.
pub type Complete = Rc<dyn Fn(Value, Delta) -> Pin<Box<dyn Future<Output = Result<Value>>>>>;

pub fn tools_schema() -> Vec<Value> {
    let tool = |name: &str, description: &str, properties: Value, required: Value| {
        json!({"type": "function", "function": {"name": name, "description": description,
            "parameters": {"type": "object", "properties": properties, "required": required, "additionalProperties": false}}})
    };
    vec![
        tool(
            "files_list",
            "List one folder of the workspace without following symlinks. `path` is a folder inside the workspace; null lists the workspace itself.",
            json!({"path": {"type": ["string", "null"]}}),
            json!([]),
        ),
        tool(
            "file_read",
            "Read one bounded UTF-8 text file of the workspace. `path` is relative to the workspace: a file name such as \"todo.txt\", or folder/name for a file in a folder.",
            json!({"path": {"type": "string"}}),
            json!(["path"]),
        ),
        tool(
            "file_write",
            "Replace reviewed text and retain the displaced file as a backup; sha256 null creates without overwrite.",
            json!({"path": {"type": "string"}, "text": {"type": "string"}, "expected_sha256": {"type": ["string", "null"]}}),
            json!(["path", "text", "expected_sha256"]),
        ),
        tool(
            "command",
            "Run argv in a bounded sandbox; no credentials, GUI bus or host home is mounted.",
            json!({"argv": {"type": "array", "items": {"type": "string"}}, "writable": {"type": "boolean"}, "network": {"type": "boolean"}}),
            json!(["argv"]),
        ),
    ]
}

/// Tool failures a model can act on are told to it as they are; any other
/// failure stays opaque, so nothing about the system leaks into a transcript.
const CORRECTABLE: &[(&str, &str)] = &[
    (
        "workspace-relative path required",
        "workspace-relative path required: name it from the workspace, such as \"todo.txt\"",
    ),
    ("invalid workspace path", "invalid workspace path"),
    (
        "workspace path unavailable",
        "no such file or folder in the workspace; call files_list with a null path to see what is there",
    ),
    (
        "workspace text file unavailable",
        "not a readable text file in the workspace; call files_list with a null path to see what is there",
    ),
    (
        "workspace folder unavailable",
        "no such folder in the workspace; call files_list with a null path to see what is there",
    ),
    ("file exceeds limit", "file exceeds limit"),
    ("folder exceeds entry limit", "folder exceeds entry limit"),
    ("file changed; review again", "file changed; review again"),
    ("invalid command arguments", "invalid command arguments"),
    ("command deadline exceeded", "command deadline exceeded"),
];

/// A tool result as a client shows it: at most 4 KiB, cut at a character.
pub fn shown(text: &str) -> String {
    let mut end = text.len().min(4096);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn model_error(error: &ToolError) -> &'static str {
    CORRECTABLE
        .iter()
        .find(|(known, _)| error.0 == *known)
        .map_or("tool-operation-unavailable", |(_, told)| told)
}

/// Transcript budget sent to the provider. It also keeps the saved transcript,
/// plus one final assistant message, inside one service frame for export.
pub const CONTEXT_LIMIT: usize = 640 * 1024;
/// One tool result in the transcript; larger results are cut with a marker.
pub const RESULT_LIMIT: usize = 96 * 1024;

fn size(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|m| m.to_string().len() + 1)
        .sum::<usize>()
        + 2
}

/// Answer every call of an unfinished tool batch so the transcript stays a
/// valid provider conversation after a cancel, disconnect or failure.
pub fn close_batch(messages: &mut Vec<Value>) {
    let Some(start) = messages.iter().rposition(|m| m["role"] == "assistant") else {
        return;
    };
    let answered = messages[start + 1..]
        .iter()
        .filter_map(|m| m["tool_call_id"].as_str().map(str::to_owned))
        .collect::<std::collections::HashSet<_>>();
    let missing = messages[start]["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_str())
        .filter(|id| !answered.contains(*id))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for id in missing {
        messages.push(json!({"role": "tool", "tool_call_id": id, "content": json!({"ok": false, "error": "tool-call-cancelled"}).to_string()}));
    }
}

/// Drop the oldest whole turns (a user message up to the next one) until the
/// transcript fits; the latest user turn is never dropped.
pub fn trim(messages: &mut Vec<Value>, limit: usize) {
    while size(messages) > limit {
        let Some(next) = messages
            .iter()
            .skip(1)
            .position(|m| m["role"] == "user")
            .map(|i| i + 1)
        else {
            return;
        };
        messages.drain(..next);
    }
}

/// Binding of an approval to its exact session, turn, call, tool and arguments.
pub fn fingerprint(value: &Value) -> String {
    enzimos_platform::json::fingerprint(value).unwrap_or_default()
}

pub fn token() -> String {
    enzimos_platform::files::random_hex(16).unwrap_or_default()
}

struct Pending {
    sender: Option<oneshot::Sender<bool>>,
    binding: String,
    turn_id: Option<String>,
    expires: Instant,
}

pub struct Session {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub policy: Value,
    pub profile: String,
    pub workspace: Workspace,
    pub messages: Vec<Value>,
    pub events: VecDeque<Value>,
    event_sizes: VecDeque<usize>,
    event_bytes: usize,
    pub sequence: u64,
    pub turn_id: Option<String>,
    pending: HashMap<String, Pending>,
    pub task: Option<tokio::task::AbortHandle>,
    pub last_active: Instant,
    pub observers: Vec<mpsc::Sender<Value>>,
    /// Messages the user sent into the running turn, taken before its next step.
    steers: Vec<String>,
}

pub type Shared = Rc<RefCell<Session>>;

impl Session {
    pub fn new(
        workspace: Workspace,
        provider: &str,
        model: &str,
        policy: Value,
        profile: &str,
    ) -> Result<Self> {
        if !["writes", "commands", "network"]
            .iter()
            .all(|k| policy[*k].is_boolean())
        {
            return fail("invalid session policy");
        }
        Ok(Self {
            id: token(),
            provider: provider.into(),
            model: model.into(),
            policy,
            profile: profile.into(),
            workspace,
            messages: vec![],
            events: VecDeque::new(),
            event_sizes: VecDeque::new(),
            event_bytes: 0,
            sequence: 0,
            turn_id: None,
            pending: HashMap::new(),
            task: None,
            last_active: Instant::now(),
            observers: vec![],
            steers: vec![],
        })
    }

    pub fn emit(&mut self, kind: &str, values: Value) -> Value {
        self.sequence += 1;
        let mut event = json!({"schema_version": 1, "sequence": self.sequence, "session_id": self.id, "turn_id": self.turn_id, "type": kind});
        if let Value::Object(extra) = values {
            event.as_object_mut().unwrap().extend(extra);
        }
        let size = event.to_string().len();
        self.events.push_back(event.clone());
        self.event_sizes.push_back(size);
        self.event_bytes += size;
        while self.events.len() > 512 || self.event_bytes > 2 * 1024 * 1024 {
            self.events.pop_front();
            self.event_bytes -= self.event_sizes.pop_front().unwrap_or(0);
        }
        let before = self.observers.len();
        // A stalled observer never grows an unbounded buffer; it ends the turn.
        self.observers
            .retain(|queue| queue.try_send(event.clone()).is_ok() || queue.is_closed());
        if self.observers.len() != before
            && let Some(task) = &self.task
        {
            task.abort();
        }
        event
    }

    /// Queue a user message into the running turn; the model reads it before
    /// its next step, and the turn takes another step for it if it was ending.
    pub fn steer(&mut self, text: &str) -> Result<()> {
        let text = text.trim();
        if self.turn_id.is_none() {
            return fail("no running turn");
        }
        if !(1..=65536).contains(&text.len()) || self.steers.len() >= 8 {
            return fail("invalid steering message");
        }
        self.steers.push(text.to_owned());
        Ok(())
    }

    /// Move queued steering messages into the transcript; true when there were any.
    fn take_steers(&mut self) -> bool {
        let steers = std::mem::take(&mut self.steers);
        for text in &steers {
            self.messages.push(json!({"role": "user", "content": text}));
            self.emit("steer.accepted", json!({"text": text}));
        }
        !steers.is_empty()
    }

    pub fn resolve(&mut self, request_id: &str, binding: &str, allow: &Value) -> Result<()> {
        let allow = allow
            .as_bool()
            .ok_or_else(|| ToolError("stale or mismatched approval".into()))?;
        let turn = self.turn_id.clone();
        let pending = self
            .pending
            .get_mut(request_id)
            .filter(|p| p.binding == binding && p.turn_id == turn && p.expires > Instant::now())
            .ok_or_else(|| ToolError("stale or mismatched approval".into()))?;
        let sender = pending
            .sender
            .take()
            .ok_or_else(|| ToolError("approval already resolved".into()))?;
        let _ = sender.send(allow);
        Ok(())
    }

    pub fn cancel(&self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Removes a pending approval and reports it resolved, however the wait ends.
struct ApprovalGuard {
    session: Shared,
    request_id: String,
}
impl Drop for ApprovalGuard {
    fn drop(&mut self) {
        if let Ok(mut session) = self.session.try_borrow_mut() {
            session.pending.remove(&self.request_id);
            session.emit("approval.resolved", json!({"request_id": self.request_id}));
        }
    }
}

pub async fn approval(session: &Shared, call_id: &str, name: &str, arguments: &Value) -> bool {
    let (sender, receiver) = oneshot::channel();
    let request_id = token();
    {
        let mut s = session.borrow_mut();
        let identity = json!({"session_id": s.id, "turn_id": s.turn_id, "call_id": call_id, "tool": name, "arguments": arguments});
        let binding = fingerprint(&identity);
        let turn_id = s.turn_id.clone();
        s.pending.insert(
            request_id.clone(),
            Pending {
                sender: Some(sender),
                binding: binding.clone(),
                turn_id,
                expires: Instant::now() + Duration::from_secs(90),
            },
        );
        s.emit("approval.requested", json!({"request_id": request_id, "binding": binding, "call_id": call_id, "tool": name, "arguments": arguments}));
    }
    let _guard = ApprovalGuard {
        session: session.clone(),
        request_id,
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(90), receiver).await,
        Ok(Ok(true))
    )
}

pub async fn invoke(
    session: &Shared,
    call_id: &str,
    name: &str,
    arguments: Value,
    mcp: Option<&Mcp>,
) -> Result<Value> {
    let arguments = match arguments {
        Value::Object(map) if Value::Object(map.clone()).to_string().len() <= 128 * 1024 => map,
        _ => return fail("tool arguments exceed limit"),
    };
    let schemas = tools_schema();
    let builtin = schemas
        .iter()
        .find(|t| t["function"]["name"] == name)
        .map(|t| t["function"]["parameters"].clone());
    match &builtin {
        Some(schema) => {
            let properties = schema["properties"].as_object().unwrap();
            let required = schema["required"].as_array().unwrap();
            if arguments.keys().any(|k| !properties.contains_key(k))
                || required
                    .iter()
                    .any(|k| !arguments.contains_key(k.as_str().unwrap()))
            {
                return fail("invalid tool arguments");
            }
        }
        None if mcp.is_some_and(|m| m.has(name)) => {}
        None => return fail("unknown tool"),
    }
    let policy = session.borrow().policy.clone();
    let flag = |k: &str| arguments.get(k).and_then(Value::as_bool).unwrap_or(false);
    // External MCP calls always need harness consent; Center may require more.
    let needs_approval = builtin.is_none()
        || (name == "file_write" && policy["writes"] != true)
        || (name == "command"
            && (policy["commands"] != true
                || (flag("writable") && policy["writes"] != true)
                || (flag("network") && policy["network"] != true)));
    session.borrow_mut().emit(
        "tool.started",
        json!({"call_id": call_id, "tool": name, "arguments": Value::Object(arguments.clone())}),
    );
    let started = Instant::now();
    let outcome = async {
        if needs_approval
            && !approval(session, call_id, name, &Value::Object(arguments.clone())).await
        {
            return fail("tool action declined or approval expired");
        }
        let text = |k: &str| {
            arguments
                .get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| ToolError("invalid tool request".into()))
        };
        match name {
            "files_list" => {
                let path = match arguments.get("path") {
                    None | Some(Value::Null) => None,
                    // The workspace itself, however a model names it.
                    Some(Value::String(p)) if ["", ".", "./", "/"].contains(&p.as_str()) => None,
                    Some(Value::String(p)) => Some(p.clone()),
                    _ => return fail("invalid tool request"),
                };
                let workspace = &session.borrow().workspace;
                // A file named where a folder was expected is read instead:
                // the same authority, and what the model was after.
                workspace
                    .list(path.as_deref())
                    .or_else(|error| match path.as_deref() {
                        Some(file) => workspace
                            .read(file)
                            .map(|read| json!({"file": file, "read": read}))
                            .map_err(|_| error),
                        None => Err(error),
                    })
            }
            "file_read" => session.borrow().workspace.read(&text("path")?),
            "file_write" => {
                let expected = match &arguments["expected_sha256"] {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    _ => return fail("invalid tool request"),
                };
                session.borrow().workspace.write(
                    &text("path")?,
                    &text("text")?,
                    expected.as_deref(),
                )
            }
            "command" => {
                let argv = arguments["argv"]
                    .as_array()
                    .and_then(|a| {
                        a.iter()
                            .map(|v| v.as_str().map(str::to_owned))
                            .collect::<Option<Vec<_>>>()
                    })
                    .ok_or_else(|| ToolError("invalid command arguments".into()))?;
                for key in ["writable", "network"] {
                    if arguments.get(key).is_some_and(|v| !v.is_boolean()) {
                        return fail("invalid command policy");
                    }
                }
                // A duplicate of the session's descriptor; no borrow is held across the run.
                let ws = session.borrow().workspace.share()?;
                tools::command(&ws, &argv, flag("writable"), flag("network"), false).await
            }
            _ => {
                mcp.unwrap()
                    .call(name, Value::Object(arguments.clone()))
                    .await
            }
        }
    }
    .await;
    let (status, output) = match &outcome {
        Ok(result) => (
            "completed",
            shown(&match result {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            }),
        ),
        Err(error) => ("failed", model_error(error).to_owned()),
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    session.borrow_mut().emit("tool.completed", json!({"call_id": call_id, "tool": name, "status": status, "duration_ms": duration_ms, "output": output}));
    outcome
}

/// Clears turn state however the turn ends; an abandoned turn reports cancelled.
struct TurnGuard {
    session: Shared,
    finished: bool,
    started: Instant,
}
impl Drop for TurnGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = self.session.try_borrow_mut() {
            if !self.finished {
                close_batch(&mut s.messages);
                s.emit("turn.completed", json!({"status": "cancelled", "duration_ms": self.started.elapsed().as_millis() as u64}));
            }
            s.pending.clear();
            s.steers.clear();
            s.turn_id = None;
            s.task = None;
        }
    }
}

pub async fn run(
    session: &Shared,
    prompt: &str,
    complete: &Complete,
    mcp: Option<&Mcp>,
    max_steps: usize,
    instructions: Option<&str>,
) -> Result<()> {
    if session.borrow().turn_id.is_some() {
        return fail("session already has an active turn");
    }
    if !(1..=65536).contains(&prompt.len()) || !(1..=16).contains(&max_steps) {
        return fail("invalid turn input");
    }
    if instructions.is_some_and(|i| i.len() > 131072) {
        return fail("skill instruction context exceeds limit");
    }
    {
        let mut s = session.borrow_mut();
        let mut messages = s.messages.clone();
        messages.push(json!({"role": "user", "content": prompt}));
        trim(&mut messages, CONTEXT_LIMIT);
        if size(&messages) > CONTEXT_LIMIT {
            return fail("session context exceeds limit");
        }
        s.messages = messages;
        s.last_active = Instant::now();
        s.turn_id = Some(token());
        s.emit("turn.started", json!({}));
    }
    let mut guard = TurnGuard {
        session: session.clone(),
        finished: false,
        started: Instant::now(),
    };
    let steps = async {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..max_steps {
            let (provider, model, mut messages) = {
                let mut s = session.borrow_mut();
                s.take_steers();
                trim(&mut s.messages, CONTEXT_LIMIT);
                if size(&s.messages) > CONTEXT_LIMIT {
                    return fail("session context exceeds limit");
                }
                (s.provider.clone(), s.model.clone(), s.messages.clone())
            };
            if let Some(instructions) = instructions.filter(|i| !i.is_empty()) {
                messages.insert(0, json!({"role": "system", "content": instructions}));
            }
            let mut tools = tools_schema();
            if let Some(mcp) = mcp {
                tools.extend(mcp.schemas.borrow().iter().cloned());
            }
            // Streamed text reaches the client as it arrives; reasoning is
            // closed by the first answer text or by the end of the reply.
            let thinking = Rc::new(std::cell::Cell::new(None::<Instant>));
            let close_thinking = |s: &mut Session, thinking: &std::cell::Cell<Option<Instant>>| {
                if let Some(started) = thinking.take() {
                    s.emit(
                        "reasoning.completed",
                        json!({"duration_ms": started.elapsed().as_millis() as u64}),
                    );
                }
            };
            let delta: Delta = {
                let (session, thinking) = (session.clone(), thinking.clone());
                Rc::new(move |kind: &str, text: &str| {
                    let mut s = session.borrow_mut();
                    if kind == "reasoning" {
                        if thinking.get().is_none() {
                            thinking.set(Some(Instant::now()));
                        }
                        s.emit("reasoning.delta", json!({"text": text}));
                    } else {
                        close_thinking(&mut s, &thinking);
                        s.emit("message.delta", json!({"text": text}));
                    }
                })
            };
            let message = complete(json!({"provider": provider, "model": model, "messages": messages, "tools": tools, "stream": true}), delta).await;
            close_thinking(&mut session.borrow_mut(), &thinking);
            let message = message?;
            if message["role"] != "assistant" || message.to_string().len() > 256 * 1024 {
                return fail("invalid provider response");
            }
            let calls = match message.get("tool_calls") {
                None => vec![],
                Some(Value::Array(c)) if c.len() <= 8 => c.clone(),
                _ => return fail("provider tool batch exceeds limit"),
            };
            // Model output is untrusted: check every call identity before the
            // batch enters the transcript, so a bad one never leaves it unanswered.
            let mut ids = Vec::new();
            for call in &calls {
                let call_id = call["id"]
                    .as_str()
                    .filter(|c| (1..=128).contains(&c.len()) && !seen.contains(*c))
                    .ok_or_else(|| ToolError("invalid tool call identity".into()))?
                    .to_owned();
                seen.insert(call_id.clone());
                ids.push(call_id);
            }
            {
                let mut s = session.borrow_mut();
                s.messages.push(message.clone());
                s.emit(
                    "message.completed",
                    json!({"content": message.get("content")}),
                );
            }
            if calls.is_empty() {
                // A message sent while the reply was being written gets its own answer.
                if session.borrow().steers.is_empty() {
                    return Ok(());
                }
                continue;
            }
            for (call, call_id) in calls.into_iter().zip(ids) {
                let function = &call["function"];
                let result = match (
                    function["name"].as_str(),
                    function["arguments"]
                        .as_str()
                        .map(serde_json::from_str::<Value>),
                ) {
                    (Some(name), Some(Ok(arguments))) => {
                        invoke(session, &call_id, name, arguments, mcp).await
                    }
                    _ => fail("invalid tool request"),
                };
                let mut content = match result {
                    Ok(result) => json!({"ok": true, "result": result}).to_string(),
                    Err(error) => json!({"ok": false, "error": model_error(&error)}).to_string(),
                };
                let mut s = session.borrow_mut();
                let room = CONTEXT_LIMIT
                    .saturating_sub(size(&s.messages))
                    .min(RESULT_LIMIT)
                    .saturating_sub(256);
                if content.len() > room {
                    // Escaping can double the partial text; keep it inside the room.
                    let mut cut = room / 2;
                    while !content.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    content = json!({"ok": true, "truncated": true, "original_bytes": content.len(), "partial": &content[..cut]}).to_string();
                }
                s.messages
                    .push(json!({"role": "tool", "tool_call_id": call_id, "content": content}));
            }
        }
        fail("turn step budget exceeded")
    };
    let result = tokio::time::timeout(Duration::from_secs(600), steps)
        .await
        .unwrap_or_else(|_| fail("turn deadline exceeded"));
    guard.finished = true;
    let status = if result.is_ok() {
        "completed"
    } else {
        "failed"
    };
    let mut s = session.borrow_mut();
    close_batch(&mut s.messages);
    s.emit(
        "turn.completed",
        json!({"status": status, "duration_ms": guard.started.elapsed().as_millis() as u64}),
    );
    drop(s);
    result
}

pub fn assistant(content: &str, calls: Vec<Value>) -> Value {
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert("content".into(), json!(content));
    if !calls.is_empty() {
        message.insert("tool_calls".into(), json!(calls));
    }
    Value::Object(message)
}

#[cfg(test)]
mod transcript_tests {
    use super::*;

    #[test]
    fn open_batches_are_answered_and_old_turns_trimmed_whole() {
        let call = |id: &str| json!({"id": id, "type": "function", "function": {"name": "file_read", "arguments": "{}"}});
        let mut messages = vec![
            json!({"role": "user", "content": "a"}),
            assistant("", vec![call("1"), call("2")]),
            json!({"role": "tool", "tool_call_id": "1", "content": "{}"}),
        ];
        close_batch(&mut messages);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3]["tool_call_id"], "2");
        close_batch(&mut messages);
        assert_eq!(messages.len(), 4);
        messages.push(json!({"role": "user", "content": "x".repeat(1000)}));
        trim(&mut messages, 1100);
        assert_eq!(
            (messages.len(), messages[0]["role"].as_str()),
            (1, Some("user"))
        );
        trim(&mut messages, 10);
        assert_eq!(messages.len(), 1);
    }

    // Recoverable path errors may guide the model; protected paths stay private.
    #[test]
    fn a_model_hears_only_the_tool_failures_it_can_correct() {
        assert!(
            model_error(&ToolError("workspace path unavailable".into())).contains("files_list")
        );
        assert_eq!(
            model_error(&ToolError(
                "protected service data is outside tool authority".into()
            )),
            "tool-operation-unavailable"
        );
        assert_eq!(
            model_error(&ToolError("/home/someone/.ssh unreadable".into())),
            "tool-operation-unavailable"
        );
    }
}
