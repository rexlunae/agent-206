# Agent 206

**Your work. Your agents.**

A new Rust workspace for the successor to RustyClaw: desktop-first control, a
shared fc-dioxus UI, SSH transport, explicit authority, and bounded WASM extensions.
This is a working development foundation, **not yet a feature-complete or
security-equivalent RustyClaw replacement**. See the [implementation ledger](docs/status.md).

![Agent 206 mark](crates/ui/assets/agent206.svg)

## Run

Use stable Rust (the repository's toolchain file selects it) and Dioxus CLI 0.7.9.
On macOS the desktop build uses the system webview. Linux desktop additionally
needs Dioxus's WebKitGTK development dependencies.

```sh
# Primary desktop experience, with the gateway started automatically.
dx serve --platform desktop -p agent206

# Headless CLI and local gateway; no GUI system dependencies.
cargo build -p agent206 --no-default-features
./target/debug/agent206 new 'A new idea'
./target/debug/agent206 snapshot

# Same component tree in the Dioxus terminal renderer.
cargo run -p agent206 --no-default-features --features tui -- tui

# Remote gateway using your existing SSH identities and verified known_hosts.
./target/debug/agent206 --remote ssh://user@host snapshot
```

The remote host needs an installed `agent206` executable. Automatic remote
installation/updating is not implemented. Unknown SSH host keys are refused;
verify them using your ordinary SSH workflow first.

State is stored under `~/.agent206`; pass `--data-dir /absolute/path` to use an
isolated profile. Settings are exported as TOML; apply edits through the shared
API using `agent206 settings --apply settings.toml`. The gateway owns the database.
Don't edit `state.sqlite3` while it is running.

```sh
# Export account configuration without secrets or static model facts.
./target/debug/agent206 settings > settings.toml

# ACP and MCP adapters bind to a user-selected existing thread.
./target/debug/agent206 acp THREAD_UUID
./target/debug/agent206 mcp THREAD_UUID
# Optional, explicit permission for this session to rename its bound thread.
./target/debug/agent206 mcp THREAD_UUID --allow-title-edit

# Stop the shared gateway (interrupts active work).
./target/debug/agent206 stop-gateway
```

The desktop's Security page has stop/start/reconnect controls. Closing a client
leaves the shared gateway available to other clients. A subsequent client starts
it when needed. Windows named-pipe transport is not implemented yet.

## Web client

Build release assets so Dioxus development overlays are not shipped to a static
server. The web bridge uses the same gateway commands and MessagePack frames.

```sh
rustup target add wasm32-unknown-unknown
dx build --platform web -p agent206 --release --no-default-features --features web
cargo build -p agent206 --no-default-features
./target/debug/agent206 web --static-dir target/dx/agent206/release/web/public
```

Open the one-session authorization URL printed by the server. It binds only to
127.0.0.1 and establishes an origin-scoped bearer session through a one-use link.
POST requests need the exact Host/Origin and explicit Authorization header.
The browser token lives in sessionStorage and is removed from the address bar. Do not reverse-proxy this preview onto
a public network: multi-user authentication, HTTPS deployment and remote web
sessions are not implemented.

## What works

- Shared fc-dioxus shell, resizable navigation/chat/inspector panes, side-by-side
  conversations, new branding and future-css theme selection.
- Persisted conversations and projects, editable mode prompts, API-only model
  discovery, explicit per-thread subagent model allowlists and delegated threads.
- A configurable, grouped provider library covering hosted and local
  services; TOML additions/overrides and shared GUI editing; native Anthropic/Gemini
  and compatible text chat,
  bounded model pagination, cancellation, reported token usage/cost when present,
  background catalog refresh and honest unknown values.
- Single local gateway with private socket, SSH stdio bridge, bounded binary frames,
  durable command receipts and ordered event replay.
- Monitored HTTPS downloads with cancellation and optional SHA-256 verification;
  destinations include the transfer library and `~/models`.
- Owned local/SSH Joshua process start/stop, configurable API port, and API discovery.
- UTF-8 document previews, scoped Git status and a sandboxed browser preview.
- A compiled bundled WASM extension in a separate limited worker process.
- SecureStore-compatible vault library and interactive vault CLI. The complete
  credential broker/approval flow is **not connected**; credentialed providers are
  rejected instead of sending secrets through the chat runtime.
- Official ACP v1 session/prompt/cancel adapter and scoped MCP harness tools.
- Optional Codex app-server and Claude Agent ACP backends with macOS isolation,
  per-thread project grants, restricted model proxy, streamed text and cancellation.
  Runtimes must be installed separately; authenticated dispatch and external
  delegation remain disabled;
  see [backend setup and limits](docs/coding-backends.md).

Unknown model limits, costs and account balances stay unknown. The application
ships provider connector identities and setup defaults, with no activated accounts
or static model catalog. Model facts in tests come from running APIs.

## Development

```sh
cargo fmt --all -- --check
cargo test --workspace --no-default-features
cargo clippy --workspace --all-targets --no-default-features -- -D warnings
cargo check -p agent206 --features tui
cargo check -p agent206 --no-default-features --features web --target wasm32-unknown-unknown
cargo build -p agent206 --no-default-features
python3 scripts/protocol-smoke.py target/debug/agent206
python3 scripts/joshua-smoke.py target/debug/agent206
```

Read [AGENTS.md](AGENTS.md), [architecture](docs/architecture.md),
[plugin API](docs/plugin-api.md), [upstream evidence](docs/upstream.md), and
[status](docs/status.md) before extending the system.

MIT. RustyClaw reuse is attributed in [NOTICE](NOTICE).

Bundle icons are generated from the vector source with `scripts/build-icons.py`
(CairoSVG and Pillow; Cairo is required by CairoSVG). PNG/ICO/ICNS files are checked
in, so these tools are not needed to build the application.
