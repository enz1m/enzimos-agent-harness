# enzimOS Agent Harness

Agent Harness runs the conversation and tool loop for the enzimOS desktop
assistant. It keeps track of each session, streams replies and tool activity to
the desktop shell, and pauses for approval when an action needs it.

The shell presents the conversation and approval prompts. AI Gateway routes
requests to the configured provider, and Inference Service runs local models.
The harness uses the provider selected for the session. [AI Gateway](https://github.com/enz1m/enzimos-ai-gateway) · [Inference Service](https://github.com/enz1m/enzimos-inference-service)

## Run the service

Use the example configuration to choose a gateway socket, a provider and the
workspace directories the assistant may use. Keep the configuration file mode
`0600` and its directory private. [Protocol guide](docs/PROTOCOL.md)

For a manual run, create private runtime and state directories and pass their
paths to the service.

```sh
enzimos-agent-harness --config /private/harness.json \
  --socket /private-runtime/harness.sock --state /private-state
```

In another terminal, the JSON client can list saved sessions. It accepts
requests on stdin and streams events as JSON lines.

```sh
printf '%s\n' '{"method":"session.list"}' |
  enzimos-agent-harness-client --socket /private-runtime/harness.sock
```

On enzimOS, the user unit in [deploy/](deploy/) reads
`$XDG_CONFIG_HOME/enzimos/agent-harness.json` and stores transcripts under
`$XDG_STATE_HOME/enzimos-agent-harness`. After configuring an installed unit,
start it with `systemctl --user start enzimos-agent-harness.service`.

## Workspaces, approvals and history

The harness can read and list workspace files, propose file replacements, and
run commands in a Bubblewrap sandbox with a systemd user scope for memory, CPU
and process limits. Commands require both. The sandbox excludes the host home,
credentials and graphical session bus. Ask every time binds each prompted
approval to one exact action. You can allow a tool for the live session,
auto-approve workspace file edits or explicitly confirm Full send. The same
workspace and sandbox restrictions apply in every mode. Cancelling or disconnecting ends the turn and its tool processes.

Before replacing a file, the harness checks its identity and contents again.
It retains the original file as a `.agent-backup-*` entry with its existing
permissions and returns the backup path. New-file creation leaves any
concurrent creator's file intact. Keep backups until you have reviewed them
and any editor using the previous file has closed it. An outside editor can
still save during a replacement. The backup retains the displaced file.

You can resume, export or delete saved sessions. The harness keeps up to 128
sessions and checks current workspace and provider permissions before resuming.
Pending approvals expire on restart. Transcripts contain prompts and tool
results, so treat them as private. The socket and files are accessible to local
processes running as your user.

## Screenshots

Enable screenshots for a live conversation to expose the screenshot tool.
The first capture requires approval unless Full send is active. The desktop
Screenshot portal separately decides whether it can capture the request.
Choose text summaries or enable image delivery for a vision-capable backend.
Among the bundled local profiles, LFM3-VL-2B exposes the screenshot tool and
becomes the first choice for automatic sessions with screenshots enabled.
Images go to the conversation’s provider and stay out of the saved transcript.

## Skills and MCP

Skills add instructions to a turn. The harness checks the selected skill
against its SHA-256 before the turn starts. The session keeps the same tool
permissions after loading a skill.

Enable MCP servers explicitly in the private configuration. The harness
supports stdio and Streamable HTTP over verified HTTPS or literal loopback
HTTP. Stdio executables run as your user, outside the command sandbox. External
tool calls follow the conversation’s approval mode. When using MCP Center, its enrollment and
app consent rules also apply. See the [protocol guide](docs/PROTOCOL.md) for
configuration, tool discovery limits, cancellation and handling lost responses.

## Build and test

Requires Linux and Rust 1.95 or newer. The namespace tests use Bubblewrap.
Build the standalone MCP fixture before running the suite.

```sh
cargo build --release --locked
cargo build --example fake_mcp --locked
cargo test --locked
cargo clippy --all-targets --locked
cargo fmt --check
```

The tests use private sockets, a scripted provider and local MCP fixtures.
Use a disposable system for installed service activation, cgroup limits and
desktop integration checks.

## License

[MIT](LICENSE)
