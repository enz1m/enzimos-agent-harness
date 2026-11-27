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
    let approval_default = profile.as_object_mut().unwrap().remove("approval_default");
    let config = json!({"providers": ["local"], "profiles": {"default": profile},
        "enzui": {"approval_default": approval_default}});
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
    session_with_model(fixture, "m").await
}

async fn session_with_model(fixture: &Fixture, model: &str) -> String {
    let created = request(
        &fixture.socket,
        json!({"method": "session.create", "workspace": fixture.workspace.path(), "model": model}),
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
                runtime::assistant("", vec![call("w1", "file_write", json!({"path": "new.txt", "text": "hi"}))]),
                runtime::assistant("ok", vec![]),
            ];
            let (fixture, seen) = start(json!({}), replies).await;
            let id = session_with_model(&fixture, "minicpm5-1b").await;
            let socket = fixture.socket.clone();
            let turn = tokio::task::spawn_local({
                let id = id.clone();
                async move { request(&socket, json!({"method": "turn.start", "session_id": id, "prompt": "write new.txt"})).await }
            });
            let pending = tokio::time::timeout(Duration::from_secs(3), approval_requested(&fixture, &id)).await.unwrap();
            assert_eq!(pending["arguments"], json!({"path": "new.txt", "text": "hi", "expected_sha256": null}));
            assert_eq!(seen.borrow()[0]["tools"].as_array().unwrap().len(), 3);
            let wrong = request(&fixture.socket, json!({"method": "approval.resolve", "session_id": id, "request_id": pending["request_id"], "binding": "0".repeat(64), "allow": true})).await;
            assert_eq!(wrong[0]["ok"], false);
            assert!(!fixture.workspace.path().join("new.txt").exists());
            let ok = request(&fixture.socket, json!({"method": "approval.resolve", "session_id": id, "request_id": pending["request_id"], "binding": pending["binding"], "allow": true})).await;
            assert_eq!(ok[0]["result"]["resolved"], true);
            let frames = turn.await.unwrap();
            assert_eq!(frames.last().unwrap()["turn_finished"], true);
            assert!(frames.iter().any(|frame| {
                let event = &frame["event"];
                event["type"] == "approval.resolved"
                    && event["request_id"] == pending["request_id"]
                    && event["allow"] == true
            }));
            assert_eq!(std::fs::read_to_string(fixture.workspace.path().join("new.txt")).unwrap(), "hi");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn broad_approvals_require_live_consent_and_keep_workspace_boundaries() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let write = |id: &str, path: &str| call(
                id, "file_write", json!({"path": path, "text": id})
            );
            let (fixture, _) = start(json!({"approval_default": "full_send", "allow_writes": true}), vec![
                runtime::assistant("", vec![write("w1", "with-consent"), write("w2", "with-grant")]),
                runtime::assistant("done", vec![]),
                runtime::assistant("", vec![write("w3", "auto-edit"), write("w4", "../outside")]),
                runtime::assistant("done", vec![]),
                runtime::assistant("", vec![write("w5", "../outside")]),
                runtime::assistant("done", vec![]),
            ]).await;
            let id = session_with_model(&fixture, "minicpm5-1b").await;
            let send = |body| request(&fixture.socket, body);
            for (confirmed, expected) in [(false, false), (true, true)] {
                let result = send(json!({"method": "session.set_approval_mode", "session_id": id,
                    "approval_mode": "full_send", "confirm_full_send": confirmed})).await;
                assert_eq!(result[0]["ok"], expected);
            }
            let result = send(json!({"method": "turn.start", "session_id": id, "prompt": "no mode acknowledgement"})).await;
            assert_eq!(result[0]["ok"], false);
            assert_eq!(send(json!({"method": "session.close", "session_id": id})).await[0]["ok"], true);
            let resumed = send(json!({"method": "session.resume", "session_id": id})).await;
            assert_eq!(resumed[0]["result"]["approval_mode"], "ask");

            let turn_request = |id: &str, mode| {
                let body = json!({"method":"turn.start", "session_id":id, "approval_mode": mode, "prompt":"write files in the workspace"});
                let socket = fixture.socket.clone();
                tokio::task::spawn_local(async move { request(&socket, body).await })
            };
            let turn = turn_request(&id, "ask");
            let pending = tokio::time::timeout(Duration::from_secs(3), approval_requested(&fixture, &id)).await.unwrap();
            let allow = send(json!({"method":"approval.resolve", "session_id":id,
                "request_id":pending["request_id"], "binding":pending["binding"], "allow":true, "remember":true})).await;
            assert_eq!(allow[0]["ok"], true);
            let frames = tokio::time::timeout(Duration::from_secs(3), turn).await.unwrap().unwrap();
            assert_eq!(frames.iter().filter(|f| f["event"]["type"] == "approval.requested").count(), 1);
            for (file, content) in [("with-consent", "w1"), ("with-grant", "w2")] {
                assert_eq!(std::fs::read_to_string(fixture.workspace.path().join(file)).unwrap(), content);
            }

            let edited = send(json!({"method":"session.create", "workspace":fixture.workspace.path(),
                "model":"minicpm5-1b", "approval_mode":"workspace_edits"})).await;
            let edited = edited[0]["result"]["session_id"].as_str().unwrap();
            let turn = turn_request(edited, "workspace_edits");
            let pending = tokio::time::timeout(Duration::from_secs(3), approval_requested(&fixture, edited)).await.unwrap();
            assert_eq!(pending["call_id"], "w4");
            assert_eq!(std::fs::read_to_string(fixture.workspace.path().join("auto-edit")).unwrap(), "w3");
            send(json!({"method":"approval.resolve", "session_id":edited,
                "request_id":pending["request_id"], "binding":pending["binding"], "allow":false})).await;
            assert_eq!(turn.await.unwrap().last().unwrap()["turn_finished"], true);

            assert_eq!(send(json!({"method": "session.set_approval_mode", "session_id": edited,
                "approval_mode": "full_send", "confirm_full_send": true})).await[0]["ok"], true);
            let frames = tokio::time::timeout(Duration::from_secs(3), turn_request(edited, "full_send")).await.unwrap().unwrap();
            assert!(!frames.iter().any(|f| f["event"]["type"] == "approval.requested"));
            assert!(frames.iter().any(|f| f["event"]["type"] == "tool.completed" && f["event"]["status"] == "failed"));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn screenshot_access_is_explicit_and_reviewed_in_the_current_session() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let shot = || runtime::assistant("", vec![call("shot", "screenshot", json!({"target": "screen"}))]);
            let (fixture, seen) = start(json!({}), vec![runtime::assistant("done", vec![]), shot(), runtime::assistant("done", vec![]), shot()]).await;
            let text_session = session_with_model(&fixture, "minicpm5-1b").await;
            let id = session_with_model(&fixture, "lfm3-vl-2b").await;
            let turn = |id: &str, mode, prompt| json!({"method":"turn.start", "session_id":id, "screenshots":mode, "prompt":prompt});
            let enabled = request(&fixture.socket, json!({"method":"session.set_screenshots", "session_id":text_session, "screenshots":"vision"})).await;
            assert_eq!(enabled[0]["ok"], true);
            let frames = request(&fixture.socket, turn(&text_session, "vision", "List the workspace")).await;
            assert_eq!(frames.last().unwrap()["turn_finished"], true);
            let advertised = |index: usize| seen.borrow()[index]["tools"].as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "screenshot"));
            assert!(!advertised(0));
            let frames = request(&fixture.socket, turn(&id, "off", "look")).await;
            assert_eq!(frames.last().unwrap()["turn_finished"], true);
            assert!(!advertised(1));

            let opt_in = request(&fixture.socket, json!({"method":"session.set_screenshots", "session_id":id, "screenshots":"vision"})).await;
            assert_eq!(opt_in[0]["result"]["screenshots"], "vision");
            let socket = fixture.socket.clone();
            let turn = turn(&id, "vision", "take a screenshot");
            let live = tokio::task::spawn_local(async move { request(&socket, turn).await });
            let pending = tokio::time::timeout(Duration::from_secs(3), approval_requested(&fixture, &id)).await.unwrap();
            assert_eq!(pending["tool"], "screenshot");
            assert!(advertised(3));
            let revoke = request(&fixture.socket, json!({"method":"session.set_screenshots", "session_id":id, "screenshots":"off"})).await;
            assert_eq!(revoke[0]["ok"], false);
            request(&fixture.socket, json!({"method":"turn.cancel", "session_id":id})).await;
            assert_eq!(live.await.unwrap().last().unwrap()["status"], "cancelled");
            let revoke = request(&fixture.socket, json!({"method":"session.set_screenshots", "session_id":id, "screenshots":"off"})).await;
            assert_eq!(revoke[0]["result"]["screenshots"], "off");

            let late = request(&fixture.socket, json!({"method":"approval.resolve", "session_id":id,
                "request_id":pending["request_id"], "binding":pending["binding"], "allow":true, "remember":true})).await;
            assert_eq!(late[0]["ok"], false);
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

#[tokio::test(flavor = "current_thread")]
async fn full_send_does_not_let_an_unverified_peer_attach_desktop_control() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (fixture, seen) = start(json!({}), vec![runtime::assistant("done", vec![])]).await;
            let id = session(&fixture).await;
            let mode = request(
                &fixture.socket,
                json!({"method":"session.set_approval_mode", "session_id":id,
            "approval_mode":"full_send", "confirm_full_send":true}),
            )
            .await;
            assert_eq!(mode[0]["result"]["approval_mode"], "full_send");
            let attachment = request(
                &fixture.socket,
                json!({"method":"session.desktop_control", "session_id":id,
            "generation":"0".repeat(32)}),
            )
            .await;
            assert_eq!(attachment[0]["ok"], false);
            let turned = request(
                &fixture.socket,
                json!({"method":"turn.start", "session_id":id,
            "approval_mode":"full_send", "prompt":"Click"}),
            )
            .await;
            assert_eq!(turned.last().unwrap()["turn_finished"], true);
            assert!(
                !seen.borrow()[0]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| {
                        tool["function"]["name"]
                            .as_str()
                            .is_some_and(|name| name.starts_with("computer_"))
                    })
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn turn_failures_expose_only_actionable_provider_errors() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for (error, expected) in [
                (crate::provider::GATEWAY_UNAVAILABLE, "gateway-unavailable"),
                (crate::provider::PROVIDER_TIMEOUT, "provider-timeout"),
                ("private upstream detail", "request-unavailable"),
            ] {
                let complete: Complete = Rc::new(move |_, _| Box::pin(async move { fail(error) }));
                let fixture = start_with(json!({}), complete).await;
                let id = session(&fixture).await;
                let frames = request(
                    &fixture.socket,
                    json!({"method": "turn.start", "session_id": id, "prompt": "hello"}),
                )
                .await;
                assert_eq!(frames.last(), Some(&json!({"ok": false, "error": expected})));
                assert!(frames.iter().any(|frame| {
                    frame["event"]["type"] == "turn.completed"
                        && frame["event"]["status"] == "failed"
                }));
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn revising_keeps_attachments_and_preserves_the_saved_reply_on_failure() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let seen = Rc::new(RefCell::new(Vec::new()));
            let complete: Complete = {
                let seen = seen.clone();
                Rc::new(move |body, _| {
                    let mut seen = seen.borrow_mut();
                    seen.push(body);
                    let result = match seen.len() {
                        1 => Ok(runtime::assistant("first answer", vec![])),
                        2 => fail(crate::provider::PROVIDER_TIMEOUT),
                        _ => Ok(runtime::assistant("revised answer", vec![])),
                    };
                    Box::pin(async move { result })
                })
            };
            let fixture = start_with(json!({}), complete).await;
            let id = session(&fixture).await;
            let start = request(&fixture.socket, json!({
                "method": "turn.start", "session_id": id, "prompt": "Show the room",
                "attachments": [{"name": "agenda.txt", "text": "Room K42"}]
            })).await;
            let revision = start.iter().find(|f| f["event"]["type"] == "turn.completed").unwrap()["event"]["sequence"].clone();
            let revise = |expected: Value| json!({
                "method": "turn.revise", "session_id": id, "prompt": "Give the room in one word",
                "expected_revision": expected
            });
            let export = json!({"method": "session.export", "session_id": id, "items_only": true});
            let stale = request(&fixture.socket, revise(json!(0))).await;
            assert_eq!(stale[0]["error"], "stale-turn-revision");
            let failed = request(&fixture.socket, revise(revision.clone())).await;
            assert_eq!(failed.last().unwrap()["error"], crate::provider::PROVIDER_TIMEOUT);
            let saved = request(&fixture.socket, export.clone()).await;
            assert_eq!(saved[0]["result"]["items"][1]["text"], "first answer");
            assert_eq!(saved[0]["result"]["last_turn_revision"], revision);

            let revised = request(&fixture.socket, revise(revision.clone())).await;
            assert_eq!(revised.last().unwrap()["ok"], true);
            let saved = request(&fixture.socket, export).await;
            let items = saved[0]["result"]["items"].as_array().unwrap();
            assert_eq!(items.len(), 2);
            assert_eq!(items[0]["text"], "Give the room in one word");
            assert_eq!(items[0]["attachments"], json!(["agenda.txt"]));
            assert_eq!(items[1]["text"], "revised answer");
            assert_ne!(saved[0]["result"]["last_turn_revision"], revision);
            let seen = seen.borrow();
            let messages = seen[2]["messages"].as_array().unwrap();
            let users: Vec<_> = messages.iter().filter(|message| message["role"] == "user").collect();
            assert_eq!(users.len(), 1);
            assert!(users[0]["content"].as_str().unwrap().contains("Room K42"));
            assert!(users[0].get("attachments").is_none());
        })
        .await;
}
