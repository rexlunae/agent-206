# Native run engine and tool hosts

The gateway runs the tool loop for native providers
(`crates/runtime/src/gateway/run.rs`). External coding backends (Codex, Claude
Agent) run their own loops and are not covered here.

## One run

A `Send` on a native conversation runs rounds until the model answers without
tool calls, a budget is used up, or the user cancels:

1. List every tool host's tools for the conversation's scope and offer the model
   only those whose effective permission is not Deny.
2. Stream the response. Text deltas are journaled as before; tool calls are
   assembled from OpenAI-compatible `tool_calls` deltas, Anthropic `tool_use`
   blocks with `input_json_delta`, or Gemini `functionCall` parts (whose
   `thoughtSignature` is stored and sent back). An OpenAI stream is complete only
   with both a finish reason (including `tool_calls`) and `[DONE]`.
3. Record the calls on the assistant message. Argument text is parsed; a code fence
   or surrounding prose and trailing commas are repaired (`healed`), truncated or
   non-object arguments fail that call, and an identical call later in the same
   response is `Skipped`. Missing or repeated provider call IDs are replaced.
4. Decide each call in order against the conversation as it is now (a permission
   revoked mid-run applies to the next call), then run it, ask first, or refuse it.
5. Send every call's result text back in the protocol's own shape and repeat.

Every call ends with model-facing result text and, unless it succeeded, a
structured `ErrorReport` that clients display. Budget exhaustion fails the run
with a `PermissionDenied` report; cancellation leaves it `Cancelled`. A gateway
restart cancels unfinished calls and drops pending approvals.

## Permissions

`Thread::tool_permissions` holds the user's Allow/Ask/Deny per model-facing tool
name; an unset tool is denied. The enforced permission is the most restrictive of:
the user's setting, every ancestor conversation's setting, the host's own policy
(`ToolSpec::permission`, for example an MCP server's per-tool policy), and Deny if
any capability the tool declares is outside the conversation's grant within its
mode ceiling. A delegated conversation cannot be set above its parent.

`Thread::tool_budget` limits model rounds per run (at least one) and tool calls
decided per run; both are capped at 256 and a delegated conversation cannot
exceed its parent.

Commands: `ListTools`, `SetToolPermission`, `SetToolBudget`, `ResolveApproval`.
CLI: `agent206 tools list|permit|budget` and `agent206 approvals list|approve|deny`.

## Approvals

An Ask call publishes an `ApprovalRequest` in the thread (`ApprovalRequested`
event) with a gateway-generated ID, the exact argument text and its digest
(SHA-256 of tool name and arguments). Only `ResolveApproval` with that ID and
digest answers it; any other digest is a conflict and leaves it pending. A request
is single-use: a repeated identical call needs a new approval. It expires
(`ApprovalResolved { outcome: Expired }`) when the run is cancelled or ends.
Approval IDs never enter model context, and tool hosts receive no handle that
could answer one.

## Tool hosts

```rust
pub trait ToolHost: Send + Sync + 'static {
    fn list<'a>(&'a self, scope: &'a ToolScope) -> BoxFuture<'a, ToolListing>;
    fn call<'a>(&'a self, scope: &'a ToolScope, request: ToolRequest)
        -> BoxFuture<'a, Result<ToolOutput, ErrorReport>>;
}
```

Hosts are added with `Gateway::with_tool_host`. Names are model-facing and
namespaced by the host (1–64 of `[A-Za-z0-9_-]`); a name two hosts offer is
offered by neither. Schemas must be object schemas (32 KiB at most). Results are
capped at 64 KiB with a declared truncation. Dropping a `call` future cancels the
call, so a host must stop the work it started. A host failure is an `ErrorReport`
so its kind and causes survive the process boundary.

`ToolScope` carries the thread ID, its project root and its effective
capabilities. `ToolRequest::approved` is true only after the user approved that
exact call.

### Adapters still to write

- MCP client (PR #16): `list` maps `Gateway::mcp_offers(thread)` to specs named
  `mcp__<server>__<tool>` with the server's per-tool policy as
  `ToolSpec::permission` and the server's declared capabilities as
  `ToolSpec::capabilities`, and `unavailable` servers to `UnavailableTools`.
  `call` passes `Approval::Approved` when `request.approved` and flattens
  `a206_mcp::ToolOutput` content blocks to text, keeping `is_error`.
- WASM extensions (PR #18): `list` maps `Gateway::extensions().tools()` to specs
  named per extension, converting `ParamSpec` lists to a JSON Schema object;
  `call` converts the JSON arguments to `a206_core::extension::Args` and calls
  `call_tool(extension, tool, args, Scope { workspace, ceiling: Some(scope.capabilities) })`.

Both adapters need only the gateway handle they wrap; the run engine is unchanged.

## Not implemented

Leaked tool-markup filtering, schema type coercion and repeat-call warnings from
RustyClaw's healing; Anthropic thinking-block replay alongside tool results;
`ask_user` prompts; retries; parallel tool execution.
