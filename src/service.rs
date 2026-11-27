// SPDX-License-Identifier: MIT
//! Minimal private per-user service; the shell owns presentation. One JSON
//! request line per connection; turns stream event lines until the outcome.
use crate::{
    Result, ToolError, desktop, fail,
    mcp::Mcp,
    provider::Gateway,
    runtime::{self, ApprovalMode, Complete, Session, Shared},
    screenshot::Access,
    skills::Skills,
    tools::Workspace,
    uid,
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream, unix::OwnedWriteHalf},
};

pub const MAX_FRAME: usize = 1024 * 1024;

pub fn private_directory(path: &Path) -> Result<PathBuf> {
    let m = std::fs::symlink_metadata(path)
        .map_err(|_| ToolError("private owned directory required".into()))?;
    if !m.is_dir() || m.uid() != uid() || m.mode() & 0o077 != 0 {
        return fail("private owned directory required");
    }
    Ok(path.to_path_buf())
}

pub fn config_file(path: &Path) -> Result<Value> {
    let refuse = || ToolError("private owned configuration required".into());
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| refuse())?;
    let m = file.metadata().map_err(|_| refuse())?;
    if !m.is_file() || m.uid() != uid() || m.mode() & 0o077 != 0 || m.len() > 65536 {
        return Err(refuse());
    }
    let mut raw = Vec::new();
    (&mut file)
        .take(65537)
        .read_to_end(&mut raw)
        .map_err(|_| refuse())?;
    serde_json::from_slice(&raw).map_err(|_| refuse())
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/nonexistent"), PathBuf::from)
}

pub struct Service {
    config: Value,
    state: PathBuf,
    complete: Complete,
    gateway: Option<Rc<Gateway>>,
    sessions: RefCell<HashMap<String, Shared>>,
    clients: Cell<usize>,
    active: Cell<usize>,
    busy: RefCell<HashSet<String>>,
    skills: Skills,
    protected: Vec<PathBuf>,
}

fn session_id(value: &Value) -> Result<String> {
    value
        .as_str()
        .filter(|s| {
            s.len() == 32
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
        .map(str::to_owned)
        .ok_or_else(|| ToolError("invalid saved session identity".into()))
}

const BASE_INSTRUCTIONS: &str = "You are the enzimOS assistant, working in the folder the user chose (the workspace). \
Use the tools to look at and change it. Paths are relative to the workspace, such as \"todo.txt\" or \"notes/plan.md\"; \
call files_list with a null path to see what is there. When a tool reports a path error, correct the path and call it again. \
Answer briefly, from what the tools returned.";

/// A saved transcript's title (its first request) and preview (its last
/// reply, else its last request), each one line of plain text.
pub(crate) fn headline(messages: &[Value]) -> (Option<String>, Option<String>) {
    let said = |role: &'static str| {
        messages
            .iter()
            .filter(move |m| m["role"] == role)
            .filter_map(|m| m["content"].as_str())
            .filter(|t| !t.trim().is_empty())
    };
    let line = |text: &str, length: usize| {
        text.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .chars()
            .take(length)
            .collect::<String>()
    };
    let title = said("user").next().map(|t| line(t, 60));
    let preview = said("assistant")
        .last()
        .or_else(|| said("user").last())
        .map(|t| line(t, 90));
    (title, preview)
}

/// A saved transcript as a client shows it: user and assistant text, and each
/// tool call with its result.
fn saved_title(body: &Value) -> Option<String> {
    body["title"]
        .as_str()
        .filter(|title| !title.trim().is_empty())
        .map(|title| title.chars().take(60).collect())
}

fn last_revision(body: &Value, messages: &[Value]) -> Option<u64> {
    if let Some(revision) = body.get("last_turn_revision") {
        return revision.as_u64();
    }
    let last = messages.last()?;
    if last["role"] == "assistant"
        && last["tool_calls"].is_null()
        && last["content"]
            .as_str()
            .is_some_and(|text| !text.trim().is_empty())
    {
        body["last_sequence"].as_u64()
    } else {
        None
    }
}

fn items(messages: &[Value]) -> Vec<Value> {
    let mut items = Vec::new();
    let mut calls: HashMap<String, usize> = HashMap::new();
    for message in messages {
        match message["role"].as_str().unwrap_or_default() {
            "user" => items.push(json!({"kind": "user", "text": message["content"],
                "attachments": crate::attachment::names(message)})),
            "assistant" => {
                if let Some(text) = message["content"].as_str().filter(|t| !t.is_empty()) {
                    items.push(json!({"kind": "assistant", "text": text}));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let arguments = call["function"]["arguments"]
                        .as_str()
                        .and_then(|a| serde_json::from_str::<Value>(a).ok())
                        .unwrap_or(Value::Null);
                    calls.insert(
                        call["id"].as_str().unwrap_or_default().to_owned(),
                        items.len(),
                    );
                    items.push(json!({"kind": "tool", "tool": call["function"]["name"], "title": "", "arguments": arguments, "output": "", "status": "cancelled"}));
                }
            }
            "tool" => {
                let Some(&index) = message["tool_call_id"]
                    .as_str()
                    .and_then(|id| calls.get(id))
                else {
                    continue;
                };
                let result = message["content"]
                    .as_str()
                    .and_then(|c| serde_json::from_str::<Value>(c).ok())
                    .unwrap_or(Value::Null);
                let (status, output) = match (&result["ok"], &result["result"], &result["error"]) {
                    (Value::Bool(true), Value::String(text), _) => ("completed", text.clone()),
                    (Value::Bool(true), other, _) => ("completed", other.to_string()),
                    (_, _, Value::String(error)) if error == "tool-call-cancelled" => {
                        ("cancelled", String::new())
                    }
                    (_, _, error) => ("failed", error.as_str().unwrap_or_default().to_owned()),
                };
                items[index]["status"] = json!(status);
                items[index]["output"] = json!(runtime::shown(&output));
            }
            _ => {}
        }
    }
    items
}

// Credentials and files that run code at login, under active and default XDG roots.
fn credential_paths(home: &Path, var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let mut paths = [
        ".ssh",
        ".gnupg",
        ".git-credentials",
        ".netrc",
        ".pgpass",
        ".aws",
        ".azure",
        ".kube",
        ".docker",
        ".password-store",
        ".mozilla",
        ".var/app",
        ".profile",
        ".bashrc",
        ".bash_profile",
        ".bash_login",
        ".bash_logout",
        ".zshenv",
        ".zshrc",
        ".zprofile",
        ".zlogin",
        ".zlogout",
        ".xprofile",
        ".xinitrc",
    ]
    .iter()
    .map(|path| home.join(path))
    .collect::<Vec<_>>();
    for (variable, default, private) in [
        (
            "XDG_CONFIG_HOME",
            ".config",
            &[
                "enzimos",
                "enzui/apps",
                "enzui/agents",
                "enzremote",
                "autostart",
                "systemd",
                "environment.d",
                "fish",
                "gh",
                "git/credentials",
                "gcloud",
                "chromium",
                "google-chrome",
                "BraveSoftware",
            ][..],
        ),
        (
            "XDG_DATA_HOME",
            ".local/share",
            &["keyrings", "enzremote", "systemd"],
        ),
        (
            "XDG_STATE_HOME",
            ".local/state",
            &["enzui", "enzimos-ai-gateway"],
        ),
    ] {
        let default = home.join(default);
        paths.extend(private.iter().map(|path| default.join(path)));
        if let Some(active) = var(variable)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && *path != default)
        {
            paths.extend(private.iter().map(|path| active.join(path)));
        }
    }
    paths
}

impl Service {
    pub fn new(
        config: Value,
        state: &Path,
        complete: Option<Complete>,
        protected: &[PathBuf],
    ) -> Result<Rc<Self>> {
        let state = private_directory(state)?;
        let (complete, gateway) = match complete {
            Some(complete) => (complete, None),
            None => {
                let gateway = Rc::new(Gateway {
                    path: PathBuf::from(
                        config["gateway_socket"]
                            .as_str()
                            .ok_or_else(|| ToolError("gateway socket required".into()))?,
                    ),
                });
                let client = Rc::clone(&gateway);
                let complete: Complete = Rc::new(move |body, delta| {
                    let gateway = Rc::clone(&client);
                    Box::pin(async move { gateway.complete(body, delta).await })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>>>>
                });
                (complete, Some(gateway))
            }
        };
        let mut paths = vec![state.clone()];
        paths.extend(credential_paths(&home(), |name| std::env::var_os(name)));
        paths.extend(protected.iter().cloned());
        let configured = config.get("protected_paths").cloned().unwrap_or(json!([]));
        let configured = configured
            .as_array()
            .filter(|c| c.len() <= 32)
            .and_then(|c| {
                c.iter()
                    .map(|p| p.as_str().filter(|p| p.starts_with('/')).map(PathBuf::from))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| ToolError("invalid protected service paths".into()))?;
        paths.extend(configured);
        let skills = Skills::new(config.get("skill_directories").unwrap_or(&json!([])))?;
        Ok(Rc::new(Self {
            config,
            state,
            complete,
            gateway,
            sessions: RefCell::new(HashMap::new()),
            clients: Cell::new(0),
            active: Cell::new(0),
            busy: RefCell::new(HashSet::new()),
            skills,
            protected: paths,
        }))
    }

    fn save(&self, session: &Session) -> Result<()> {
        let body = json!({"schema_version": 1, "id": session.id, "workspace": session.workspace.path, "provider": session.provider, "model": session.model,
            "approval_mode": session.approval_mode.saved(), "title": session.title, "messages": session.messages, "last_turn_revision": session.last_turn_revision, "last_sequence": session.sequence, "profile": session.profile});
        self.write_saved(&session.id, &body)
    }

    fn write_saved(&self, id: &str, body: &Value) -> Result<()> {
        let raw = body.to_string();
        if raw.len() > 2 * MAX_FRAME {
            return fail("session state exceeds limit");
        }
        let temporary = self.state.join(format!(".session-{}", runtime::token()));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temporary)?;
            file.write_all(raw.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, self.state.join(format!("{id}.json")))
        })();
        let _ = std::fs::remove_file(&temporary);
        result.map_err(|_| ToolError("session state unavailable".into()))
    }

    fn create(&self, request: &Value) -> Result<Value> {
        if self.sessions.borrow().len() >= 16 {
            // Every turn is saved, so closing the longest-idle session loses only
            // its event buffer; clients that crashed never close theirs.
            let idle = self
                .sessions
                .borrow()
                .iter()
                .filter(|(id, s)| s.borrow().task.is_none() && !self.busy.borrow().contains(*id))
                .min_by_key(|(_, s)| s.borrow().last_active)
                .map(|(id, _)| id.clone());
            match idle {
                Some(id) => {
                    self.sessions.borrow_mut().remove(&id);
                }
                None => return fail("session limit exceeded"),
            }
        }
        let profile_name = match request.get("profile") {
            None => "default".to_owned(),
            Some(name) => name
                .as_str()
                .ok_or_else(|| ToolError("invalid session policy profile".into()))?
                .to_owned(),
        };
        let profile = self.config["profiles"]
            .get(&profile_name)
            .filter(|p| p.is_object())
            .ok_or_else(|| ToolError("unknown session policy profile".into()))?;
        let workspace = std::fs::canonicalize(
            request["workspace"]
                .as_str()
                .ok_or_else(|| ToolError("workspace required".into()))?,
        )
        .map_err(|_| ToolError("workspace unavailable".into()))?;
        let roots = profile["workspace_roots"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|r| {
                r.as_str()
                    .and_then(|r| std::fs::canonicalize(r).ok())
                    .ok_or_else(|| ToolError("workspace root unavailable".into()))
            })
            .collect::<Result<Vec<_>>>()?;
        if !roots.iter().any(|root| workspace.starts_with(root)) {
            return fail("workspace outside approved profile roots");
        }
        let provider = match request
            .get("provider")
            .or_else(|| self.config.get("default_provider"))
        {
            None => "local".to_owned(),
            Some(name) => name
                .as_str()
                .ok_or_else(|| ToolError("invalid provider".into()))?
                .to_owned(),
        };
        let providers = self
            .config
            .get("providers")
            .cloned()
            .unwrap_or(json!(["local"]));
        if !providers
            .as_array()
            .is_some_and(|p| p.contains(&json!(provider)))
        {
            return fail("provider not approved");
        }
        let model = match request.get("model") {
            None | Some(Value::Null) if provider == "local" => "",
            Some(Value::String(model))
                if (provider == "local" || !model.is_empty()) && model.chars().count() <= 128 =>
            {
                model.as_str()
            }
            _ => return fail("invalid model identity"),
        };
        let approval_mode = match request.get("approval_mode") {
            Some(value) => ApprovalMode::parse(value)?,
            None => match ApprovalMode::parse(&self.config["enzui"]["approval_default"]) {
                Ok(ApprovalMode::WorkspaceEdits) => ApprovalMode::WorkspaceEdits,
                _ => ApprovalMode::Ask,
            },
        };
        if approval_mode == ApprovalMode::FullSend {
            return fail("set full send on an open session with explicit confirmation");
        }
        let screenshots = Access::parse(request.get("screenshots"))?;
        let session = Session::new(
            Workspace::new(&workspace, &self.protected)?,
            &provider,
            model,
            approval_mode,
            screenshots,
            &profile_name,
        )?;
        self.save(&session)?;
        let id = session.id.clone();
        self.sessions
            .borrow_mut()
            .insert(id.clone(), Rc::new(RefCell::new(session)));
        Ok(
            json!({"session_id": id, "approval_mode": approval_mode.wire(), "screenshots": screenshots.wire()}),
        )
    }

    fn stored(&self, identity: &Value) -> Result<Value> {
        let identity = session_id(identity)?;
        let path = self.state.join(format!("{identity}.json"));
        let invalid = || ToolError("invalid saved session file".into());
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|_| invalid())?;
        let m = file.metadata().map_err(|_| invalid())?;
        if !m.is_file()
            || m.uid() != uid()
            || m.mode() & 0o077 != 0
            || m.len() > 2 * MAX_FRAME as u64
        {
            return Err(invalid());
        }
        let mut raw = Vec::new();
        (&mut file)
            .take(2 * MAX_FRAME as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(|_| invalid())?;
        if raw.len() > 2 * MAX_FRAME {
            return fail("saved session exceeds limit");
        }
        let body: Value = serde_json::from_slice(&raw).map_err(|_| invalid())?;
        if body["schema_version"] != 1
            || !body["schema_version"].is_u64()
            || body["id"] != identity.as_str()
        {
            return fail("invalid saved session format");
        }
        Ok(body)
    }

    fn saved_list(&self) -> Result<Value> {
        let mut names = std::fs::read_dir(&self.state)
            .map_err(|_| ToolError("session state unavailable".into()))?
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".json"))
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        names.sort();
        let mut rows = Vec::new();
        for name in names {
            if rows.len() >= 128 {
                return fail("saved session limit exceeded");
            }
            let body = self.stored(&json!(name))?;
            let updated = std::fs::metadata(self.state.join(format!("{name}.json")))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            let (fallback, preview) =
                headline(body["messages"].as_array().map_or(&[][..], Vec::as_slice));
            let title = saved_title(&body).or(fallback);
            rows.push(json!({"id": body["id"], "workspace": body["workspace"], "provider": body["provider"], "model": body["model"], "profile": body["profile"],
                "title": title, "preview": preview, "updated": updated}));
        }
        Ok(json!(rows))
    }

    /// Reopen under the CURRENT profile policy, at the last complete tool boundary.
    fn resume(&self, request: &Value) -> Result<Value> {
        let identity = session_id(&request["session_id"])?;
        if let Some(open) = self.sessions.borrow().get(&identity) {
            // A client that crashed without closing can take its idle session back.
            if open.borrow().task.is_some() || self.busy.borrow().contains(&identity) {
                return fail("session already open");
            }
            return Ok(
                json!({"session_id": identity, "approval_mode": open.borrow().approval_mode.wire(), "screenshots": open.borrow().screenshots.wire()}),
            );
        }
        let body = self.stored(&json!(identity))?;
        let messages = body["messages"]
            .as_array()
            .cloned()
            .ok_or_else(|| ToolError("invalid saved transcript".into()))?;
        let sequence = body["last_sequence"]
            .as_u64()
            .ok_or_else(|| ToolError("invalid saved transcript".into()))?;
        if messages.iter().any(|m| {
            !["user", "assistant", "tool"].contains(&m["role"].as_str().unwrap_or_default())
        }) {
            return fail("invalid saved transcript messages");
        }
        let mut complete = Vec::new();
        let mut pending: HashSet<String> = HashSet::new();
        let mut boundary = 0;
        for message in &messages {
            match message["role"].as_str().unwrap_or_default() {
                "assistant" => {
                    let calls = match message.get("tool_calls") {
                        None => vec![],
                        Some(Value::Array(c)) => c.clone(),
                        _ => return fail("invalid saved tool batch"),
                    };
                    pending = calls
                        .iter()
                        .map(|c| {
                            c["id"]
                                .as_str()
                                .map(str::to_owned)
                                .ok_or_else(|| ToolError("invalid saved tool batch".into()))
                        })
                        .collect::<Result<_>>()?;
                }
                "tool" => {
                    let call = message["tool_call_id"].as_str().unwrap_or_default();
                    if !pending.remove(call) {
                        return fail("orphan saved tool result");
                    }
                }
                // A batch interrupted by a crash is answered as cancelled, as a
                // live cancel does, instead of making the history unusable.
                _ if !pending.is_empty() => {
                    for id in std::mem::take(&mut pending) {
                        complete.push(json!({"role": "tool", "tool_call_id": id, "content": json!({"ok": false, "error": "tool-call-cancelled"}).to_string()}));
                    }
                    boundary = complete.len();
                }
                _ => {}
            }
            complete.push(message.clone());
            if pending.is_empty() {
                boundary = complete.len();
            }
        }
        let created = self.create(&json!({"workspace": body["workspace"], "provider": body["provider"], "model": body["model"], "profile": body["profile"],
            "approval_mode": ApprovalMode::parse(&body["approval_mode"]).unwrap_or_default().saved()}))?;
        let temporary_id = created["session_id"].as_str().unwrap().to_owned();
        let session = self.sessions.borrow_mut().remove(&temporary_id).unwrap();
        let _ = std::fs::remove_file(self.state.join(format!("{temporary_id}.json")));
        {
            let mut s = session.borrow_mut();
            s.id = identity.clone();
            s.messages = complete[..boundary].to_vec();
            s.title = saved_title(&body).or_else(|| headline(&s.messages).0);
            s.sequence = sequence;
            s.last_turn_revision = last_revision(&body, &s.messages);
            self.save(&s)?;
        }
        let approval_mode = session.borrow().approval_mode.wire();
        self.sessions.borrow_mut().insert(identity.clone(), session);
        Ok(
            json!({"session_id": identity, "approval_mode": approval_mode, "messages": boundary, "discarded_incomplete_messages": messages.len() - boundary}),
        )
    }

    fn text<'a>(request: &'a Value, key: &str) -> Result<&'a str> {
        request[key]
            .as_str()
            .ok_or_else(|| ToolError("invalid request".into()))
    }

    fn dispatch(&self, request: &Value) -> Result<Value> {
        match request["method"].as_str().unwrap_or_default() {
            "session.create" => {
                if self.saved_list()?.as_array().map_or(0, Vec::len) >= 128 {
                    return fail("saved session limit reached; explicitly delete old sessions");
                }
                return self.create(request);
            }
            "session.list" => return self.saved_list(),
            "session.resume" => return self.resume(request),
            "session.export" => {
                let mut body = self.stored(&request["session_id"])?;
                let messages = body["messages"].as_array().map_or(&[][..], Vec::as_slice);
                let (items, (title, _)) = (items(messages), headline(messages));
                body["last_turn_revision"] = json!(last_revision(&body, messages));
                body["items"] = json!(items);
                if request["items_only"] == true {
                    body.as_object_mut().unwrap().remove("messages");
                }
                body["title"] = json!(saved_title(&body).or(title));
                return Ok(body);
            }
            "session.rename" => {
                let id = session_id(&request["session_id"])?;
                if self.busy.borrow().contains(&id) {
                    return fail("stop the turn before renaming the chat");
                }
                let title = Self::text(request, "title")?.trim();
                if title.is_empty()
                    || title.chars().count() > 60
                    || title.chars().any(char::is_control)
                {
                    return fail("invalid chat title");
                }
                let sessions = self.sessions.borrow();
                if let Some(session) = sessions.get(&id) {
                    let mut session = session.borrow_mut();
                    let previous = session.title.replace(title.to_owned());
                    if let Err(error) = self.save(&session) {
                        session.title = previous;
                        return Err(error);
                    }
                } else {
                    let mut body = self.stored(&json!(id))?;
                    body["title"] = json!(title);
                    self.write_saved(&id, &body)?;
                }
                return Ok(json!({"title": title}));
            }
            "session.delete" => {
                let id = session_id(&request["session_id"])?;
                if self.busy.borrow().contains(&id) {
                    return fail("stop the turn before deleting its history");
                }
                self.stored(&json!(id))?;
                std::fs::remove_file(self.state.join(format!("{id}.json")))
                    .map_err(|_| ToolError("session state unavailable".into()))?;
                self.sessions.borrow_mut().remove(&id);
                return Ok(json!({"deleted": true}));
            }
            "skills.list" => return self.skills.discover(),
            "skills.create" => {
                let directory = Self::text(request, "directory")?;
                if !self.config["writable_skill_directories"]
                    .as_array()
                    .is_some_and(|d| d.contains(&json!(directory)))
                {
                    return fail("skill destination not enabled for user creation");
                }
                return self.skills.create(
                    directory,
                    Self::text(request, "name")?,
                    Self::text(request, "description")?,
                    Self::text(request, "instructions")?,
                );
            }
            "skills.load" => {
                let skill = self
                    .skills
                    .load(Self::text(request, "name")?, Self::text(request, "sha256")?)?;
                return Ok(
                    json!({"name": skill.name, "description": skill.description, "sha256": skill.sha256, "instructions": skill.instructions}),
                );
            }
            _ => {}
        }
        let id = request["session_id"].as_str().unwrap_or_default();
        let session = self
            .sessions
            .borrow()
            .get(id)
            .cloned()
            .ok_or_else(|| ToolError("unknown session".into()))?;
        match request["method"].as_str().unwrap_or_default() {
            "events.list" => {
                let s = session.borrow();
                let cursor = request.get("after").cloned().unwrap_or(json!(0));
                let cursor = cursor
                    .as_u64()
                    .filter(|c| cursor.is_u64() && *c <= s.sequence)
                    .ok_or_else(|| ToolError("invalid event cursor".into()))?;
                // One page fits one frame; `more` asks the client to continue
                // from the last returned sequence.
                let mut events = Vec::new();
                let mut bytes = 0;
                let mut more = false;
                for event in s
                    .events
                    .iter()
                    .filter(|e| e["sequence"].as_u64().unwrap_or(0) > cursor)
                {
                    let n = event.to_string().len() + 1;
                    if !events.is_empty() && bytes + n > MAX_FRAME - 4096 {
                        more = true;
                        break;
                    }
                    bytes += n;
                    events.push(event.clone());
                }
                let first = s.events.front().and_then(|e| e["sequence"].as_u64());
                Ok(
                    json!({"events": events, "gap": first.is_some_and(|f| cursor + 1 < f), "more": more, "last_sequence": s.sequence}),
                )
            }
            "session.set_approval_mode" => {
                if self.busy.borrow().contains(id) {
                    return fail("stop the turn before changing approval mode");
                }
                let mode = ApprovalMode::parse(&request["approval_mode"])?;
                let mut s = session.borrow_mut();
                let previous = s.approval_mode;
                s.configure_mode(mode, request["confirm_full_send"] == true)?;
                if let Err(error) = self.save(&s) {
                    s.approval_mode = previous;
                    return Err(error);
                }
                Ok(json!({"approval_mode": mode.wire()}))
            }
            "session.set_screenshots" => {
                if self.busy.borrow().contains(id) {
                    return fail("stop the turn before changing screenshot access");
                }
                let access = Access::parse(request.get("screenshots"))?;
                session.borrow_mut().set_screenshots(access);
                Ok(json!({"screenshots": access.wire()}))
            }
            "approval.resolve" => {
                session.borrow_mut().resolve(
                    Self::text(request, "request_id")?,
                    Self::text(request, "binding")?,
                    &request["allow"],
                    request.get("remember"),
                )?;
                Ok(json!({"resolved": true}))
            }
            "turn.steer" => {
                session.borrow_mut().steer(Self::text(request, "text")?)?;
                Ok(json!({"steered": true}))
            }
            "turn.cancel" => {
                session.borrow().cancel();
                Ok(json!({"cancel_requested": true}))
            }
            "session.close" => {
                if session.borrow().task.is_some() || self.busy.borrow().contains(id) {
                    return fail("cancel the active turn before closing");
                }
                self.sessions.borrow_mut().remove(id);
                Ok(json!({"closed": true}))
            }
            _ => fail("unsupported service operation"),
        }
    }

    async fn warmup(&self, request: &Value) -> Result<Value> {
        let (provider, model) =
            if let Some(id) = request.get("session_id").filter(|id| !id.is_null()) {
                let id = session_id(id)?;
                let saved = self.stored(&json!(id))?;
                (saved["provider"].clone(), saved["model"].clone())
            } else {
                (
                    self.config
                        .get("default_provider")
                        .cloned()
                        .unwrap_or(json!("local")),
                    self.config["enzui"]["model"].clone(),
                )
            };
        if provider != "local" || self.active.get() != 0 {
            return Ok(json!({"warmed": false}));
        }
        let Some(gateway) = &self.gateway else {
            return Ok(json!({"warmed": false}));
        };
        let model = match model.as_str().filter(|name| !name.is_empty()) {
            Some(name) => name.to_owned(),
            None => crate::local::pick_model(
                &gateway.local_models().await?,
                crate::local::Task::Text,
                false,
            )
            .ok_or_else(|| ToolError(crate::provider::LOCAL_MODELS_UNAVAILABLE.into()))?,
        };
        gateway.warmup(&model).await
    }

    async fn send(writer: &mut OwnedWriteHalf, body: &Value) -> Result<()> {
        let mut raw = body.to_string().into_bytes();
        raw.push(b'\n');
        if raw.len() > MAX_FRAME {
            return fail("service response exceeds limit");
        }
        tokio::time::timeout(Duration::from_secs(5), writer.write_all(&raw))
            .await
            .map_err(|_| ToolError("client stalled".into()))?
            .map_err(|_| ToolError("client closed".into()))
    }

    async fn turn<R: tokio::io::AsyncRead + Unpin>(
        self: &Rc<Self>,
        request: &Value,
        reader: &mut R,
        writer: &mut OwnedWriteHalf,
    ) -> Result<()> {
        let id = request["session_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let session = self.sessions.borrow().get(&id).cloned();
        let Some(session) = session.filter(|s| {
            !self.busy.borrow().contains(&id) && s.borrow().task.is_none() && self.active.get() < 2
        }) else {
            return fail("session unavailable or service busy");
        };
        let active_mode = session.borrow().approval_mode;
        match request.get("approval_mode") {
            Some(mode) if ApprovalMode::parse(mode)? == active_mode => {}
            None if active_mode != ApprovalMode::FullSend => {}
            _ => return fail("approval mode changed; reopen the conversation"),
        }
        if Access::parse(request.get("screenshots"))? != session.borrow().screenshots {
            return fail("screenshot access changed; reopen the conversation");
        }
        let (prompt, attachments, replace, previous) = {
            let state = session.borrow();
            if request["method"] == "turn.revise" {
                let index = state
                    .messages
                    .iter()
                    .rposition(|m| m["role"] == "user")
                    .ok_or_else(|| ToolError("stale-turn-revision".into()))?;
                let revision = state
                    .last_turn_revision
                    .filter(|revision| request["expected_revision"].as_u64() == Some(*revision))
                    .ok_or_else(|| ToolError("stale-turn-revision".into()))?;
                let previous = &state.messages[index];
                let prompt = request
                    .get("prompt")
                    .unwrap_or(&previous["content"])
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                    .ok_or_else(|| ToolError("Enter a message to send.".into()))?;
                (
                    prompt.to_owned(),
                    crate::attachment::validate(previous.get("attachments"))?,
                    Some(index),
                    Some((state.messages.clone(), revision)),
                )
            } else {
                (
                    Self::text(request, "prompt")?.to_owned(),
                    crate::attachment::validate(request.get("attachments"))?,
                    None,
                    None,
                )
            }
        };
        // Fallible setup runs before the session is marked busy.
        let mcp = Rc::new(Mcp::new(
            self.config.get("mcp_servers").cloned().unwrap_or(json!({})),
        )?);
        self.active.set(self.active.get() + 1);
        self.busy.borrow_mut().insert(id.clone());
        let (queue, mut events) = tokio::sync::mpsc::channel(256);
        session.borrow_mut().observers.push(queue);
        let task = {
            let (service, session, mcp, request) =
                (self.clone(), session.clone(), mcp.clone(), request.clone());
            tokio::task::spawn_local(async move {
                // One deadline and cancellation owner covers discovery and execution.
                let work = async {
                    let selected = request.get("skills").cloned().unwrap_or(json!([]));
                    let selected = selected
                        .as_array()
                        .filter(|s| s.len() <= 8)
                        .ok_or_else(|| ToolError("skill selection limit exceeded".into()))?;
                    let mut instructions = vec![BASE_INSTRUCTIONS.to_owned()];
                    for choice in selected {
                        let valid = choice.as_object().is_some_and(|c| {
                            c.len() == 2 && c.contains_key("name") && c.contains_key("sha256")
                        });
                        if !valid {
                            return fail("invalid selected skill");
                        }
                        let skill = service.skills.load(
                            choice["name"].as_str().unwrap_or_default(),
                            choice["sha256"].as_str().unwrap_or_default(),
                        )?;
                        instructions.push(format!("Skill: {}\n{}", skill.name, skill.instructions));
                    }
                    let instructions = instructions.join("\n\n");
                    if instructions.len() > 131072 {
                        return fail("selected skills exceed context limit");
                    }
                    mcp.start().await?;
                    let active = (!mcp.schemas.borrow().is_empty()).then_some(&*mcp);
                    let (automatic, task, screenshots) = {
                        let session = session.borrow();
                        let task = if selected.is_empty() && active.is_none() {
                            let history = replace
                                .map_or(session.messages.as_slice(), |i| &session.messages[..i]);
                            crate::local::Task::from_turn(&prompt, history)
                        } else {
                            crate::local::Task::Full
                        };
                        (
                            session.provider == "local" && session.model.is_empty(),
                            task,
                            session.screenshots,
                        )
                    };
                    let has_images = crate::attachment::has_images(&attachments);
                    let screen_request = crate::local::Task::asks_for_screenshot(&prompt);
                    if screen_request && screenshots == Access::Off {
                        return fail(crate::screenshot::ACCESS_OFF);
                    }
                    let model = if automatic {
                        let gateway = service.gateway.as_ref().ok_or_else(|| {
                            ToolError(crate::provider::LOCAL_MODELS_UNAVAILABLE.into())
                        })?;
                        crate::local::pick_model(
                            &gateway.local_models().await?,
                            task,
                            has_images || (screen_request && screenshots == Access::Vision),
                        )
                        .ok_or_else(|| {
                            ToolError(crate::provider::LOCAL_MODELS_UNAVAILABLE.into())
                        })?
                    } else {
                        session.borrow().model.clone()
                    };
                    if has_images
                        && (automatic
                            || crate::local::Profile::is_builtin(
                                &session.borrow().provider,
                                &model,
                            ))
                        && model != crate::local::Profile::VISION_MODEL
                    {
                        return fail(crate::attachment::VISION_REQUIRED);
                    }
                    if screen_request
                        && screenshots == Access::Vision
                        && (automatic
                            || crate::local::Profile::is_builtin(
                                &session.borrow().provider,
                                &model,
                            ))
                        && model != crate::local::Profile::VISION_MODEL
                    {
                        return fail(crate::screenshot::VISION_REQUIRED);
                    }
                    runtime::run_configured(
                        &session,
                        &prompt,
                        &service.complete,
                        active,
                        8,
                        runtime::TurnProfile {
                            instructions: Some(&instructions),
                            model: &model,
                            task,
                            attachments,
                            replace,
                        },
                    )
                    .await
                    .map_err(|error| {
                        if has_images && error.0 == crate::provider::IMAGE_REJECTED {
                            ToolError(crate::attachment::VISION_REQUIRED.into())
                        } else {
                            error
                        }
                    })
                };
                tokio::time::timeout(Duration::from_secs(600), work)
                    .await
                    .unwrap_or_else(|_| fail(crate::provider::PROVIDER_TIMEOUT))
            })
        };
        session.borrow_mut().task = Some(task.abort_handle());
        let mut task = task;
        let outcome = loop {
            tokio::select! {
                biased;
                _ = reader.read_u8() => {
                    task.abort();
                    let _ = (&mut task).await;
                    break None;
                }
                result = &mut task => break Some(result),
                event = events.recv() => {
                    if let Some(event) = event
                        && Self::send(writer, &json!({"event": event})).await.is_err() {
                            task.abort();
                            let _ = (&mut task).await;
                            break None;
                        }
                }
            }
        };
        while let Ok(event) = events.try_recv() {
            if outcome.is_some() {
                let _ = Self::send(writer, &json!({"event": event})).await;
            }
        }
        mcp.close().await;
        {
            let mut s = session.borrow_mut();
            s.observers.clear();
            s.task = None;
        }
        self.active.set(self.active.get() - 1);
        self.busy.borrow_mut().remove(&id);
        let restore = |messages, revision| {
            let mut s = session.borrow_mut();
            s.messages = messages;
            s.last_turn_revision = Some(revision);
        };
        if let Some((messages, revision)) = previous {
            if matches!(&outcome, Some(Ok(Ok(())))) {
                let saved = self.save(&session.borrow());
                if saved.is_err() {
                    restore(messages, revision);
                }
                saved?;
            } else {
                restore(messages, revision);
            }
        } else {
            self.save(&session.borrow())?;
        }
        match outcome {
            None => Ok(()),
            Some(Err(join)) if join.is_cancelled() => {
                Self::send(
                    writer,
                    &json!({"ok": true, "turn_finished": true, "status": "cancelled"}),
                )
                .await
            }
            Some(Ok(Ok(()))) => {
                Self::send(writer, &json!({"ok": true, "turn_finished": true})).await
            }
            Some(Ok(Err(error))) => Err(error),
            Some(Err(_)) => fail("turn failed"),
        }
    }

    async fn client(self: Rc<Self>, stream: UnixStream) {
        self.clients.set(self.clients.get() + 1);
        let peer = stream.peer_cred().map(|c| c.uid()).ok();
        let mut reader = BufReader::with_capacity(65536, stream);
        let result = async {
            if self.clients.get() > 16 {
                return fail("client limit exceeded");
            }
            if peer != Some(uid()) {
                return fail("peer refused");
            }
            let mut line = Vec::new();
            tokio::time::timeout(
                Duration::from_secs(5),
                (&mut reader)
                    .take(MAX_FRAME as u64 + 1)
                    .read_until(b'\n', &mut line),
            )
            .await
            .map_err(|_| ToolError("request timed out".into()))?
            .map_err(|_| ToolError("request unavailable".into()))?;
            if !line.ends_with(b"\n") || line.len() > MAX_FRAME {
                return fail("invalid request frame");
            }
            let request: Value =
                serde_json::from_slice(&line).map_err(|_| ToolError("invalid request".into()))?;
            if request["schema_version"] != 1 || !request["schema_version"].is_u64() {
                return fail("unsupported request version");
            }
            let desktop = if request["method"] == "session.desktop_control" {
                let peer = desktop::ShellPeer::new(reader.get_ref())
                    .ok_or_else(|| ToolError(desktop::UNAVAILABLE.into()))?;
                Some((
                    peer,
                    session_id(&request["session_id"])?,
                    session_id(&request["generation"])?,
                ))
            } else {
                None
            };
            if !reader.buffer().is_empty() {
                return fail("client sent excess request data");
            }
            Ok((request, desktop))
        }
        .await;
        let (input, mut writer) = reader.into_inner().into_split();
        let mut reader = BufReader::with_capacity(65536, input);
        let result = async {
            let (request, desktop) = result?;
            if let Some((peer, id, generation)) = desktop {
                let (session, commands) = {
                    let sessions = self.sessions.borrow();
                    let session = sessions
                        .get(&id)
                        .ok_or_else(|| ToolError(desktop::UNAVAILABLE.into()))?;
                    let (sender, commands) = tokio::sync::mpsc::channel(1);
                    let mut state = session.borrow_mut();
                    if state
                        .desktop
                        .as_ref()
                        .is_some_and(|active| !active.commands.is_closed())
                    {
                        return fail(desktop::UNAVAILABLE);
                    }
                    state.desktop = Some(desktop::Control {
                        generation: generation.clone(),
                        active: false,
                        commands: sender,
                    });
                    (Rc::downgrade(session), commands)
                };
                Self::send(&mut writer, &json!({"ok":true, "generation":generation})).await?;
                desktop::exchange(session, generation, reader, &mut writer, peer, commands).await;
                Ok(())
            } else if request["method"] == "model.warmup" {
                let result = tokio::select! {
                    _ = reader.read_u8() => return Ok(()),
                    result = tokio::time::timeout(Duration::from_secs(45), self.warmup(&request)) => {
                        result.map_err(|_| ToolError(crate::provider::PROVIDER_TIMEOUT.into()))??
                    }
                };
                Self::send(&mut writer, &json!({"ok": true, "result": result})).await
            } else if matches!(
                request["method"].as_str(),
                Some("turn.start" | "turn.revise")
            ) {
                self.turn(&request, &mut reader, &mut writer).await
            } else {
                let result = self.dispatch(&request)?;
                Self::send(&mut writer, &json!({"ok": true, "result": result})).await
            }
        }
        .await;
        if let Err(error) = result {
            // Only reasons a client can act on are distinguished.
            use crate::provider::{
                GATEWAY_UNAVAILABLE, LOCAL_MODELS_UNAVAILABLE, PROVIDER_TIMEOUT,
            };
            let code = match error.0.as_str() {
                Skills::CHANGED
                | GATEWAY_UNAVAILABLE
                | LOCAL_MODELS_UNAVAILABLE
                | PROVIDER_TIMEOUT
                | crate::screenshot::ACCESS_OFF
                | crate::screenshot::VISION_REQUIRED
                | crate::attachment::INVALID
                | crate::attachment::VISION_REQUIRED
                | "local-context-limit"
                | "stale-turn-revision" => error.0.as_str(),
                _ => "request-unavailable",
            };
            let _ = Self::send(&mut writer, &json!({"ok": false, "error": code})).await;
        }
        let _ = writer.shutdown().await;
        self.clients.set(self.clients.get() - 1);
    }

    pub async fn serve(self: Rc<Self>, listener: UnixListener) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                // Back off on descriptor exhaustion instead of spinning.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            tokio::task::spawn_local(self.clone().client(stream));
        }
    }

    /// Cancel running turns and save every open transcript; aborted turn
    /// handlers never reach their own save.
    pub fn close(&self) {
        for session in self.sessions.borrow().values() {
            session.borrow().cancel();
            let _ = self.save(&session.borrow());
        }
    }
}

#[allow(unsafe_code, reason = "Linux syscall ABI (umask).")]
pub fn bind(path: &Path) -> Result<UnixListener> {
    private_directory(
        path.parent()
            .ok_or_else(|| ToolError("socket directory required".into()))?,
    )?;
    if std::fs::symlink_metadata(path).is_ok() {
        return fail("new service socket required");
    }
    unsafe { libc::umask(0o077) };
    let listener = UnixListener::bind(path).map_err(|_| ToolError("socket unavailable".into()))?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|_| ToolError("socket unavailable".into()))?;
    Ok(listener)
}

#[cfg(test)]
mod tests;
