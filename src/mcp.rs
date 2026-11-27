// SPDX-License-Identifier: MIT
//! Bounded explicit stdio MCP host, including the enrolled Center adapter, and
//! the per-turn aggregation of configured servers' tools.
use crate::{Result, ToolError, fail, http_mcp::HttpClient, sha256_hex};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashSet},
    process::Stdio,
    rc::Rc,
    time::Duration,
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};

const MAX_FRAME: usize = 1024 * 1024;
pub const PROTOCOL: &str = "2025-11-25";

struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

pub struct StdioClient {
    argv: Vec<String>,
    environment: BTreeMap<String, String>,
    process: tokio::sync::Mutex<Option<Process>>,
    group: Cell<Option<i32>>,
    request_id: Cell<u64>,
}

impl Drop for StdioClient {
    #[allow(unsafe_code, reason = "Linux syscall ABI (kill).")]
    fn drop(&mut self) {
        // Disconnect is authoritative even if a server ignores cancellation.
        if let Some(group) = self.group.take() {
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
    }
}

impl StdioClient {
    pub fn new(argv: &Value, environment: &Value) -> Result<Rc<Self>> {
        let argv = argv
            .as_array()
            .filter(|a| !a.is_empty() && a.len() <= 64)
            .and_then(|a| {
                a.iter()
                    .map(|v| {
                        v.as_str()
                            .filter(|s| !s.contains('\0') && s.len() <= 4096)
                            .map(str::to_owned)
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .filter(|a| a[0].starts_with('/'))
            .ok_or_else(|| {
                ToolError("MCP requires explicit existing executable arguments".into())
            })?;
        let environment = match environment {
            Value::Null => BTreeMap::new(),
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| {
                    if !["HOME", "XDG_RUNTIME_DIR", "PATH", "LANG"].contains(&k.as_str()) {
                        return fail("MCP environment requires an explicit supported key");
                    }
                    let value = v.as_str().ok_or_else(|| {
                        ToolError("MCP environment values must be strings".into())
                    })?;
                    Ok((k.clone(), value.to_owned()))
                })
                .collect::<Result<_>>()?,
            _ => return fail("MCP environment requires an explicit supported key"),
        };
        Ok(Rc::new(Self {
            argv,
            environment,
            process: tokio::sync::Mutex::new(None),
            group: Cell::new(None),
            request_id: Cell::new(0),
        }))
    }

    pub async fn start(self: &Rc<Self>) -> Result<()> {
        let mut command = tokio::process::Command::new(&self.argv[0]);
        command
            .args(&self.argv[1..])
            .env_clear()
            .env("PATH", "/usr/bin")
            .env("LANG", "C.UTF-8")
            .envs(&self.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|_| ToolError("MCP transport unavailable".into()))?;
        self.group.set(child.id().map(|p| p as i32));
        let (stdin, stdout) = (
            child.stdin.take().unwrap(),
            BufReader::new(child.stdout.take().unwrap()),
        );
        *self.process.lock().await = Some(Process {
            child,
            stdin,
            stdout,
        });
        let result = async {
            let response = self.request("initialize", json!({"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "enzimos-agent-harness", "version": "0.2.0"}})).await?;
            if ![PROTOCOL, "2025-06-18", "2025-03-26", "2024-11-05"].contains(&response["protocolVersion"].as_str().unwrap_or_default()) {
                return fail("unsupported MCP protocol");
            }
            let mut guard = self.process.lock().await;
            let process = guard.as_mut().ok_or_else(|| ToolError("MCP not started".into()))?;
            send(&mut process.stdin, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await
        }
        .await;
        if result.is_err() {
            self.close().await;
        }
        result
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut guard = self.process.lock().await;
        let process = guard
            .as_mut()
            .ok_or_else(|| ToolError("MCP not started".into()))?;
        let id = self.request_id.get() + 1;
        self.request_id.set(id);
        let exchange = async {
            send(
                &mut process.stdin,
                &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            )
            .await?;
            let mut consumed = 0;
            for _ in 0..128 {
                let mut line = Vec::new();
                let n = crate::provider::read_line(&mut process.stdout, &mut line, MAX_FRAME)
                    .await
                    .map_err(|_| ToolError("MCP transport unavailable".into()))?;
                consumed += n;
                if n == 0 || n > MAX_FRAME || consumed > 2 * MAX_FRAME {
                    return fail("MCP response unavailable or exceeds limit");
                }
                let body: Value = serde_json::from_slice(&line)
                    .map_err(|_| ToolError("invalid MCP frame".into()))?;
                if body["jsonrpc"] != "2.0" {
                    return fail("invalid MCP frame");
                }
                if body.get("id") == Some(&json!(id)) && body.get("method").is_none() {
                    if body.get("error").is_some() {
                        return fail("MCP call refused");
                    }
                    return body
                        .get("result")
                        .filter(|r| r.is_object())
                        .cloned()
                        .ok_or_else(|| ToolError("invalid MCP result".into()));
                }
                if body.get("method").is_some() && body.get("id").is_some() {
                    // Sampling, roots and elicitation are never silently granted.
                    send(&mut process.stdin, &json!({"jsonrpc": "2.0", "id": body["id"], "error": {"code": -32601, "message": "Client capability not enabled"}})).await?;
                }
            }
            fail("MCP notification budget exceeded")
        };
        let result = tokio::time::timeout(Duration::from_secs(60), exchange)
            .await
            .unwrap_or_else(|_| fail("MCP transport unavailable"));
        if result.is_err() {
            drop(guard);
            self.close().await;
        }
        result
    }

    async fn close_center(&self) {
        let process = self.process.lock().await.take();
        if let Some(Process {
            mut child,
            stdin,
            stdout: _stdout,
        }) = process
        {
            drop(stdin);
            if tokio::time::timeout(Duration::from_millis(300), child.wait())
                .await
                .is_ok()
            {
                self.group.set(None);
            }
        }
        self.close().await;
    }

    #[allow(unsafe_code, reason = "Linux syscall ABI (kill).")]
    pub async fn close(&self) {
        if let Some(group) = self.group.take() {
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        if let Some(mut process) = self.process.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), process.child.wait()).await;
        }
    }
}

async fn send(stdin: &mut ChildStdin, body: &Value) -> Result<()> {
    let mut raw = body.to_string().into_bytes();
    raw.push(b'\n');
    if raw.len() > MAX_FRAME {
        return fail("MCP request exceeds limit");
    }
    stdin
        .write_all(&raw)
        .await
        .map_err(|_| ToolError("MCP transport unavailable".into()))?;
    stdin
        .flush()
        .await
        .map_err(|_| ToolError("MCP transport unavailable".into()))
}

pub enum Client {
    Stdio(Rc<StdioClient>),
    Http(Rc<HttpClient>),
}

impl Client {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match self {
            Client::Stdio(c) => c.request(method, params).await,
            Client::Http(c) => c.request(method, params).await,
        }
    }
    async fn close(&self) {
        match self {
            Client::Stdio(c) => c.close().await,
            Client::Http(c) => c.close().await,
        }
    }
}

/// Explicitly enabled servers for one turn; every call needs harness approval.
pub struct Mcp {
    servers: Value,
    clients: RefCell<Vec<Rc<Client>>>,
    routes: RefCell<BTreeMap<String, (Rc<Client>, String)>>,
    pub schemas: RefCell<Vec<Value>>,
    center: RefCell<Option<Rc<StdioClient>>>,
    center_routes: RefCell<BTreeMap<String, &'static str>>,
    approval_identities: RefCell<BTreeMap<String, String>>,
}

fn server_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=24).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

fn tool_name(name: &str) -> bool {
    (1..=40).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

impl Mcp {
    pub fn new(servers: Value) -> Result<Self> {
        if servers.as_object().is_some_and(|s| s.len() > 8) {
            return fail("MCP server limit exceeded");
        }
        Ok(Self {
            servers,
            clients: RefCell::new(vec![]),
            routes: RefCell::new(BTreeMap::new()),
            schemas: RefCell::new(vec![]),
            center: RefCell::default(),
            center_routes: RefCell::default(),
            approval_identities: RefCell::default(),
        })
    }

    pub fn has(&self, alias: &str) -> bool {
        self.routes.borrow().contains_key(alias) || self.uses_center(alias)
    }

    pub(crate) fn uses_center(&self, alias: &str) -> bool {
        self.center_routes.borrow().contains_key(alias)
    }

    pub(crate) async fn start_center(&self) -> Result<()> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| ToolError(CENTER_UNAVAILABLE.into()))?;
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| home.map(|path| path.join(".local/state")))
            .ok_or_else(|| ToolError(CENTER_UNAVAILABLE.into()))?;
        let credential = state.join("enzui/agents/dev.enzui.Assistant.credential.json");
        let socket = runtime.join("enzui/center.sock");
        if !credential.is_file() || !socket.exists() {
            return fail(CENTER_UNAVAILABLE);
        }
        let binary = std::env::current_exe()
            .ok()
            .map(|path| path.with_file_name("enzui-mcp"))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| "/usr/bin/enzui-mcp".into());
        let client = StdioClient::new(
            &json!([binary, "--socket", socket, "--credential", credential]),
            &Value::Null,
        )?;
        client.start().await?;
        let mut schemas = Vec::new();
        let mut routes = BTreeMap::new();
        // Inspection is independent of the prompt and preserves the order of the model prefix.
        for (alias, id) in CENTER_TOOLS {
            let reply = client
                .request(
                    "tools/call",
                    json!({
                        "name":"enzui_function",
                        "arguments":{"app_id":SETTINGS_APP, "function_id":id}
                    }),
                )
                .await?;
            let function = &center_result(&reply)?["function"];
            if function["id"] != *id || function["input_schema"]["type"] != "object" {
                return fail(CENTER_UNAVAILABLE);
            }
            if self.has(alias) {
                return fail("duplicate MCP tool alias");
            }
            schemas.push(json!({"type":"function","function":{
                "name":alias,"description":function["description"],"parameters":function["input_schema"]
            }}));
            routes.insert((*alias).to_owned(), *id);
        }
        self.schemas.borrow_mut().extend(schemas);
        self.center_routes.replace(routes);
        self.center.replace(Some(client));
        Ok(())
    }

    pub(crate) fn approval_identity(&self, alias: &str) -> String {
        self.approval_identities.borrow()[alias].clone()
    }

    pub async fn start(&self) -> Result<()> {
        let result = self.discover().await;
        if result.is_err() {
            self.close().await;
        }
        result
    }

    async fn discover(&self) -> Result<()> {
        for (name, config) in self.servers.as_object().cloned().unwrap_or_default() {
            if !server_name(&name) {
                return fail("invalid MCP server identity");
            }
            if config.get("enabled") != Some(&json!(true)) {
                continue;
            }
            let client = Rc::new(
                match config
                    .get("transport")
                    .and_then(Value::as_str)
                    .unwrap_or("stdio")
                {
                    "stdio" => {
                        let c = StdioClient::new(
                            &config["argv"],
                            config.get("environment").unwrap_or(&Value::Null),
                        )?;
                        self.clients
                            .borrow_mut()
                            .push(Rc::new(Client::Stdio(c.clone())));
                        c.start().await?;
                        Client::Stdio(c)
                    }
                    "http" => {
                        let c = HttpClient::new(
                            config["url"].as_str().unwrap_or_default(),
                            config.get("authorization").and_then(Value::as_str),
                        )?;
                        self.clients
                            .borrow_mut()
                            .push(Rc::new(Client::Http(c.clone())));
                        c.start().await?;
                        Client::Http(c)
                    }
                    _ => return fail("unsupported MCP transport"),
                },
            );
            // Explicit selection keeps large catalogs out of the model context.
            let selected = match config.get("selected_tools") {
                None | Some(Value::Null) => None,
                Some(Value::Array(list)) if list.len() <= 32 => {
                    let names = list
                        .iter()
                        .map(|v| v.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| ToolError("invalid MCP tool selection".into()))?;
                    if names.iter().collect::<HashSet<_>>().len() != names.len() {
                        return fail("invalid MCP tool selection");
                    }
                    Some(names)
                }
                _ => return fail("invalid MCP tool selection"),
            };
            let mut tools = Vec::new();
            let mut cursors = HashSet::new();
            let mut cursor: Option<String> = None;
            let mut catalog_bytes = 0;
            let mut complete = false;
            for _ in 0..8 {
                let result = client
                    .request(
                        "tools/list",
                        cursor.as_ref().map_or(json!({}), |c| json!({"cursor": c})),
                    )
                    .await?;
                let rows = result["tools"]
                    .as_array()
                    .ok_or_else(|| ToolError("invalid MCP tool catalog".into()))?;
                catalog_bytes += json!(rows).to_string().len();
                tools.extend(rows.iter().cloned());
                if tools.len() > 256 || catalog_bytes > 2 * MAX_FRAME {
                    return fail("MCP catalog exceeds discovery limit");
                }
                match result.get("nextCursor") {
                    None | Some(Value::Null) => {
                        complete = true;
                        break;
                    }
                    Some(Value::String(c)) if c.is_empty() => {
                        complete = true;
                        break;
                    }
                    Some(Value::String(c)) if c.len() <= 4096 && cursors.insert(c.clone()) => {
                        cursor = Some(c.clone())
                    }
                    _ => return fail("invalid MCP pagination cursor"),
                }
            }
            if !complete {
                return fail("MCP catalog page budget exceeded");
            }
            let names = tools
                .iter()
                .map(|t| t["name"].as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| ToolError("invalid duplicate MCP catalog names".into()))?;
            if names.iter().collect::<HashSet<_>>().len() != names.len() {
                return fail("invalid duplicate MCP catalog names");
            }
            if let Some(selected) = &selected {
                if !selected.iter().all(|s| names.contains(s)) {
                    return fail("selected MCP tool unavailable");
                }
                tools.retain(|t| selected.iter().any(|s| t["name"] == s.as_str()));
            }
            if tools.len() > 32 {
                return fail("select at most32 tools from this MCP server");
            }
            for tool in tools {
                let original = tool["name"].as_str().unwrap_or_default().to_owned();
                if !tool_name(&original) {
                    return fail("unsupported MCP tool name");
                }
                let mut alias = format!("mcp_{name}_{original}");
                if alias.len() > 64 {
                    alias = format!("{}_{}", &alias[..53], &sha256_hex(alias.as_bytes())[..10]);
                }
                if self.routes.borrow().contains_key(&alias) {
                    return fail("duplicate MCP tool alias");
                }
                let schema = tool
                    .get("inputSchema")
                    .filter(|s| s["type"] == "object")
                    .cloned()
                    .ok_or_else(|| ToolError("invalid MCP schema".into()))?;
                let description = tool
                    .get("description")
                    .map(|d| d.as_str().map_or_else(|| d.to_string(), str::to_owned))
                    .unwrap_or_default();
                let description = description.chars().take(1024).collect::<String>();
                let identity =
                    crate::runtime::fingerprint(&json!([name, original, schema, description]));
                self.approval_identities
                    .borrow_mut()
                    .insert(alias.clone(), format!("mcp:{identity}"));
                self.routes
                    .borrow_mut()
                    .insert(alias.clone(), (client.clone(), original));
                self.schemas.borrow_mut().push(json!({"type": "function", "function": {"name": alias, "description": description, "parameters": schema}}));
            }
            if json!(*self.schemas.borrow()).to_string().len() > 65536 {
                return fail("MCP discovery context exceeds limit");
            }
        }
        Ok(())
    }

    /// Calls one discovered tool.
    ///
    /// # Errors
    /// Returns an error for an unknown tool, a refused call or transport failure.
    pub async fn call(&self, alias: &str, arguments: Value) -> Result<Value> {
        let center_route = self.center_routes.borrow().get(alias).copied();
        if let Some(id) = center_route {
            let client = self
                .center
                .borrow()
                .clone()
                .ok_or_else(|| ToolError(CENTER_UNAVAILABLE.into()))?;
            return call_center(&client, id, arguments).await;
        }
        let route = self.routes.borrow().get(alias).cloned();
        let (client, name) = route.ok_or_else(|| ToolError("unknown MCP tool".into()))?;
        client
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    pub async fn close(&self) {
        let clients = std::mem::take(&mut *self.clients.borrow_mut());
        for client in clients {
            client.close().await;
        }
        let center = self.center.take();
        if let Some(center) = center {
            center.close_center().await;
        }
        self.center_routes.borrow_mut().clear();
        self.routes.borrow_mut().clear();
        self.schemas.borrow_mut().clear();
        self.approval_identities.borrow_mut().clear();
    }
}

const SETTINGS_APP: &str = "dev.enzui.Settings";
pub(crate) const CENTER_UNAVAILABLE: &str = "The desktop MCP Center is unavailable.";
const CENTER_TOOLS: &[(&str, &str)] = &[
    ("search_settings", "settings.search"),
    ("get_setting", "settings.read"),
    ("set_setting", "settings.change"),
    ("open_app", "desktop.open"),
    ("start_timer", "desktop.timer"),
    ("device_status", "desktop.status"),
    ("find_downloads", "desktop.downloads"),
];

fn center_result(reply: &Value) -> Result<Value> {
    if reply["isError"] == true {
        let error = reply["content"][0]["text"]
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or_default();
        return fail(match error["code"].as_str() {
            Some("permission_required") => "center-approval-pending",
            Some("permission_denied") => crate::runtime::ACTION_DECLINED,
            Some("invalid_arguments" | "invalid_value" | "invalid") => {
                "Invalid setting or tool arguments."
            }
            Some("locked") => crate::device::LOCKED,
            Some("no_backlight") => crate::device::NO_BACKLIGHT,
            Some("app") => crate::device::APP_UNAVAILABLE,
            Some("folder") => crate::device::FOLDER_UNAVAILABLE,
            Some("limit") => crate::device::TIMER_LIMIT,
            _ => CENTER_UNAVAILABLE,
        });
    }
    if let Some(result) = reply.get("structuredContent") {
        return Ok(result.clone());
    }
    reply["content"][0]["text"]
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .ok_or_else(|| ToolError(CENTER_UNAVAILABLE.into()))
}

async fn call_center(client: &StdioClient, id: &str, arguments: Value) -> Result<Value> {
    let request = json!({"name":"enzui_call","arguments":{
        "app_id":SETTINGS_APP, "function_id":id, "arguments":arguments,
    }});
    // Matches the MCP Center's longest call so the user has time to read the consent dialog.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let reply = client.request("tools/call", request.clone()).await?;
        match center_result(&reply) {
            Err(error) if error.0 == "center-approval-pending" => {
                if tokio::time::Instant::now() >= deadline {
                    return fail(crate::runtime::ACTION_DECLINED);
                }
                tokio::time::sleep(Duration::from_millis(750)).await;
            }
            result => return result.map(|mut value| value["result"].take()),
        }
    }
}
