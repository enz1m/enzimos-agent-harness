// SPDX-License-Identifier: MIT
//! Send one JSON control request; turn events stream as JSON lines. Never
//! approves actions: approvals need a separate explicit approval.resolve.
use enzimos_agent_harness::service::{MAX_FRAME, private_directory};
use serde_json::{Value, json};
use std::{
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};
use tokio::io::{AsyncWriteExt, BufReader};

#[allow(unsafe_code, reason = "Linux syscall ABI (getuid).")]
async fn exchange(path: PathBuf, mut request: Value) -> Result<Value, ()> {
    private_directory(path.parent().ok_or(())?).map_err(|_| ())?;
    let m = std::fs::symlink_metadata(&path).map_err(|_| ())?;
    if !m.file_type().is_socket() || m.uid() != unsafe { libc::getuid() } || m.mode() & 0o077 != 0 {
        return Err(());
    }
    let stream = tokio::net::UnixStream::connect(&path)
        .await
        .map_err(|_| ())?;
    if stream.peer_cred().map(|c| c.uid()).ok() != Some(unsafe { libc::getuid() }) {
        return Err(());
    }
    request["schema_version"] = json!(1);
    let mut raw = request.to_string().into_bytes();
    raw.push(b'\n');
    if raw.len() > MAX_FRAME {
        return Err(());
    }
    let (reader, mut writer) = stream.into_split();
    tokio::time::timeout(Duration::from_secs(5), writer.write_all(&raw))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    let mut reader = BufReader::new(reader);
    loop {
        // A turn can wait ninety seconds for each exact user decision.
        let mut line = Vec::new();
        let n = tokio::time::timeout(
            Duration::from_secs(605),
            enzimos_agent_harness::provider::read_line(&mut reader, &mut line, MAX_FRAME),
        )
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
        if n == 0 || line.len() > MAX_FRAME || !line.ends_with(b"\n") {
            return Err(());
        }
        let frame: Value = serde_json::from_slice(&line).map_err(|_| ())?;
        println!("{frame}");
        if frame.get("event").is_none() {
            return Ok(frame);
        }
    }
}

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let (mut socket, mut request) = (None, None);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--socket" => socket = rest.next().map(PathBuf::from),
            "--request" => request = rest.next().cloned(),
            _ => {
                eprintln!("usage: enzimos-agent-harness-client --socket PATH [--request JSON]");
                return ExitCode::from(2);
            }
        }
    }
    let Some(socket) = socket else {
        eprintln!("usage: enzimos-agent-harness-client --socket PATH [--request JSON]");
        return ExitCode::from(2);
    };
    let raw = match request {
        Some(r) => r.into_bytes(),
        None => {
            let mut raw = Vec::new();
            let _ = std::io::stdin()
                .take(MAX_FRAME as u64 + 1)
                .read_to_end(&mut raw);
            raw
        }
    };
    let request = serde_json::from_slice::<Value>(&raw)
        .ok()
        .filter(|r| raw.len() <= MAX_FRAME && r["method"].is_string());
    let reply = match (
        request,
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
    ) {
        (Some(request), Ok(runtime)) => runtime.block_on(exchange(socket, request)),
        _ => Err(()),
    };
    match reply {
        Ok(reply) if reply["ok"] == true => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(()) => {
            println!("{}", json!({"ok": false, "error": "harness-unavailable"}));
            ExitCode::FAILURE
        }
    }
}
