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
  "profiles": {"default": {"workspace_roots": ["/home/admin/Projects"],
    "allow_writes": false, "allow_commands": false, "allow_network": false}},
  "skill_directories": ["/home/admin/.config/enzimos/skills"],
  "writable_skill_directories": ["/home/admin/.config/enzimos/skills"],
  "mcp_servers": {}
}
```

A profile grants only its configured roots/actions. Requests cannot expand it.
False action defaults require an exact user approval for each action, rather than
prohibiting the user from approving one action. Commands require Bubblewrap plus
a systemd user scope; no fallback executes unsandboxed commands. Service state,
Center state/credentials, SSH/GPG directories, configuration and runtime endpoints
are excluded from file tools and command mounts even within an approved root.
Additional `protected_paths` may contain up to32 absolute paths.

## Sessions and turns

- `session.create`: workspace, model, optional provider/profile. Returns session_id
  and immutable session policy. At most16 open sessions.
- `session.list`: stored session summaries, each with `title` (first request),
  `preview` (last reply) and `updated` (Unix seconds). `session.resume`: session_id; checks
  current policy/provider/workspace and discards incomplete tool batches.
- `session.export`, `session.delete`: session_id. An export adds `title` and
  `items`: the transcript as a client shows it (`user`, `assistant` and `tool`
  records with tool, arguments, output and status). Delete requires a closed session.
  Stored transcripts are sensitive, bounded2MiB each, at most128 sessions.
- `session.close`: session_id; cancel a running turn first.
- `turn.start`: session_id, prompt, optional `skills: [{name, sha256}]` (max8).
  Events include turn/tool started/finished, approval requested/resolved and failure.
  Replies are streamed: `reasoning.delta` and `message.delta` carry `text` as it
  arrives, `reasoning.completed` carries `duration_ms`, and `message.completed`
  carries the whole `content`.
- `turn.steer`: session_id, text. Queues a user message into the running turn; the
  model reads it before its next step (`steer.accepted` with `text`), and a turn
  that was ending takes another step to answer it. At most8 queued; refused when
  no turn is running.
- `turn.cancel`: session_id. Disconnect also cancels pending approval/execution.
- `events.list`: session_id, after sequence; returns events, last_sequence, `gap`
  when the bounded512-event/2MiB ring no longer contains the requested history,
  and `more`: a page fits one frame, so ask again after its last sequence.
- An idle open session is closed when a 17th is created, and `session.resume`
  takes over an idle open session (a client that crashed never closed it).

A cancelled, timed-out or failed turn answers its open tool calls with a
`tool-call-cancelled` result, so the transcript stays a valid conversation.
Transcripts are kept within640KiB of context by dropping the oldest whole
turns; one tool result is cut at96KiB with a `truncated` marker.

An `approval.requested` event supplies request_id and binding plus the exact tool
and arguments. Use `approval.resolve` with session_id, request_id, binding and
boolean allow. Wrong/stale/repeated bindings are rejected; approvals expire90s and
never survive restart. Clients must show the actual action before approving.
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
not within the command sandbox. Every MCP call also requires harness approval.

Center configuration uses `/usr/bin/enzui-mcp --credential /private/agent.json
--socket /run/user/1000/enzui/center.sock`. Enroll this identity explicitly through
Center; retain its four progressive discovery tools. Center app consent remains
a separate authority, and an MCP result cannot authorize a foreground Send.

No implicit OAuth, sampling, roots, elicitation, redirects or action replay. HTTP
supports JSON/SSE POST, session/protocol headers and DELETE/cancel. Automatic GET
stream resumption is not implemented; lost responses have unknown outcomes and
require explicit user review before retry.
