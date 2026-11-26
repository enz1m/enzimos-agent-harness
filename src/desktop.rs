// SPDX-License-Identifier: MIT
use crate::{Result, ToolError, fail, provider::read_line, runtime::Session, uid};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    fs,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    rc::Weak,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc, oneshot},
};

pub(crate) const UNAVAILABLE: &str = "desktop control is no longer available";
pub(crate) const INVALID: &str = "desktop action refused";
const FRAME: usize = 4096;
const DEADLINE: Duration = Duration::from_secs(2);

pub(crate) struct Control {
    pub(crate) generation: String,
    pub(crate) active: bool,
    pub(crate) commands: mpsc::Sender<Command>,
}

pub(crate) struct Command {
    name: String,
    arguments: Value,
    reply: oneshot::Sender<Result<()>>,
}

pub(crate) fn schemas() -> Vec<Value> {
    let coordinate = json!({"type":"number","minimum":0,"exclusiveMaximum":1});
    let position = json!({"x":coordinate,"y":coordinate});
    let tool = |name, description: &str, properties, required| {
        json!({
            "type":"function", "function":{
                "name":name,
                "description":format!("{description} Use one desktop action per step. Verify its result with screenshot when available before acting again."),
                "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}
            }
        })
    };
    vec![
        tool(
            "computer_move",
            "Move the pointer to a position normalized to the desktop screenshot. The top left is (0,0).",
            position.clone(),
            json!(["x", "y"]),
        ),
        tool(
            "computer_click",
            "Left click at a position normalized to the desktop screenshot. The top left is (0,0).",
            position,
            json!(["x", "y"]),
        ),
        tool(
            "computer_scroll",
            "Scroll at the pointer. Positive steps scroll down or right.",
            json!({"x":{"type":"integer","minimum":-12,"maximum":12},"y":{"type":"integer","minimum":-12,"maximum":12}}),
            json!(["x", "y"]),
        ),
        tool(
            "computer_key",
            "Press and release a key. Use a letter, digit, Enter, Tab, Escape, Backspace, Delete, Space, Home, End, PageUp, PageDown, Up, Down, Left or Right. Optional modifiers are Ctrl, Alt, Shift and Super, separated by +, e.g. Ctrl+a.",
            json!({"key":{"type":"string","maxLength":32}}),
            json!(["key"]),
        ),
        tool(
            "computer_type",
            "Type up to 256 printable characters that the session keyboard layout can produce into the focused field.",
            json!({"text":{"type":"string","minLength":1,"maxLength":256}}),
            json!(["text"]),
        ),
    ]
}

pub(crate) async fn invoke(
    control: mpsc::Sender<Command>,
    name: &str,
    arguments: Value,
) -> Result<Value> {
    if arguments.to_string().len() > 2048 {
        return fail(INVALID);
    }
    let (reply, answer) = oneshot::channel();
    control
        .try_send(Command {
            name: name.into(),
            arguments,
            reply,
        })
        .map_err(|_| ToolError(UNAVAILABLE.into()))?;
    tokio::time::timeout(DEADLINE * 2, answer)
        .await
        .map_err(|_| ToolError(UNAVAILABLE.into()))?
        .map_err(|_| ToolError(UNAVAILABLE.into()))??;
    Ok(json!({"performed": true}))
}

pub(crate) struct ShellPeer(OwnedFd);

impl ShellPeer {
    #[allow(
        unsafe_code,
        reason = "Pin the socket's original Linux peer across process identity checks."
    )]
    pub(crate) fn new(socket: &UnixStream) -> Option<Self> {
        let credentials = socket.peer_cred().ok()?;
        if credentials.uid() != uid() {
            return None;
        }
        let pid = credentials.pid()?;
        let mut raw = -1;
        let mut len = std::mem::size_of_val(&raw) as libc::socklen_t;
        // SAFETY: getsockopt writes one descriptor to raw and its size to len.
        if unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&raw mut raw).cast::<libc::c_void>(),
                &mut len,
            )
        } != 0
        {
            return None;
        }
        // SAFETY: Linux returned a new owned pidfd on success.
        let peer = Self(unsafe { OwnedFd::from_raw_fd(raw) });
        let preview = std::env::var_os("ENZUI_DESKTOP_CONTROL_SHELL_EXE");
        let expected = preview.as_ref().map_or_else(
            || PathBuf::from("/usr/lib/enzui/bin/enzui-shell"),
            PathBuf::from,
        );
        let proc = PathBuf::from(format!("/proc/{pid}"));
        let running = fs::metadata(proc.join("exe")).ok()?;
        let trusted = fs::metadata(expected).ok()?;
        (running.is_file()
            && (running.dev(), running.ino()) == (trusted.dev(), trusted.ino())
            && clean_environment(&proc, preview.is_some())
            && peer.alive())
        .then_some(peer)
    }

    #[allow(
        unsafe_code,
        reason = "Linux exposes peer exit readiness on the pidfd."
    )]
    fn alive(&self) -> bool {
        let mut fd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd addresses one initialized poll descriptor.
        unsafe { libc::poll(&mut fd, 1, 0) == 0 }
    }
}

fn clean_environment(proc: &Path, preview: bool) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(env) = fs::read(proc.join("environ")) else {
        return false;
    };
    let library_path = if preview {
        std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default()
    } else {
        std::ffi::OsString::from("/usr/lib/enzui/compositor/prefix/lib")
    };
    env.split(|byte| *byte == 0).all(|entry| {
        !entry
            .strip_prefix(b"LD_PRELOAD=")
            .is_some_and(|value| !value.is_empty())
            && !entry
                .strip_prefix(b"LD_AUDIT=")
                .is_some_and(|value| !value.is_empty())
            && entry
                .strip_prefix(b"LD_LIBRARY_PATH=")
                .is_none_or(|value| value.is_empty() || value == library_path.as_bytes())
    })
}

pub(crate) async fn exchange(
    session: Weak<RefCell<Session>>,
    generation: String,
    mut input: BufReader<tokio::net::unix::OwnedReadHalf>,
    output: &mut tokio::net::unix::OwnedWriteHalf,
    peer: ShellPeer,
    mut commands: mpsc::Receiver<Command>,
) {
    let activate = async {
        let mut line = Vec::new();
        let frame = tokio::select! {
            biased;
            _ = commands.recv() => return None,
            result = read_line(&mut input, &mut line, FRAME) => result.ok()?,
        };
        if frame > FRAME || !line.ends_with(b"\n") || !peer.alive() {
            return None;
        }
        let frame: Value = serde_json::from_slice(&line).ok()?;
        if frame["generation"] != generation || frame["activate"] != true {
            return None;
        }
        {
            let state = session.upgrade()?;
            let mut state = state.borrow_mut();
            let control = state
                .desktop
                .as_mut()
                .filter(|control| control.generation == generation)?;
            control.active = true;
        }
        let reply = json!({"ok":true,"generation":generation});
        tokio::time::timeout(DEADLINE, output.write_all(format!("{reply}\n").as_bytes()))
            .await
            .ok()?
            .ok()
    };
    if tokio::time::timeout(Duration::from_secs(72), activate)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        commands_loop(&generation, &mut input, output, &peer, &mut commands).await;
    }
    if let Some(session) = session.upgrade() {
        let mut session = session.borrow_mut();
        if session
            .desktop
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            session.desktop = None;
        }
    }
}

async fn commands_loop(
    generation: &str,
    input: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    output: &mut tokio::net::unix::OwnedWriteHalf,
    peer: &ShellPeer,
    commands: &mut mpsc::Receiver<Command>,
) {
    loop {
        let command = tokio::select! {
            biased;
            _ = input.read_u8() => break,
            command = commands.recv() => match command {
                Some(command) => command,
                None => break,
            },
        };
        if !peer.alive() || command.reply.is_closed() {
            break;
        }
        let request =
            json!({"generation":generation,"tool":command.name,"arguments":command.arguments});
        let mut line = Vec::new();
        let exchange = async {
            output
                .write_all(format!("{request}\n").as_bytes())
                .await
                .ok()?;
            read_line(input, &mut line, FRAME).await.ok()?;
            if line.len() > FRAME || !line.ends_with(b"\n") || !peer.alive() {
                return None;
            }
            let value: Value = serde_json::from_slice(&line).ok()?;
            if value["generation"] != generation {
                return None;
            }
            match value["ok"].as_bool()? {
                true => Some(Ok(())),
                false => Some(fail(INVALID)),
            }
        };
        let Ok(Some(result)) = tokio::time::timeout(DEADLINE, exchange).await else {
            break;
        };
        let _ = command.reply.send(result);
    }
}
