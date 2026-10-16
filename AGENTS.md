# Agent 206 contributor contract

Agent 206 is a fresh Rust successor to RustyClaw. Preserve its security boundaries,
encrypted vault compatibility and SSH transport; reuse well-isolated code with MIT
attribution. Prefer a small replacement over carrying historical architecture.

## Architecture and security

- Use idiomatic, DRY Rust, typed errors and small crates. No application `unsafe`.
- Desktop is the primary control surface. Desktop, web and dioxus_tui share the
  same fc-dioxus components, state and command API. CLI commands use that API too.
- The gateway owns orchestration and policy, never the presentation layer. Keep
  vault, gateway and untrusted extension execution in separate process boundaries.
  An in-process sandbox is defense in depth, not a replacement for OS isolation.
- Deny by default. Effective subagent privileges are the intersection of user
  grants, mode, thread, parent, extension manifest and task scope. An agent may
  request permission but cannot approve itself, change grants or unlock the vault.
- Secret values never enter model context, plugin memory, ordinary events, DOM
  attributes or logs. Use opaque handles and brokered, destination-bound operations.
  Preserve encrypted storage and RustyClaw's approval/TOTP and credential policies.
  Never silently downgrade protections when a platform lacks an isolation facility.
- Browser pages are hostile. Separate profiles/origins; vault injection is approved
  for an exact origin and use, and cannot expose values through the agent API.
- SSH uses host verification, argv-safe invocation, keepalives and timeouts.
  Binary channels need bounded frames, protocol negotiation, durable event cursors,
  deduplication and explicit resynchronization when replay is unavailable. TCP/SSH
  already retransmits packets; do not invent a second unreliable packet transport.

## Data and interoperability

- Human-authored configuration, prompts, plugin manifests and themes use TOML.
- APIs we own use MessagePack with versioned schemas. JSON is confined to external
  standards (provider APIs, ACP, MCP and legacy SecureStore compatibility).
- Use Agent Client Protocol and the official SDK. MCP harness tools expose only
  safe, scoped operations. Neither protocol may bypass policy or vault isolation.
- Bundle well-known provider connectors and setup presets: provider identity,
  protocol, official endpoint defaults and authentication instructions are allowed.
  Presets do not enable an account or grant access without user configuration.
  Maintain broad global, Chinese and local coverage. Users can extend, override,
  group and hide presets through TOML and shared GUI controls; preserve those
  choices across upgrades without changing existing accounts.
- **No hardcoded model names/catalogs, limits, capabilities, prices or balances,
  including tests and examples.** Obtain these facts exclusively from APIs.
  Missing values are unknown, never zero or fabricated. Track source, retrieval
  time and expiry. Tests generate identifiers and response data at runtime.
- Each thread controls the provider/model set permitted for its subagents. Changes
  to API catalogs must not silently expand that set or fall back to another model.
- Downloads have one observable lifecycle, whether requested by a user or agent.
  Verify integrity, handle interruption, and publish events only after state changes.

## Extensions and integrations

- Most tools belong in WASM extensions. Builtins ship as compiled WASM with the
  application and follow the same grants as user/agent-created extensions.
- Version and document the plugin ABI, UI contributions, host capabilities and
  event contracts in docs/plugin-api.md. Validate manifests before installation.
  Fuel, memory, output, execution time and host I/O are bounded. No ambient WASI.
- Plugins contribute typed UI descriptions rendered by trusted fc-dioxus code;
  never inject arbitrary HTML, JavaScript or CSS into privileged app chrome.
- Agents can prepare extensions; enabling them or expanding permissions is a user
  operation. Protect the approval surface from extension impersonation.
- Git/PR/CI integrations use a forge-neutral model and adapters. Do not make the
  project architecture depend on one CI provider. Document format viewers are
  extensions where practical; evaluate ArtCraft and standard crates before inventing.
- Joshua is managed locally or over ordinary SSH, models live under ~/models,
  selection comes from APIs, and process ownership is explicit. Do not kill or
  update user-managed services. Verify downloaded releases before installation.
- Keep model providers separate from coding-agent backends. Support optional
  Codex and Claude Agent backends through documented interfaces; prefer ACP for
  agent interoperability. External runtimes remain isolated processes, with
  gateway-enforced grants, scoped MCP, cancellation and brokered credential use.
  Never treat their internal approval logic as our security boundary, inherit
  ambient hooks/plugins/credentials, or silently widen delegated model policy.

## UI and toolkit

- Prefer fc-dioxus components to raw HTML/basic Dioxus controls. Use its chat,
  context, tool trace, approval, transfer, git, document, progress, navigation and
  theme-picker components. Columns use accessible resizable splitters.
- Build a beautiful, information-rich workspace with concurrent chats, project
  supervision, visible privilege scopes, editable mode prompts, context usage and
  API-sourced cost/balance information. Show unknown/error/stale states honestly.
- Use future-css tokens, logical CSS properties, keyboard controls, reduced motion
  and accessible names. Theme selection uses the future-css ThemePicker.
- Chat composers stay compact: one growing text field with borderless, keyboard-
  accessible controls inside its bottom toolbar. Include attachment staging,
  model/mode/advertised effort selection, context used/limit and API cost/balance
  widgets; details expand in popovers. Never fabricate an unsupported control.
- Use theme-owned feature/raised/subtle surfaces, accent/status badges and icons
  to express hierarchy. Honor light/dark/high-contrast settings; avoid flat black
  chrome, hardcoded colors and references to nonexistent design tokens.
- Generic toolkit improvements belong upstream. Read fc-dioxus and future-css
  contributor rules before changing them. Never modify generated toolkit assets.
- Keep product UI focused on useful actions; do not present implementation details
  or nonfunctional buttons as finished features. No fake activity or invented stats.

## Validation and delivery

- Check popular, maintained Rust crates and primary documentation before choosing
  major dependencies. Pin git dependencies and keep Cargo.lock committed.
- Run cargo fmt --all -- --check, cargo test --workspace and relevant renderer
  builds. Test policy denials, malformed frames, replay, vault failures and sandbox
  limits. Test behavior, not an implementation's own constants.
- Keep docs/status.md accurate: distinguish implemented, tested, experimental and
  planned. Never call a scaffold a complete secure product. No bypass to make a
  check pass, and no claim of end-to-end support based solely on compilation.
- Do not overwrite unrelated local changes or deploy/publish without task authority.
