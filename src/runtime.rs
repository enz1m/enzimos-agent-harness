// SPDX-License-Identifier: MIT
//! Sessions, exact expiring tool approvals and a provider-independent bounded tool loop.
use crate::{
    Result, ToolError, fail,
    local::{Profile, Task},
    mcp::Mcp,
    screenshot::{self, Access},
    tools::{self, Workspace},
};
use serde_json::{Map, Value, json};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    pin::Pin,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

pub use crate::provider::Delta;

/// One provider completion: the request body and where streamed text goes.
pub type Complete = Rc<dyn Fn(Value, Delta) -> Pin<Box<dyn Future<Output = Result<Value>>>>>;

fn tools_schema(screenshots: Access) -> Vec<Value> {
    let tool = |name: &str, description: &str, properties: Value, required: Value| {
        json!({"type": "function", "function": {"name": name, "description": description,
            "parameters": {"type": "object", "properties": properties, "required": required, "additionalProperties": false}}})
    };
    let mut tools = vec![
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
    ];
    if screenshots != Access::Off {
        tools.push(tool(
            "screenshot",
            "Capture the desktop or the focused window through the desktop portal. If the portal lacks focused-window support, ask the user before capturing the whole screen.",
            json!({"target": {"type":"string", "enum":["screen","focused_window"]}}),
            json!(["target"]),
        ));
    }
    tools
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
    (screenshot::UNAVAILABLE, screenshot::UNAVAILABLE),
    (screenshot::UNSUPPORTED, screenshot::UNSUPPORTED),
    ("invalid screenshot target", "invalid screenshot target"),
    (
        "one screenshot per model step",
        "one screenshot per model step",
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

/// Controls when a live session asks before executing a tool.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApprovalMode {
    /// Review each tool call that can change state or reach outside the workspace.
    #[default]
    Ask,
    /// Permit writes through the workspace file tool.
    WorkspaceEdits,
    /// Skip harness approvals while keeping tool boundaries.
    FullSend,
}

impl ApprovalMode {
    pub(crate) fn parse(value: &Value) -> Result<Self> {
        match value.as_str() {
            Some("ask") => Ok(Self::Ask),
            Some("workspace_edits") => Ok(Self::WorkspaceEdits),
            Some("full_send") => Ok(Self::FullSend),
            _ => fail("invalid approval mode"),
        }
    }

    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::WorkspaceEdits => "workspace_edits",
            Self::FullSend => "full_send",
        }
    }

    pub(crate) fn saved(self) -> &'static str {
        match self {
            Self::FullSend => Self::Ask.wire(),
            mode => mode.wire(),
        }
    }
}

#[derive(Clone, Copy)]
enum ApprovalDecision {
    Deny,
    Once,
    Session,
}

struct Pending {
    sender: Option<oneshot::Sender<ApprovalDecision>>,
    binding: String,
    turn_id: Option<String>,
    expires: Instant,
}

pub struct Session {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub(crate) approval_mode: ApprovalMode,
    pub(crate) screenshots: Access,
    screenshot_granted: bool,
    allowed_tools: HashSet<String>,
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
    pub(crate) fn new(
        workspace: Workspace,
        provider: &str,
        model: &str,
        approval_mode: ApprovalMode,
        screenshots: Access,
        profile: &str,
    ) -> Result<Self> {
        Ok(Self {
            id: token(),
            provider: provider.into(),
            model: model.into(),
            approval_mode,
            screenshots,
            screenshot_granted: false,
            allowed_tools: HashSet::new(),
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

    pub fn resolve(
        &mut self,
        request_id: &str,
        binding: &str,
        allow: &Value,
        remember: Option<&Value>,
    ) -> Result<()> {
        let decision = match (allow.as_bool(), remember) {
            (Some(false), None | Some(Value::Bool(false))) => ApprovalDecision::Deny,
            (Some(true), None | Some(Value::Bool(false))) => ApprovalDecision::Once,
            (Some(true), Some(Value::Bool(true))) => ApprovalDecision::Session,
            _ => return fail("invalid approval decision"),
        };
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
        let _ = sender.send(decision);
        Ok(())
    }

    pub(crate) fn configure_mode(&mut self, mode: ApprovalMode, confirmed: bool) -> Result<()> {
        if mode == ApprovalMode::FullSend && !confirmed {
            return fail("full send requires confirmation");
        }
        if self.approval_mode != mode {
            self.approval_mode = mode;
            self.allowed_tools.clear();
            self.screenshot_granted = false;
        }
        Ok(())
    }

    pub(crate) fn set_screenshots(&mut self, access: Access) {
        if self.screenshots != access {
            self.screenshots = access;
            self.screenshot_granted = false;
            self.allowed_tools.remove("builtin:screenshot");
        }
    }

    fn needs_approval(&self, name: &str, arguments: &Value, grant: &str) -> bool {
        if self.approval_mode == ApprovalMode::FullSend
            || matches!(name, "files_list" | "file_read")
            || (name == "screenshot" && self.screenshot_granted)
            || self.allowed_tools.contains(grant)
        {
            return false;
        }
        !(name == "file_write"
            && self.approval_mode == ApprovalMode::WorkspaceEdits
            && arguments["path"]
                .as_str()
                .is_some_and(|path| self.workspace.contains_write_path(path)))
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

async fn approval(
    session: &Shared,
    call_id: &str,
    name: &str,
    arguments: &Value,
    grant: &str,
) -> bool {
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
    match tokio::time::timeout(Duration::from_secs(90), receiver).await {
        Ok(Ok(ApprovalDecision::Session)) => {
            session.borrow_mut().allowed_tools.insert(grant.to_owned());
            true
        }
        Ok(Ok(ApprovalDecision::Once)) => true,
        _ => false,
    }
}

async fn invoke(
    session: &Shared,
    call_id: &str,
    name: &str,
    arguments: Value,
    mcp: Option<&Mcp>,
    image: &mut Option<Value>,
) -> Result<Value> {
    let arguments = match arguments {
        Value::Object(map) if Value::Object(map.clone()).to_string().len() <= 128 * 1024 => map,
        _ => return fail("tool arguments exceed limit"),
    };
    let screenshots = session.borrow().screenshots;
    let schemas = tools_schema(screenshots);
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
    let grant = match builtin {
        Some(_) => format!("builtin:{name}"),
        None => mcp.unwrap().approval_identity(name),
    };
    let arguments_value = Value::Object(arguments.clone());
    let needs_approval = session
        .borrow()
        .needs_approval(name, &arguments_value, &grant);
    let flag = |k: &str| arguments.get(k).and_then(Value::as_bool).unwrap_or(false);
    session.borrow_mut().emit(
        "tool.started",
        json!({"call_id": call_id, "tool": name, "arguments": Value::Object(arguments.clone())}),
    );
    let started = Instant::now();
    let outcome = async {
        if needs_approval && !approval(session, call_id, name, &arguments_value, &grant).await {
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
            "screenshot" => {
                if image.is_some() {
                    return fail("one screenshot per model step");
                }
                let capture = screenshot::capture(screenshots, &text("target")?).await?;
                session.borrow_mut().screenshot_granted = true;
                if let Some(url) = capture.image_url {
                    *image = Some(screenshot::message(call_id, url));
                }
                Ok(json!({"description": capture.description, "image_attached": image.is_some()}))
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
    let model = session.borrow().model.clone();
    run_configured(
        session,
        prompt,
        complete,
        mcp,
        max_steps,
        TurnProfile {
            instructions,
            model: &model,
            task: Task::Full,
        },
    )
    .await
}

pub(crate) struct TurnProfile<'a> {
    pub(crate) instructions: Option<&'a str>,
    pub(crate) model: &'a str,
    pub(crate) task: Task,
}

pub(crate) async fn run_configured(
    session: &Shared,
    prompt: &str,
    complete: &Complete,
    mcp: Option<&Mcp>,
    max_steps: usize,
    TurnProfile {
        instructions,
        model,
        task,
    }: TurnProfile<'_>,
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
    let (provider, screenshots) = {
        let s = session.borrow();
        (s.provider.clone(), s.screenshots)
    };
    let known_local = Profile::is_builtin(&provider, model);
    let mut tools = tools_schema(if known_local && model != Profile::VISION_MODEL {
        Access::Off
    } else {
        screenshots
    });
    if let Some(mcp) = mcp {
        tools.extend(mcp.schemas.borrow().iter().cloned());
    }
    let full_tools = tools;
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
        s.emit("turn.started", json!({"model": model}));
    }
    let mut guard = TurnGuard {
        session: session.clone(),
        finished: false,
        started: Instant::now(),
    };
    let mut incomplete = false;
    let steps = async {
        let mut task = task;
        let mut seen = HashSet::new();
        let mut image: Option<Value> = None;
        for _ in 0..max_steps {
            let mut messages = {
                let mut s = session.borrow_mut();
                if s.take_steers()
                    && task != Task::Full
                    && let Some((steer, history)) = s.messages.split_last()
                    && let Some(text) = steer["content"].as_str()
                {
                    task = Task::from_turn(text, history);
                }
                trim(&mut s.messages, CONTEXT_LIMIT);
                if size(&s.messages) > CONTEXT_LIMIT {
                    return fail("session context exceeds limit");
                }
                s.messages.clone()
            };
            let local = Profile::for_model(
                &provider,
                model,
                task,
                instructions.unwrap_or_default(),
                &full_tools,
            );
            let (instructions, tools) = match &local {
                Some(profile) => (
                    Some(profile.instructions.as_str()),
                    profile.tools.as_slice(),
                ),
                None => (instructions, full_tools.as_slice()),
            };
            if local.is_some() {
                let overhead = instructions.map_or(0, str::len) + size(tools);
                let limit = Profile::MESSAGE_BYTES.saturating_sub(overhead);
                trim(&mut messages, limit);
                if size(&messages) > limit {
                    incomplete = true;
                    return fail("local model context exceeded");
                }
            }
            if let Some(instructions) = instructions.filter(|i| !i.is_empty()) {
                messages.insert(0, json!({"role": "system", "content": instructions}));
            }
            // Screenshot bytes belong only to the next provider request.
            if let Some(visual) = image.take() {
                messages.push(visual);
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
            let mut request =
                json!({"provider": provider, "model": model, "messages": messages, "stream": true});
            if local.is_none() || !tools.is_empty() {
                request["tools"] = json!(tools);
            }
            if let Some(profile) = &local {
                request["conversation_id"] = json!(session.borrow().id);
                if profile.tune_sampling {
                    Profile::sample(&mut request);
                }
            }
            let message = complete(request, delta).await;
            close_thinking(&mut session.borrow_mut(), &thinking);
            let mut message = message?;
            incomplete |= message
                .as_object_mut()
                .and_then(|message| message.remove("finish_reason"))
                .is_some_and(|reason| reason == "length");
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
                incomplete |= message["content"]
                    .as_str()
                    .is_none_or(|content| content.trim().is_empty());
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
                    (Some(name), Some(Ok(Value::Object(mut arguments))))
                        if tools.iter().any(|tool| tool["function"]["name"] == name) =>
                    {
                        if name == "file_write"
                            && local
                                .as_ref()
                                .is_some_and(|profile| profile.optional_digest)
                        {
                            arguments.entry("expected_sha256").or_insert(Value::Null);
                        }
                        invoke(
                            session,
                            &call_id,
                            name,
                            Value::Object(arguments),
                            mcp,
                            &mut image,
                        )
                        .await
                    }
                    _ => {
                        incomplete = true;
                        fail("invalid tool request")
                    }
                };
                let mut content = match result {
                    Ok(result) => json!({"ok": true, "result": result}).to_string(),
                    Err(error) => json!({"ok": false, "error": model_error(&error)}).to_string(),
                };
                let mut s = session.borrow_mut();
                let room = CONTEXT_LIMIT
                    .saturating_sub(size(&s.messages))
                    .min(if known_local {
                        Profile::RESULT_BYTES
                    } else {
                        RESULT_LIMIT
                    })
                    .saturating_sub(256);
                if content.len() > room {
                    incomplete = true;
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
        incomplete = true;
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
    if known_local && incomplete {
        s.emit("turn.local_limit", json!({}));
    }
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

    #[tokio::test]
    async fn local_create_without_digest_cannot_replace_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("already.txt");
        std::fs::write(&target, "original").unwrap();
        let session = Rc::new(RefCell::new(
            Session::new(
                Workspace::new(dir.path(), &[]).unwrap(),
                "local",
                "minicpm5-1b",
                ApprovalMode::WorkspaceEdits,
                Access::Off,
                "default",
            )
            .unwrap(),
        ));
        let complete: Complete = Rc::new(|body, _| {
            Box::pin(async move {
                if body["messages"].as_array().unwrap().last().unwrap()["role"] == "tool" {
                    return Ok(assistant("Done.", vec![]));
                }
                let call = |id: &str, path: &str| {
                    json!({"id": id, "type": "function", "function": {"name": "file_write",
                        "arguments": json!({"path": path, "text": "new"}).to_string()}})
                };
                Ok(assistant(
                    "",
                    vec![call("existing", "already.txt"), call("new", "new.txt")],
                ))
            })
        });
        run_configured(
            &session,
            "Create new.txt and already.txt.",
            &complete,
            None,
            2,
            TurnProfile {
                instructions: None,
                model: "minicpm5-1b",
                task: Task::Write,
            },
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "original");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "new"
        );
    }

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
