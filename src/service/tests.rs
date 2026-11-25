// SPDX-License-Identifier: MIT
use super::*;
use std::os::unix::fs::PermissionsExt;

fn private() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A scripted provider: each call pops the next assistant message.
fn scripted(replies: Vec<Value>) -> (Complete, Rc<RefCell<Vec<Value>>>) {
    let replies = Rc::new(RefCell::new(replies));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = seen.clone();
    let complete: Complete = Rc::new(move |body, _delta| {
        log.borrow_mut().push(body);
        let next = replies.borrow_mut().remove(0);
        Box::pin(async move { Ok(next) })
    });
    (complete, seen)
}

fn call(id: &str, name: &str, arguments: Value) -> Value {
    json!({"id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}})
}

struct Fixture {
    socket: PathBuf,
    workspace: tempfile::TempDir,
    _dirs: Vec<tempfile::TempDir>,
}

async fn start(profile: Value, replies: Vec<Value>) -> (Fixture, Rc<RefCell<Vec<Value>>>) {
    let (complete, seen) = scripted(replies);
    (start_with(profile, complete).await, seen)
}

async fn start_with(profile: Value, complete: Complete) -> Fixture {
    let (state, run, workspace) = (private(), private(), tempfile::tempdir().unwrap());
    let mut profile = profile;
    profile["workspace_roots"] = json!([workspace.path()]);
    let config = json!({"providers": ["local"], "profiles": {"default": profile}});
    let service = Service::new(config, state.path(), Some(complete), &[]).unwrap();
    let socket = run.path().join("harness.sock");
    let listener = bind(&socket).unwrap();
    tokio::task::spawn_local(service.serve(listener));
    Fixture {
        socket,
        workspace,
        _dirs: vec![state, run],
    }
}

async fn request(socket: &Path, body: Value) -> Vec<Value> {
    let stream = UnixStream::connect(socket).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut body = body;
    body["schema_version"] = json!(1);
    writer
        .write_all(format!("{body}\n").as_bytes())
        .await
        .unwrap();
    let mut lines = BufReader::new(reader).lines();
    let mut frames = Vec::new();
    while let Some(line) = lines.next_line().await.unwrap() {
        frames.push(serde_json::from_str(&line).unwrap());
    }
    frames
}

async fn session(fixture: &Fixture) -> String {
    let created = request(
        &fixture.socket,
        json!({"method": "session.create", "workspace": fixture.workspace.path(), "model": "m"}),
    )
    .await;
    created[0]["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn approval_requested(fixture: &Fixture, id: &str) -> Value {
    loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let events = request(
            &fixture.socket,
            json!({"method": "events.list", "session_id": id}),
        )
        .await;
        if let Some(event) = events[0]["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "approval.requested")
        {
            return event.clone();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn read_only_tools_run_without_approval_and_turns_complete() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let replies = vec![
                runtime::assistant("", vec![call("c1", "file_read", json!({"path": "a.txt"}))]),
                runtime::assistant("done", vec![]),
            ];
            let (fixture, seen) = start(json!({}), replies).await;
            std::fs::write(fixture.workspace.path().join("a.txt"), "content").unwrap();
            let id = session(&fixture).await;
            let frames = request(
                &fixture.socket,
                json!({"method": "turn.start", "session_id": id, "prompt": "read it"}),
            )
            .await;
            assert_eq!(
                frames.last().unwrap(),
                &json!({"ok": true, "turn_finished": true})
            );
            let types = frames
                .iter()
                .filter_map(|f| f["event"]["type"].as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                types,
                [
                    "turn.started",
                    "message.completed",
                    "tool.started",
                    "tool.completed",
                    "message.completed",
                    "turn.completed"
                ]
            );
            let tool_message = seen.borrow()[1]["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["content"]
                .clone();
            assert!(tool_message.as_str().unwrap().contains("content"));
            let listed = request(&fixture.socket, json!({"method": "session.list"})).await;
            assert_eq!(listed[0]["result"][0]["id"], id);
            assert_eq!(listed[0]["result"][0]["title"], "read it");
            assert_eq!(listed[0]["result"][0]["preview"], "done");
            assert!(listed[0]["result"][0]["updated"].as_u64().unwrap() > 0);
            let exported = request(
                &fixture.socket,
                json!({"method": "session.export", "session_id": id}),
            )
            .await;
            let items = exported[0]["result"]["items"].as_array().unwrap();
            let kinds = items
                .iter()
                .map(|i| i["kind"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(kinds, ["user", "tool", "assistant"]);
            assert_eq!(
                (
                    &items[1]["tool"],
                    &items[1]["status"],
                    &items[1]["arguments"]
                ),
                (
                    &json!("file_read"),
                    &json!("completed"),
                    &json!({"path": "a.txt"})
                )
            );
            assert!(
                items[1]["output"].as_str().unwrap().contains("content"),
                "{items:?}"
            );
            let completed = frames
                .iter()
                .find(|f| f["event"]["type"] == "tool.completed")
                .unwrap();
            assert!(
                completed["event"]["output"]
                    .as_str()
                    .unwrap()
                    .contains("content"),
                "{completed}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn writes_need_an_exact_unexpired_approval() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let replies = vec![
                runtime::assistant("", vec![call("w1", "file_write", json!({"path": "new.txt", "text": "hi", "expected_sha256": null}))]),
                runtime::assistant("ok", vec![]),
            ];
            let (fixture, _) = start(json!({}), replies).await;
            let id = session(&fixture).await;
            let socket = fixture.socket.clone();
            let turn = tokio::task::spawn_local({
                let id = id.clone();
                async move { request(&socket, json!({"method": "turn.start", "session_id": id, "prompt": "write"})).await }
            });
            let pending = approval_requested(&fixture, &id).await;
            let wrong = request(&fixture.socket, json!({"method": "approval.resolve", "session_id": id, "request_id": pending["request_id"], "binding": "0".repeat(64), "allow": true})).await;
            assert_eq!(wrong[0]["ok"], false);
            assert!(!fixture.workspace.path().join("new.txt").exists());
            let ok = request(&fixture.socket, json!({"method": "approval.resolve", "session_id": id, "request_id": pending["request_id"], "binding": pending["binding"], "allow": true})).await;
            assert_eq!(ok[0]["result"]["resolved"], true);
            let frames = turn.await.unwrap();
            assert_eq!(frames.last().unwrap()["turn_finished"], true);
            assert_eq!(std::fs::read_to_string(fixture.workspace.path().join("new.txt")).unwrap(), "hi");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_answers_the_open_tool_batch_so_the_session_resumes() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let replies = vec![runtime::assistant(
                "",
                vec![call(
                    "w1",
                    "file_write",
                    json!({"path": "x", "text": "y", "expected_sha256": null}),
                )],
            )];
            let (fixture, _) = start(json!({}), replies).await;
            let id = session(&fixture).await;
            let socket = fixture.socket.clone();
            let turn = tokio::task::spawn_local({
                let id = id.clone();
                async move {
                    request(
                        &socket,
                        json!({"method": "turn.start", "session_id": id, "prompt": "write"}),
                    )
                    .await
                }
            });
            approval_requested(&fixture, &id).await;
            request(
                &fixture.socket,
                json!({"method": "turn.cancel", "session_id": id}),
            )
            .await;
            let frames = turn.await.unwrap();
            assert_eq!(frames.last().unwrap()["status"], "cancelled");
            assert!(
                frames.iter().any(|f| f["event"]["type"] == "turn.completed"
                    && f["event"]["status"] == "cancelled")
            );
            assert_eq!(
                request(
                    &fixture.socket,
                    json!({"method": "session.close", "session_id": id})
                )
                .await[0]["result"]["closed"],
                true
            );
            let resumed = request(
                &fixture.socket,
                json!({"method": "session.resume", "session_id": id}),
            )
            .await;
            assert_eq!(
                (
                    resumed[0]["result"]["messages"].as_u64(),
                    resumed[0]["result"]["discarded_incomplete_messages"].as_u64()
                ),
                (Some(3), Some(0)),
                "{resumed:?}"
            );
            let saved = request(
                &fixture.socket,
                json!({"method": "session.export", "session_id": id}),
            )
            .await;
            let messages = saved[0]["result"]["messages"].as_array().unwrap().clone();
            assert_eq!(
                (
                    messages[2]["role"].as_str(),
                    messages[2]["tool_call_id"].as_str()
                ),
                (Some("tool"), Some("w1"))
            );
            assert!(
                messages[2]["content"]
                    .as_str()
                    .unwrap()
                    .contains("tool-call-cancelled")
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn workspaces_outside_profile_roots_and_bad_frames_are_refused() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (fixture, _) = start(json!({}), vec![]).await;
            let outside = request(&fixture.socket, json!({"method": "session.create", "workspace": "/usr", "model": "m"})).await;
            assert_eq!(outside[0], json!({"ok": false, "error": "request-unavailable"}));
            let unknown = request(&fixture.socket, json!({"method": "turn.cancel", "session_id": "x"})).await;
            assert_eq!(unknown[0]["ok"], false);
            let stream = UnixStream::connect(&fixture.socket).await.unwrap();
            let (reader, mut writer) = stream.into_split();
            writer.write_all(b"{\"schema_version\":2,\"method\":\"session.list\"}\n").await.unwrap();
            let line = BufReader::new(reader).lines().next_line().await.unwrap().unwrap();
            assert!(line.contains("request-unavailable"));
            let state = private();
            let alias_root = private();
            let protected = fixture.workspace.path().join("private-data");
            std::fs::create_dir(&protected).unwrap();
            std::fs::write(protected.join("token"), "private fixture").unwrap();
            let alias = alias_root.path().join("config-link");
            std::os::unix::fs::symlink(&protected, &alias).unwrap();
            let (complete, _) = scripted(vec![]);
            let service = Service::new(json!({"providers":["local"], "profiles":{"default":{"workspace_roots":[fixture.workspace.path()]}}, "protected_paths":[alias]}), state.path(), Some(complete), &[]).unwrap();
            assert!(service.create(&json!({"workspace":protected,"model":"m"})).is_err());
            let admitted = service.create(&json!({"workspace":fixture.workspace.path(),"model":"m"})).unwrap();
            let session = service.sessions.borrow()[admitted["session_id"].as_str().unwrap()].clone();
            assert_eq!(session.borrow().workspace.read("private-data/token").unwrap_err().0, "protected service data is outside tool authority");

        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_changed_skill_has_its_own_error() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (fixture, _) = start(json!({}), vec![]).await;
            let id = session(&fixture).await;
            let skill = json!([{"name": "gone", "sha256": "0".repeat(64)}]);
            let turn = request(
                &fixture.socket,
                json!({"method": "turn.start", "session_id": id, "prompt": "hi", "skills": skill}),
            )
            .await;
            assert_eq!(turn.last().unwrap()["error"], "skill-changed", "{turn:?}");
            let load = request(
                &fixture.socket,
                json!({"method": "skills.load", "name": "gone", "sha256": "0".repeat(64)}),
            )
            .await;
            assert_eq!(load[0]["error"], "skill-changed");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn replies_stream_and_a_steer_gets_its_own_answer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let seen = Rc::new(RefCell::new(Vec::new()));
            let log = seen.clone();
            let complete: Complete = Rc::new(move |body, delta| {
                log.borrow_mut().push(body);
                let first = log.borrow().len() == 1;
                Box::pin(async move {
                    if !first {
                        return Ok(runtime::assistant("ok", vec![]));
                    }
                    delta("reasoning", "hm");
                    delta("content", "Hel");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    delta("content", "lo");
                    Ok(runtime::assistant("Hello", vec![]))
                })
            });
            let fixture = start_with(json!({}), complete).await;
            let id = session(&fixture).await;
            let turn = tokio::task::spawn_local({
                let (socket, id) = (fixture.socket.clone(), id.clone());
                async move {
                    request(
                        &socket,
                        json!({"method": "turn.start", "session_id": id, "prompt": "hi"}),
                    )
                    .await
                }
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            let steered = request(
                &fixture.socket,
                json!({"method": "turn.steer", "session_id": id, "text": "also this"}),
            )
            .await;
            assert_eq!(
                steered[0]["result"],
                json!({"steered": true}),
                "{steered:?}"
            );
            let frames = turn.await.unwrap();
            let types = frames
                .iter()
                .filter_map(|f| f["event"]["type"].as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                types,
                [
                    "turn.started",
                    "reasoning.delta",
                    "reasoning.completed",
                    "message.delta",
                    "message.delta",
                    "message.completed",
                    "steer.accepted",
                    "message.completed",
                    "turn.completed"
                ]
            );
            assert_eq!(seen.borrow()[0]["stream"], true);
            let messages = seen.borrow()[1]["messages"].as_array().unwrap().clone();
            assert_eq!(
                messages.last().unwrap(),
                &json!({"role": "user", "content": "also this"})
            );
            let idle = request(
                &fixture.socket,
                json!({"method": "turn.steer", "session_id": id, "text": "late"}),
            )
            .await;
            assert_eq!(idle[0]["ok"], false, "{idle:?}");
        })
        .await;
}

#[test]
fn home_workspace_refuses_credentials_and_login_hooks() {
    let home = tempfile::tempdir().unwrap();
    let xdg_config = tempfile::tempdir().unwrap();
    let active = xdg_config.path().to_path_buf();
    let protected = credential_paths(home.path(), |name| {
        (name == "XDG_CONFIG_HOME").then(|| active.clone().into_os_string())
    });
    for dir in [".config/systemd/user", ".local/state/enzimos-ai-gateway"] {
        std::fs::create_dir_all(home.path().join(dir)).unwrap();
    }
    std::fs::write(home.path().join(".bashrc"), "alias ls=ls").unwrap();
    std::fs::write(
        home.path()
            .join(".local/state/enzimos-ai-gateway/chatgpt-account.json"),
        "{}",
    )
    .unwrap();
    std::fs::write(home.path().join("notes.txt"), "plain").unwrap();
    let workspace = Workspace::new(home.path(), &protected).unwrap();
    for path in [
        ".bashrc",
        ".zshrc",
        ".profile",
        ".local/state/enzimos-ai-gateway/chatgpt-account.json",
        ".local/share/enzremote/identity.key",
        ".local/share/keyrings/login.keyring",
        ".config/enzremote/devices.json",
        ".config/autostart/agent.desktop",
        ".config/systemd/user/agent.service",
        ".config/environment.d/agent.conf",
    ] {
        assert_eq!(
            workspace.read(path).unwrap_err().0,
            "protected service data is outside tool authority",
            "{path}"
        );
        assert!(workspace.write(path, "x", None).is_err(), "{path}");
    }
    assert!(!home.path().join(".config/autostart").exists());
    assert_eq!(
        std::fs::read_to_string(home.path().join(".bashrc")).unwrap(),
        "alias ls=ls"
    );
    assert_eq!(workspace.read("notes.txt").unwrap()["text"], "plain");
    assert!(protected.contains(&xdg_config.path().join("autostart")));
}
