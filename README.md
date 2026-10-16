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
# Primary desktop experience; first launch asks which gateway to use.
dx serve --platform desktop -p agent206

# Headless CLI and local gateway; no GUI system dependencies.
cargo build -p agent206 --no-default-features
./target/debug/agent206 gateway setup --local   # once per profile
./target/debug/agent206 new 'A new idea'
./target/debug/agent206 snapshot

# Same component tree in the Dioxus terminal renderer.
cargo run -p agent206 --no-default-features --features tui -- tui

# Remote gateway using your existing SSH identities and verified host keys:
# save it as the profile's gateway, or override per command with --remote.
./target/debug/agent206 gateway setup --ssh ssh://user@host
./target/debug/agent206 --remote ssh://user@host snapshot
```

The remote host needs an installed `agent206` executable. Automatic remote
installation/updating is not implemented. Setup shows an unknown host's key
fingerprints (after applying your ssh_config aliases) and trusts only one you
confirm; hosts behind a ProxyJump/ProxyCommand must first be verified with
`ssh` itself. Changed host keys are refused.

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

# Choose the gateway once per profile (the desktop and terminal clients ask on
# first launch). Unknown SSH hosts print fingerprints to verify, then --trust one.
./target/debug/agent206 gateway setup --local
./target/debug/agent206 gateway setup --ssh ssh://user@host --trust SHA256:...

# Lifecycle controls; status and reconnect never start a missing gateway.
./target/debug/agent206 status
./target/debug/agent206 gateway status
./target/debug/agent206 gateway start
./target/debug/agent206 gateway restart
./target/debug/agent206 gateway stop
./target/debug/agent206 gateway reconnect
```

The shared Gateway page (`/gateway`) has start/stop/restart/reconnect controls and
verified process/version/uptime and active-work details. Reach it from the header,
sidebar or the workspace's Manage gateway action. It is available on desktop,
web and terminal. Closing a client leaves the service running. A deliberate stop
persists across other clients' polling and new clients; use Start or Restart to
resume. A fresh profile starts automatically from the desktop or web bridge.
Windows named-pipe transport and real remote lifecycle acceptance are not
implemented yet. SSH controls target an already-installed remote executable.

Running is separate from security readiness. Encrypted gateway enrollment/TOTP
and reviewed imports are required next implementation gates; see
[security setup/parity](docs/security-setup.md) and [import contract](docs/imports.md).
The CLI is a supported headless interface alongside the GUI; see its
[RustyClaw compatibility matrix](docs/cli.md).

## Web client

Build release assets so Dioxus development overlays are not shipped to a static
server. The web bridge uses the same gateway commands and MessagePack frames.
Restart the web bridge after updating the native binary as well as the web assets;
an older running bridge cannot serve newly added control endpoints. Reopen its
authorization URL after restarting so the browser uses the new session.

```sh
rustup target add wasm32-unknown-unknown
dx build --platform web -p agent206 --release --no-default-features --features web
cargo build -p agent206 --no-default-features
./target/debug/agent206 gateway setup --local   # or --ssh; the bridge connects for the browser
./target/debug/agent206 web --static-dir target/dx/agent206/release/web/public
```

Open the one-session authorization URL printed by the server. It binds only to
127.0.0.1 and establishes an origin-scoped bearer session through a one-use link.
POST requests need the exact Host/Origin and explicit Authorization header.
The browser token lives in storage scoped to that exact origin, so other tabs of the same browser stay connected; it is removed from the address bar and expires when the bridge restarts. `http://localhost:<port>` redirects to the `127.0.0.1` origin. Do not reverse-proxy this preview onto
a public network: multi-user authentication, HTTPS deployment and remote web
sessions are not implemented.

Navigation uses Dioxus Router across renderers. Web pages have direct addresses
such as `/connections`, `/projects`, and `/settings/appearance`. Conversations use
`/conversations/<thread-uuid>`; append `?split=<second-thread-uuid>` for two panes.
Back/forward and reload preserve the route. These addresses select content in the
current gateway profile; they do not grant access or carry authorization. Open the
printed authorization link first in each new browser session.

## What works

- Shared fc-dioxus shell with typed Dioxus Router navigation, resizable navigation/chat/inspector panes, side-by-side
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
- A separate vault broker process holds the SecureStore-compatible vault, with
  RustyClaw's credential kinds and policies, TOTP two-factor and lockout. Provider
  API keys are attached by the broker only for origins you bind them to, so the
  gateway never reads them. Managed with `agent206 vault`; the desktop setup UI
  and agent approval flow are **not built yet**, so agents cannot use credentials.
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
cargo test -p agent206 --no-default-features --test smoke --locked
```

The Rust smoke tests launch Cargo's actual `agent206` executable and an independent
Rust Joshua mock. They also run with the workspace tests; CI checks the desktop
and headless binaries. Each test owns a private gateway and profile. Unix gateway
transport is required; Windows coverage awaits named-pipe support.

To exercise an existing packaged executable instead:

```sh
A206_TEST_BINARY="$PWD/dist/Agent206.app/Contents/MacOS/agent206" \
  cargo test -p agent206 --no-default-features --test smoke --locked
```

Read [AGENTS.md](AGENTS.md), [architecture](docs/architecture.md),
[plugin API](docs/plugin-api.md), [upstream evidence](docs/upstream.md), and
[status](docs/status.md) before extending the system.

MIT. RustyClaw reuse is attributed in [NOTICE](NOTICE).

Bundle icons are generated from the vector source with `scripts/build-icons.py`
(CairoSVG and Pillow; Cairo is required by CairoSVG). PNG/ICO/ICNS files are checked
in, so these tools are not needed to build the application.
