# Native harness protocol v1

The service uses a private0600 Unix socket with same-UID peer checks. One bounded
JSON request per connection ends with newline. Set `schema_version: 1` and `method`.
Replies contain `{ok: true, result: ...}` or `{ok: false, error: ...}`. A turn
connection stays open and streams `{event: ...}` followed by a `turn_finished`
reply; closing it cancels the turn. Requests/responses are bounded1MiB.

## Configuration

The config is an owned0600 JSON file in a private directory. No configuration is
automatically enabled. Minimal example (replace paths and model identity):

```json
{
  "gateway_socket": "/run/user/1000/enzimos-ai-gateway/service.sock",
  "providers": ["local"], "default_provider": "local",
  "profiles": {"default": {"workspace_roots": ["/home/admin/Projects"]}},
  "enzui": {"approval_default": "ask"},
  "skill_directories": ["/home/admin/.config/enzimos/skills"],
  "writable_skill_directories": ["/home/admin/.config/enzimos/skills"],
  "mcp_servers": {}
}
```

The selected workspace must be inside a root listed in the profile.
Commands require Bubblewrap plus
a systemd user scope; no fallback executes unsandboxed commands. Service state,
Center state/credentials, SSH/GPG directories, keyrings, AI gateway and Phone Link
state, configuration and runtime endpoints, and login hooks (shell startup files,
autostart entries, systemd user units, `environment.d`) are excluded from file
tools and command mounts even within an approved root.
Additional `protected_paths` may contain up to32 absolute paths.

## Sessions and turns

- `session.create`: workspace, model, optional provider, profile, approval_mode
  and screenshots.
  The local provider accepts an omitted or empty model. Automatic selection
  uses the inference default for chat and prefers the evaluated local tool
  model for workspace requests. An explicit model pins the session.
  Returns the session ID and active settings. At most 16 sessions can be open.
- `session.set_approval_mode`: session_id, approval_mode, and
  `confirm_full_send: true` for Full send. Only idle sessions can change mode.
  `ask` prompts for writes, commands, MCP and the first screenshot.
  `workspace_edits` also permits writes through the workspace file tool.
  `full_send` skips harness prompts. Sandbox and portal restrictions still apply.
  `enzui.approval_default` picks the default for new sessions. A Full send
  preference starts in Ask until explicitly confirmed through the live request.
  Full send resumes as Ask after closing the live session.
- `session.set_screenshots`: session_id and screenshots (`off`, `text` or
  `vision`). Screenshots are off by default. An opt-in is live-session only.
  Text mode sends dimensions and approximate colors. Vision also attaches a
  bounded image to the next model request. Captured pixels are excluded from
  the transcript. Each capture uses the desktop Screenshot portal.
  The bundled text models can request a text summary in Text mode.
  Automatic selection prefers LFM3-VL-2B for a screenshot requested in Vision
  mode, if installed.
  Image delivery requires an inference backend serving it with vision enabled.
  For other providers or custom models, choose the mode their backend supports.
- `session.list`: stored session summaries, each with `title` (first request),
  `preview` (last reply) and `updated` (Unix seconds). `session.resume`: session_id; checks
  current policy/provider/workspace and discards incomplete tool batches.
- `session.export`, `session.delete`: session_id. An export adds `title` and
  `items`: the transcript as a client shows it (`user`, `assistant` and `tool`
  records with tool, arguments, output and status). User items list attachment
  names. Send `items_only: true` to omit the raw messages. The export also
  returns `last_turn_revision` for the latest successful turn when available.
  `session.rename`: session_id and title (1–60 characters, single line).
  Rename and delete require an idle session. Stored transcripts are sensitive,
  including the bytes of any attached documents and images. Each transcript
  is bounded to 2 MiB, with at most 128 sessions.
- `session.close`: session_id; cancel a running turn first.
- `model.warmup`: optional session_id. Ask the local inference service to load
  the pinned model or the automatic text model for an idle chat. With no
  session, use the new-chat settings. The result reports `warmed`. Preloading
  may be skipped and never sends a prompt or contacts a remote provider.
- `turn.start`: session_id, prompt, and optional `skills: [{name, sha256}]` (max8).
  The optional `attachments` array accepts up to three entries, each with a
  `name` and either `text` (4 KiB total) or `jpeg` (base64, at most 128 KiB
  encoded and 768 pixels on either side). File loading and PDF text extraction
  happen in the client. The active turn passes images to a vision model.
  Later turns retain document text and image names as context.
  Supply the current approval_mode when Full send is active and the current
  screenshots setting when enabled. If supplied, both settings must match.
  Events include turn and tool lifecycle, approval requests and resolutions, and
  failures. `turn.started` carries the model ID. `turn.local_limit` signals a
  local context, output, or tool-loop limit, or a malformed or empty completion.
  Replies are streamed: `reasoning.delta` and `message.delta` carry `text` as it
  arrives, `reasoning.completed` carries `duration_ms`, and `message.completed`
  carries the whole `content`.
- `turn.revise`: session_id, expected_revision, optional prompt, and the same
  approval and screenshot settings as `turn.start`. Use the sequence of the
  successful `turn.completed` event or the export's `last_turn_revision`.
  The original prompt is reused when omitted. The last user message and its
  reply are replaced on success, preserving its attachments. A stopped or
  failed revision preserves the original transcript.
- `turn.steer`: session_id, text. Queues a user message into the running turn; the
  model reads it before its next step (`steer.accepted` with `text`), and a turn
  that was ending takes another step to answer it. At most8 queued; refused when
  no turn is running.
- `turn.cancel`: session_id. Disconnect also cancels pending approval/execution.
- `events.list`: session_id, after sequence; returns events, last_sequence, `gap`
  when the bounded512-event/2MiB ring no longer contains the requested history,
  and `more`: a page fits one frame, so ask again after its last sequence.
- An idle open session is closed when a 17th is created, and `session.resume`
  returns the current settings for an idle open session.

A cancelled, timed-out or failed turn answers its open tool calls with a
`tool-call-cancelled` result, so the transcript stays a valid conversation.
Stored transcripts drop the oldest whole turns at 640 KiB. Before requesting
one of the bundled local models, the harness trims old turns from the model's
view to fit a 10 KiB request budget for messages, instructions, and schemas.
Tool results have a 3 KiB limit for those models and a 96 KiB limit for other
models. Cut results carry a `truncated` marker.

An `approval.requested` event supplies request_id and binding plus the exact tool
and arguments. Use `approval.resolve` with session_id, request_id, binding and
boolean allow. With `remember: true` and `allow: true`, subsequent calls of that
tool may use any accepted arguments during the live session. A successful
screenshot also covers subsequent screenshots in that session. Changing modes
clears live grants. Wrong, stale and repeated bindings are rejected. Pending
approvals expire in 90 seconds. Clients must show the actual action before approving.
No automated permission acceptance is provided by the CLI.

Sessions are saved after turns. An abrupt crash can lose in-flight conversation;
no successful tool action is automatically replayed during resume.

## Skills

`skills.list` discovers summaries without executing code. `skills.load` takes name
and SHA256. When a selected skill changed or is gone, `skills.load` and
`turn.start` fail with the error `skill-changed` (before `turn.started`): list
skills again. Every other failure is `request-unavailable`. `skills.create` takes an explicitly enabled directory, name, description
and instructions. It creates a new private folder/SKILL.md and refuses overwrite.
Creation is a user control-plane operation and is never a model tool. Skills add
instructions to a turn, never file/command/network authority.

## MCP

Explicit servers use `{enabled:true, transport:"stdio", argv:[absolute executable,
args...], environment:{...}, selected_tools:[...]}` or `{enabled:true,
transport:"http", url:"https://...", authorization:"Bearer ..."}`. HTTP on literal
loopback is allowed for local services; verified HTTPS is otherwise required.
Secrets belong in the private config, never skill text/argv supplied by a model.
Stdio executables are trusted configuration and run with the user's authority,
not within the command sandbox. The conversation’s approval mode governs harness approval.

Center configuration uses `/usr/bin/enzui-mcp --credential /private/agent.json
--socket /run/user/1000/enzui/center.sock`. Enroll this identity explicitly through
Center; retain its four progressive discovery tools. Center app consent remains
a separate authority, and an MCP result cannot authorize a foreground Send.

No implicit OAuth, sampling, roots, elicitation, redirects or action replay. HTTP
supports JSON/SSE POST, session/protocol headers and DELETE/cancel. Automatic GET
stream resumption is not implemented; lost responses have unknown outcomes and
require explicit user review before retry.
