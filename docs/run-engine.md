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
   Calls are recorded in order while their arguments fit in the conversation's
   remaining storage, keeping a reserve for settling this response's calls and
   for the next round (the longest reply the provider path accepts, its metadata
   and its own settlement); the rest fail with a `Storage` report and no stored
   arguments, and the run goes on. Tool outputs are likewise cut to the space
   above that reserve. A round never starts without room for its reply; if that
   room is gone the run fails with `HistoryFull` instead of streaming a reply it
   could not store.
4. Decide each call in order against the conversation and its ancestors as they
   are now (a permission, grant or budget narrowed mid-run applies to the next
   call), then run it, ask first, or refuse it.
5. Send every call's result text back in the protocol's own shape and repeat.
   Arguments are replayed as recorded: the canonical object a call ran with, or
   exactly the text the model sent when it could not be used. OpenAI replays that
   text as its argument string; Anthropic and Gemini accept only an object, so it
   is replayed verbatim as `{"unparsed_arguments": "<text>"}` beside the error
   result, never as an invented `{}`.

`Thread::usage` is the final model request's usage, which is what the context
widget compares with the model's limit. `Thread::run_usage` sums every request of
the run for cost; a count any request did not report leaves that total unknown.
It is cleared when a message is sent and set only when the run completes, so a
failed or cancelled run shows no cost rather than the previous run's.

Every call ends with model-facing result text and, unless it succeeded, a
structured `ErrorReport` that clients display. When a tool reports its own
failure (`ToolOutput::is_error`), the model receives the tool's text and the
report carries it as its cause, bounded like any report message; it is attached
after the report is logged, so tool output never reaches the logs. Budget exhaustion fails the run
with a `PermissionDenied` report; cancellation leaves it `Cancelled`. A gateway
restart cancels unfinished calls and drops pending approvals.

## Permissions

`Thread::tool_permissions` holds the user's Allow/Ask/Deny per model-facing tool
name; an unset tool is denied. The enforced permission is the most restrictive of:
the user's setting, every ancestor conversation's setting, the host's own policy
(`ToolSpec::permission`, for example an MCP server's per-tool policy), and Deny if
any capability the tool declares is outside the conversation's grant within its
mode ceiling or outside any ancestor's current grant within that ancestor's
ceiling. A delegated conversation cannot be set above its parent, and a parent
narrowed later narrows its descendants at their next decision.

Withdrawn authority also stops work in flight. `SetToolPermission`,
`ConfigureThread` and `SetBackend` wake every run, which re-decides its running
call and its pending approval against the conversation and its ancestors. A
running call that is no longer permitted (Deny, or Ask for a call the user did
not approve) is dropped, which stops it, and is settled `Denied` with a
`PermissionDenied` report; a pending approval whose tool is now denied ends with
`ApprovalResolved { outcome: Revoked }`. The run continues with the refusal as
that call's result.

`Thread::tool_budget` limits model rounds per run (at least one) and tool calls
decided per run; both are capped at 256 and a delegated conversation cannot be
set above its parent. The budget enforced at each check is, for rounds and for
calls independently, the smallest of the conversation's and every ancestor's
current budget.

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

### Contract for clients and messengers

`Command::Send` through `Gateway::request` resolves only when the run ends: to
`Reply::Thread` with the finished thread, which is `Complete`, `Cancelled`, or
`Failed` with its `ErrorReport` in `Thread::error` (a request that is refused
before a run starts is a `Reply::Error`). While a call waits for approval, `Send`
simply stays pending. The approval is published in `Thread::approvals` and as an
`ApprovalRequested` event, never as a chat message; it is answered only by a
`ResolveApproval` request from a trusted client, sent as another request on the
same or another connection. `CancelRun` expires it, and `Send` then returns the
`Cancelled` thread. Covered by
`send_stays_pending_while_an_approval_waits_and_returns_the_finished_thread` and
`cancelling_during_an_approval_wait_expires_the_request` in
`crates/runtime/tests/tool_loop.rs`.

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

### Native exec

`ExecHost` (`crates/runtime/src/exec_tool.rs`) is registered by `Gateway::open`
and offers one tool, `execute_command`.

- **Policy.** The spec declares only `Execute` and the host policy is `Ask`, so
  the tool is offered only to conversations granted Execute and every call
  waits for the user's approval of that exact command, whatever the
  conversation's own setting.
- **Arguments.** `command` (run with `sh -c`, 1 byte to 16 KiB, no NUL),
  optional `working_dir` relative to the project root (no `..`, absolute paths
  or symlinks leading out) and `timeout_secs` (1–600, default 30). Anything
  else, including unknown keys, is `InvalidRequest` before anything starts.
- **Sandbox.** On Linux the command runs under `a206-sandbox` (bubblewrap
  namespaces plus Landlock inside), with `/usr/bin` and `/bin` executable. The
  project is mounted only with `ReadWorkspace` and is writable only with
  `WriteWorkspace`; the host network is shared only with `Network`; gateway
  state and the credential stores under `$HOME` stay hidden even inside the
  project. Without project access the command starts in an empty private
  directory. The environment holds only a private `HOME`, `TMPDIR`, a fixed
  `PATH` and `LANG`; stdin is empty.
- **Results.** The text is the exit status, then stdout and stderr, each cut
  to `MAX_STREAM` bytes with a marker; the rest is read and dropped so a
  chatty command cannot stall. A non-zero exit is `is_error`. A command still
  running at its timeout is killed and reported as a `Timeout` error.
- **Cancellation.** Dropping the call kills the sandbox's process group; the
  sandbox's PID namespace ends with it, taking every descendant along.
- **Other platforms.** The tool is not offered; the listing reports
  `native exec` unavailable rather than run commands unconfined.

Tests: `crates/runtime/tests/native_exec.rs` drives the host through the real
sandbox (grants, hidden state, symlinked working directories, network, exit
status, output and time bounds); the test binary doubles as the helper.

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
