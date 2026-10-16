# Provider connections and coding backends

Agent 206 ships provider setup presets, not preauthorized accounts or model lists.
Open Connections for hosted, local and custom provider presets, including
DeepSeek, Moonshot/Kimi, Alibaba Cloud Model Studio, Z.ai, MiniMax, SiliconFlow,
StepFun, Tencent Hunyuan, Volcengine/Doubao and Baidu Qianfan. Review the protocol and base URL before saving. Refresh models to
fetch the API catalog. Prices, limits, effort choices and balances appear only when
reported by a supported API. A vault credential field holds a saved name, never an
API key. Authenticated dispatch is still disabled until the isolated encrypted
credential broker is connected.

The CLI lists the effective library, including user additions and overrides:

```sh
agent206 providers presets
agent206 providers add openrouter
# Joshua and custom connections require your actual endpoint URL.
agent206 settings > settings.toml
agent206 settings --apply settings.toml
```

## Customize the provider library

Connections → Provider library can add/edit presets, change their grouping, hide
individual entries, restore bundled versions, or opt out of bundled entries. A
custom entry with the same ID overrides that bundled preset; a new ID adds an
entry. Hidden IDs and user overrides persist across upgrades. These operations
change future connection setup only; existing account URLs, model allowlists and
credentials are not modified. A provider preset does not promise that a vendor
publishes a compatible model-list API. If no supported API catalog is available,
model selection remains unavailable; there is no static model fallback.

Export settings, edit the TOML, and apply it through the CLI. The running gateway
owns settings; editing its mirror file alone does not hot-reload it.

```toml
[provider_presets]
include_builtins = true
# Hide any bundled or custom ID without deleting existing accounts.
disabled = ["openai"]

[[provider_presets.entries]]
id = "my-provider"
name = "My provider"
group = "My services"
api = "open_ai"
# Blank means the user must enter an endpoint when creating an account.
base_url = ""
setup = "Enter your service's HTTPS or SSH-forwarded loopback API URL."
```

Use `anthropic` or `gemini` for those native protocols. No API keys, model names,
limits, prices or balances belong in preset entries. Region/workspace-specific
services require a URL from the vendor's console; endpoints and credentials from
different regions must not be treated as interchangeable.

Provider account configuration is separate from the agent engine. Agent 206 is the
default engine. Optional Codex and Claude Agent engines run as external processes;
they are not downloaded, updated or bundled by this version.

## Runtime configuration

Install a supported native Codex binary or the
[Claude Agent ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp)
and its Node/SDK dependencies in a dedicated code-only directory outside your
projects and private application state. Do not point the installation directory at
your home or a directory containing credentials. The supervisor grants read access
to this installation, standard system runtime libraries and its temporary home.
Other required libraries must be included in the selected installation directory.
For Claude, a standalone Node distribution and the adapter's npm installation can
share that directory. Native launchers/interpreters and scripts must all be inside
it. A Codex npm shell/JavaScript wrapper is not a native Codex binary.

Use Connections → Coding-agent backends to set the absolute executable and
installation directory, plus the JavaScript entry point for Claude Agent. The
CLI equivalents accept your real paths:

```sh
agent206 backend configure codex --executable "$CODEX_BINARY" --runtime-root "$CODEX_INSTALLATION"
agent206 backend configure claude-agent --executable "$NODE_BINARY" --runtime-root "$CLAUDE_INSTALLATION" --adapter-script "$CLAUDE_ACP_ENTRY"
```

Codex requires a compatible provider endpoint with the Responses API, which is
not implied by support for chat completions. Claude Agent requires the Anthropic
Messages protocol. Both use the exact model selected from the provider's live API.
Backend-bundled model menus/defaults are not used as Agent 206 catalog data and
cannot expand the proxy's model ceiling. Unsupported protocol/runtime versions
fail; there is no automatic backend or model fallback.

In the conversation inspector choose Agent engine. Project tools begin disabled.
For a project coordinator, select the appropriate mode and explicitly grant read,
edit or command access. Mode ceilings still apply; editing requires reading.
Delegated conversations cannot change backend grants, and external delegation is
currently disabled. CLI selection uses the same checks:

```sh
agent206 backend select "$THREAD_ID" codex
# Explicit project permissions, within the current mode's ceiling:
agent206 backend select "$PROJECT_THREAD_ID" codex --read --write --execute
agent206 backend select "$THREAD_ID" native
```

## Current boundaries and limits

The macOS implementation uses Seatbelt. Linux and Windows launch fails closed until
equivalent filesystem and network isolation is implemented. Processes start in a
scratch working directory with a new private home and cleared environment. They
cannot inherit existing agent accounts, hooks, plugins, project settings or MCP
servers. Granted project paths are included explicitly in the task. Common user
credential stores and private Agent 206 state remain denied, including through
symlinks. Runtime installation must not overlap projects or private application state.

An ephemeral loopback proxy accepts only selected inference paths and the current
model. Its per-run token is an opaque capability, not an upstream credential; it is
never forwarded upstream. Direct network access and permission expansion are
unavailable. The proxy does not unlock or access the vault. Request budgets cap
output at 8,192 tokens (or a lower API-reported output limit/requested budget), with
128 authorized requests per run. These are application resource budgets, not model
capacity facts. Responses are bounded at 16 MiB; protocol lines at 2 MiB and total
protocol output at 32 MiB; assistant text at 128 KiB. Runs stop after ten minutes.

Each turn uses a fresh external session and a transcript bounded at 2 MiB. Agent
206 persists the conversation, streamed text and bounded operation summaries.
External session resume/fork, interactive approvals and backend subagent policy
are further work. ACP context-used values are not misreported as request input
usage. Unavailable request token counts or costs stay unknown.

Stop closes the proxy, kills the owned process group (including descendants) and
reaps its root before recording cancellation. Changing/removing the active backend
or provider also cancels affected runs. No unrelated service is killed. Closing a
local connection cannot undo an inference request already accepted by a provider.

The Claude integration is labeled Claude Agent. Anthropic's
[SDK documentation](https://code.claude.com/docs/en/agent-sdk/overview) requires
third-party applications to use API-key authentication unless separately approved;
Agent 206 does not offer claude.ai subscription login. The Apache-2.0 adapter does
not license the Claude runtime; each component retains its own terms.

## Validation

Tests exercise generated model facts from running API servers, native provider
pagination and text mapping, and independent mock Codex/ACP peer executables inside
the real macOS sandbox. They verify actual file/network denials, scoped read/write,
symlink protection, permission denial, cross-session isolation, malformed/oversized
output, cancellation of root/descendant processes and configuration revocation.
They do not establish compatibility with installed vendor releases or authenticated
cloud accounts. Real vendor acceptance and the credential broker are required
before claiming production coding-agent support.
