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
`ConfigureThread`, `SetBackend`, `GrantExtension`, `EnableExtension`,
`DisconnectMcpServer` and a `SaveSettings` that changes, disables or removes an
MCP server entry wake every run, which re-decides its running call and its
pending approval against the conversation and its ancestors, and lists the tools
again to confirm the call's host still offers that tool with the same
capabilities (an extension the user disabled or ungranted, or an MCP server the
user disconnected or disabled, no longer does). A running call that is no
longer permitted (Deny, or Ask for a call the user did not approve, or a tool
its host withdrew) is dropped, which stops it, and is settled `Denied` with a
`PermissionDenied` report. Withdrawing a host can end its call before the run
sees the change (a disconnected server's session closes), so these commands
signal before they stop any work, and a call that fails while a change is still
unseen is checked the same way before its failure is recorded: if the tool is
no longer permitted, the withdrawal is reported. A call that fails with no
change pending (a server that crashed) keeps its own failure. A pending approval
whose tool is now denied ends with `ApprovalResolved { outcome: Revoked }`. The run continues with the refusal as
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
capped at 64 KiB, or at the conversation's remaining storage, with a declared
truncation. A tool whose `ToolSpec::whole_output` is set (output only useful
complete, such as instruction text) is instead settled `Failed` with a
`Storage` report when its output does not fit, and none of it is kept. Dropping a `call` future cancels the
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
  conversation's own setting. The host independently checks both approval
  and Execute before starting anything, even on a direct call.
- **Arguments.** `command` (run with `sh -c`, 1 byte to 16 KiB, no NUL),
  optional `working_dir` relative to the project root (no `..`, absolute paths
  or symlinks leading out) and `timeout_secs` (1–600, default 30). Anything
  else, including unknown keys, is `InvalidRequest` before anything starts.
- **Sandbox.** The command runs under `a206-sandbox`: bubblewrap namespaces
  plus Landlock inside on Linux, a generated Seatbelt policy applied by
  `sandbox-exec` on macOS. Both come from one profile, with `/usr/bin` and
  `/bin` executable (programs elsewhere, such as Homebrew's, are not). The
  project is mounted only with `ReadWorkspace` and is writable only with
  `WriteWorkspace`; the host network is shared only with `Network`; gateway
  state and the credential stores under `$HOME` stay hidden even inside the
  project. A project equal to or inside protected state is refused. Without project access the command starts in an empty private
  directory. The environment holds only a private `HOME`, `TMPDIR`, a fixed
  `PATH` (`/usr/bin:/bin`) and `LANG`; stdin is empty.
- **Results.** The text is the exit status, then stdout and stderr, each cut
  to `MAX_STREAM` UTF-8 bytes after lossy conversion, with a marker; the rest is read and dropped so a
  chatty command cannot stall. A non-zero exit is `is_error`. A command still
  running at its timeout is killed and reported as a `Timeout` error.
  The deadline includes sandbox startup and readiness, as well as execution.
- **Cancellation.** Dropping the call kills the sandbox's process group; on
  Linux the sandbox's PID namespace ends with it, taking every descendant
  along. macOS has no PID namespace, so the group is also killed as soon as
  the shell exits; a process that leaves the group with `setsid` can outlive
  the call there, still confined by the same policy.
- **Other platforms.** Windows does not offer the tool; the listing reports
  `native exec` unavailable rather than run commands unconfined.

Tests: `crates/runtime/tests/native_exec.rs` drives the host through the real
sandbox on Linux and macOS CI (grants, hidden state, symlinked working
directories, network, exit status, output and time bounds, leftover
background processes; on Linux the test binary doubles as the helper), and
`crates/sandbox/src/seatbelt.rs` tests the macOS policy text on every platform.

### WASM extensions

`ExtensionToolHost` (`crates/runtime/src/extension_tools.rs`) is registered by
`Gateway::open`, so every gateway offers extension tools; it needs only the
`ExtensionHost` it wraps.

- **Listing.** `ExtensionHost::tool_sources()` returns each enabled
  extension's tools from its validated manifest, keeping only tools whose
  declared capabilities the user granted to the extension. Disabled or
  ungranted tools are never listed, and nothing here can enable an extension
  or change a grant. An extension whose manifest cannot be loaded is reported
  in `unavailable` (source: its id) with a structured report; if the
  extensions cannot be read at all, the source is `extensions`.
- **Names.** `ext__<extension id>__<tool>`, every `.` of the id written as
  `_` (`ext__agent206_workspace__read_text_file`). Because that mapping can
  merge ids, a name derived for more than one tool is offered for none of them
  (`Conflict` per claimant), and a name over 64 characters is refused
  (`InvalidRequest`); names are never truncated. Listing and calling derive
  names the same way, and a call re-reads the extensions, so a tool disabled
  or ungranted after the listing is `NotFound`.
- **Spec.** The schema is `{"type": "object", "properties": {<param>: {"type",
  "description"}}, "required": [...], "additionalProperties": false}` from
  the `ParamSpec`s; `capabilities` are the tool's manifest declaration; the
  host policy is Allow, so the conversation's setting (deny when unset)
  decides.
- **Arguments.** JSON values convert kind for kind (`null` → nil, integers that
  fit `i64` → integer, other numbers → float, arrays, objects). An integer
  beyond `i64` is refused; nothing is coerced. `call_tool` then checks them
  against the declaration (unknown, missing or mistyped parameters) before a
  worker starts. Every failure is an `ErrorReport` (`InvalidRequest` for
  arguments) the model receives as `tool … failed: …` text.
- **Authority.** `call_tool(extension, tool, args, Scope { workspace, ceiling:
  Some(scope.capabilities) })`. The gateway already denies a tool whose
  declaration exceeds the conversation's grant within its mode ceiling
  (ancestors attenuated it); the extension host checks the declaration again
  against the user's grant to the extension and that ceiling, and relayed
  host calls are re-checked against the same intersection. The extension never
  receives more than user grant ∩ mode ∩ thread (∩ parent) ∩ manifest.
- **Results.** A successful outcome is `ToolOutput { is_error: false }`. A
  guest's own `failed` outcome stays an `ExtensionError::Guest` report, so the
  last host call the gateway refused remains its cause rather than flattened
  text.
- **Bounds and cancellation.** Each call is one worker child with the
  plugin-api limits (fuel, memory, input/output, host calls, 10 s wall time).
  Dropping the call future kills the worker (`kill_on_drop`); a bounded
  workspace read already on the blocking pool finishes and is discarded, and
  extension state written before the drop stays. The call is therefore safe to
  cancel but does not resume.

Tests: `crates/runtime/src/extension_tools/tests.rs` (listing, naming,
conversion, checks before the worker) and `crates/runtime/tests/extension_tools.rs`
(native runs through the real worker; the test binary doubles as the worker).
`crates/runtime/tests/memory_tools.rs` does the same for the bundled memory
extension: nothing offered or callable without the user's grant, search and
get confined to memory files, and saving only with the separate write grant.

### Subagents

`crate::delegation_tool` offers `subagent_spawn`, `subagent_send`,
`subagent_list`, `subagent_status`, `subagent_wait` and `subagent_cancel`
(RustyClaw's `sessions_spawn/send/list/history/kill`). They are listed only to
conversations the user gave subagent models, and, like every tool, are denied
until the user permits them on that conversation.

- A subagent is an ordinary delegated thread created through `CreateSubagent`:
  its model must be in the caller's subagent model set (default: the caller's
  own model when permitted, or the only permitted one), and its grant is the
  caller's grant ∩ the requested capabilities ∩ its mode's ceiling.
- It starts with the caller's tool permissions except the subagent tools, and
  no subagent models, so it cannot delegate further unless the user configures
  that. Decisions still take the minimum along the delegation chain, so a later
  change to the parent's permissions or budget applies to the child at once.
- A model addresses only its own conversation's direct subagents. A conversation
  descended from 3 others cannot spawn, and at most 8 of a conversation's
  subagents run at once.
- `subagent_spawn` and `subagent_send` start the run in the background through
  `Send`, so it is admitted, journaled and shown like any other run, approvals
  included; a message the gateway refuses is reported by `subagent_status`.
  `subagent_wait` waits at most 600 seconds and is cancel-safe; it follows a
  run the user started in the subagent's thread too, re-reading it every
  250 ms.
- `CancelRun` cancels the thread's run and the runs of every thread delegated
  from it, whoever started them. A subagent run is reserved under its parent's
  run token before `subagent_spawn`/`subagent_send` returns, and only that
  tool's own `Send` admits the run with it (other messages to the subagent
  neither take nor drop it). Cancelling the parent therefore also cancels a
  subagent whose run has not been admitted yet.

The host holds a `Delegator`, not the gateway: it can create, message, cancel
and read subagents and nothing that changes a grant, permission or approval. The
delegator reaches the gateway's tool hosts through a weak link, so the hosts
holding it never keep a closed gateway alive. A gateway that gains a tool host
(`with_tool_host`) gets subagent tools bound to its own host list, so its other
clones keep theirs. Tests: `crates/runtime/tests/tool_loop.rs` (spawn,
inherited permissions, wait, cascading cancel, refusals),
`delegation_tool/tests.rs` and `gateway/delegation/tests.rs` (reservations,
clones, waiting on user-started runs).

### MCP servers

`McpToolHost` (`crates/runtime/src/mcp_tools.rs`) is registered by `Gateway::open`
after the extension host. It holds only the gateway's MCP client (server sessions
and the state they report to), not the gateway: no approvals, runs or vault.

- **Listing.** The tools of every enabled server the user has not disconnected
  that the conversation may use: `a206_mcp::server_callable` (enabled, the
  thread's grant covers every capability the server holds, a workspace server is
  bound to the thread's project) and the server's capabilities within
  `scope.capabilities`. Other servers are neither listed nor started; one that is
  starts on first use. A server that fails to start, or whose session ends, is
  reported in `unavailable` (source `mcp__<server>`) with its start failure as
  the cause, and the other servers' tools are still listed. If the servers
  cannot be read at all, the source is `mcp`.
- **Names.** `mcp__<server>__<tool>`, every character of the tool name outside
  `[A-Za-z0-9_-]` written as `_`. Server names never contain `_`, so the server is
  unambiguous; a name that two tools of one server would share is offered for
  neither (`Conflict` per claimant), and an empty tool name or a name over 64
  characters is refused (`InvalidRequest`); names are never truncated.
- **Spec.** The server's JSON Schema (an unparsable one is reported as
  `Protocol`); the description, else the title; the server's capabilities as
  `ToolSpec::capabilities`; the entry's per-tool policy (Ask unless configured) as
  `ToolSpec::permission`. Deny tools are listed so the user sees them, with an
  effective Deny, and are never offered to a model. Description and schema are
  untrusted server text, bounded by the catalog checks.
- **Calls.** Routed by the server's live session under its current entry (a call
  never starts a server); the host refuses a server capability outside
  `scope.capabilities` (`PermissionDenied`), then calls `Gateway::call_mcp_tool`,
  which authorizes the call against the thread's current grant, project and the
  entry's policy (`Approval::Approved` only when `request.approved`) and is the
  single enforcement point for MCP authority. Failures stay structured
  (`GatewayError::Mcp` reports, for example `NotConnected` → `Unavailable`).
- **Results.** Text blocks are joined with newlines; image, audio, binary resource
  and link blocks are described, not inlined; structured JSON is used only when a
  result has no content. An `isError` result is `ToolOutput { is_error: true }`,
  which the run engine records as a failed call with the tool's text as the
  report's cause.
- **Cancellation.** Dropping the call future (run cancelled, authority
  withdrawn) drops the session request, which sends the server an MCP
  `notifications/cancelled`; the server's eventual reply is discarded. Losing the
  thread's authority mid-call also cancels it at the server (`McpError::Revoked`),
  and changing the server's entry stops its session.

Native-run tests launch no server: the fixture binary belongs to `a206-mcp`, and
on Linux the launching binary must also serve as the sandbox helper. They serve the
Rust fixture (`crates/mcp/src/bin/a206-mcp-fixture/server.rs`) in process and attach
its session with `Gateway::attach_mcp_session`, which exists only with the
runtime's `test-support` feature (enabled for the runtime's own tests). Tests:
`crates/runtime/src/mcp_tools/tests.rs` (naming, specs, results, calls through the
gateway, the scope ceiling, cancellation) and `crates/runtime/tests/mcp_tools.rs`
(native runs).

## Questions for the user (`ask_user`)

`runtime/src/prompt_tool.rs` is a gateway-internal `ToolHost` offering one tool,
`ask_user`, with `question` (required), up to 8 distinct `choices` and
`allow_other` (default true; false needs choices). It uses no capability and,
like every tool, is denied until the user permits it in a conversation. A call
pushes a `UserPrompt` onto the thread and journals `PromptRequested`, then waits
while the call shows as Running. Only `Command::AnswerPrompt { thread, prompt,
answer }` answers it: the thread must match, the answer (`Choice { index }`,
`Text`, or `Decline`) must fit the question (`UserPrompt::check`, shared with the
clients), and a question is answered once. The model receives
`{"answer": text}`, `{"declined": true}`, or, if the call ends unanswered,
`{"declined": true, "expired": true}`; it never sees the question's id. Dropping
the call (cancel, permission withdrawn) withdraws the question with
`PromptResolved { outcome: Expired }`, and a restart clears pending questions.
An answer approves nothing. Clients: the conversation's question card
(`ui/src/prompts.rs`, desktop/web/TUI) and `agent206 prompts`.

## Context

### Project instruction files

At the start of each native run the gateway reads instruction files from the
project root when the conversation may read its project: `ReadWorkspace` is in its
grant within its mode's ceiling and in every ancestor's (the same intersection
tools get, `ToolScope::capabilities`). Otherwise nothing is read. The reviewed
list, in prompt order, is `AGENTS.md`, `CLAUDE.md`, `SOUL.md`, `IDENTITY.md`,
`TOOLS.md` (`crates/runtime/src/instructions.rs`). RustyClaw's `USER.md`,
`MEMORY.md`, `HEARTBEAT.md` and daily memory files are personal or scheduling state
and are not read into the prompt; a conversation the user lets use the bundled
`agent206.memory` tools can search and read `MEMORY.md` and `memory/` on demand.

- Files are opened through cap-std from the project root: absolute paths, `..` and
  symlinks that leave the project are refused; a symlink within the project is
  followed. Only regular files are read (FIFOs are opened non-blocking and
  refused), and only up to the per-file limit: the text included must be UTF-8
  without NUL, while bytes past the limit are never read or checked.
- Each file contributes at most 32 KiB and all of them 64 KiB, cut on a character
  boundary with a note in the prompt; a file whose content repeats an included one
  (for example `CLAUDE.md` linked to `AGENTS.md`) is skipped.
- They follow the mode prompt under a "Project instructions" heading that says they
  cannot grant permissions. They are read once per run.
- `Thread::instructions` records every file found: `Included`, `Truncated` (with
  the reason as a report) or `Skipped` (with a structured `ErrorReport`: outside the
  project, not regular, not text, duplicate, total limit used), its included bytes,
  size and the SHA-256 of the included text. Missing files are not listed. The
  desktop/TUI context popover shows it.

### Compaction

`crates/runtime/src/gateway/compaction.rs`. A compaction summarizes a conversation's
older turns (a turn starts at a user message) with the conversation's own model and
effort, through `provider::complete` like any request: same broker, credential
binding and stream checks, no tools offered. The summarizing model gets a text
transcript (each message at most 8 KiB, tool arguments and results 2 KiB, the whole
transcript 256 KiB with the earliest messages left out first and said so), with the
previous summary first.

- **Automatic.** Before each model request of a native run, if the previous
  request's provider-reported input tokens exceed `COMPACTION_THRESHOLD_PERCENT`
  (75, a policy choice shared with RustyClaw, not a model fact) of the model's
  API-reported `ModelInfo::context_limit`, the run compacts everything before the
  current and the previous turn. Before the first round "previous" is the last
  request of the previous run. An unknown limit or input count never triggers it.
  When only those two turns are in context there is nothing to compact and the
  request is sent as it is (a single turn larger than the window is not reduced).
- **Manual.** `Command::CompactThread { thread }` compacts a native conversation
  that is not running, keeping its latest turn, whatever its context usage or
  limit. It marks the thread `Running` (so `Send` waits and `CancelRun` stops it)
  with the previous state saved in `Thread::compacting`, restores that state
  afterwards, or on the next start if the gateway stopped meanwhile, and replies
  with the thread. Nothing to
  compact is a `Conflict` before any request. CLI: `agent206 compact THREAD`.
- **Result.** The summary is inserted as a `Role::System` message with
  `Message::compaction` (trigger, messages covered, previous summary, input tokens
  and context limit at the time, the summary request's usage) just before the first
  kept turn. The covered messages stay in the thread for the user. A model request
  carries only the latest summary and the messages after it: the summary, after a
  fixed preamble, is prefixed to the first following user message (so every
  protocol keeps a user-first, alternating history and the summary never gets
  system-prompt authority). Since a call and its result live on one assistant
  message and cuts fall only before user messages, every tool call keeps its
  result in OpenAI, Anthropic and Gemini form. `Thread::usage` becomes unknown
  until the next request reports it.
- **Accounting.** An automatic compaction's usage is added to `Thread::run_usage`;
  it does not count against the round budget.
- **Failure and cancellation.** Nothing changes until the summary is complete. A
  failed summary (`CompactionError::Summary`, with the provider failure as its
  cause), an empty one, or a conversation that changed meanwhile leaves the history
  intact: an automatic compaction fails the run with that report, a manual one
  replies with it. Cancellation (`CancelRun`, shutdown) abandons the summary
  request and leaves the run `Cancelled` or the manual compaction failed with
  `Cancelled`.
- **Memory flush** (`gateway/memory_flush.rs`). When an automatic compaction is
  due and the round would offer the bundled `memory_save` tool (extension enabled
  and granted, the conversation's permission not Deny), the gateway first sends one
  round with the full context and a flush instruction added to that round's system
  prompt: save lasting notes with `memory_save` (suggesting `memory/YYYY-MM-DD.md`
  in the gateway's local date), then continue with the user's request. The user's
  turn is kept; the model answers in the same run. Saves go through the ordinary
  tool path, so an `Ask` permission asks the user. The compaction then happens
  before the next round, or at the start of the next run if the model just
  answered. The flush is offered once per summary: `Thread::memory_flush` records
  the latest summary when a flush round completes, so restarts and later runs do
  not repeat it, while a failed or cancelled flush round is offered again. It is
  offered only when the compaction would summarize at least one turn and the budget
  leaves a tool call for the save and a round to answer in. The tools offered are
  listed again after a compaction. Manual compaction has no
  flush. Follows RustyClaw's `memory_flush.rs`, but triggers at the compaction
  threshold instead of a fixed token count before it.
- Compaction does not reduce the conversation's stored size.

Tests: `crates/runtime/tests/context.rs` (scripted OpenAI, Anthropic and Gemini
streams) and unit tests in `compaction.rs`, `memory_flush.rs` and `instructions.rs`.

## Retrying model requests

`crates/runtime/src/retry.rs`. A model request (a round or a compaction summary) is
repeated only while nothing of its response has arrived. That covers a connection
failure or timeout (made directly, or reported by the vault broker for a
credentialed request), and an HTTP 408, 429, 500, 502, 503, 504 or 529 refusal. Once any
response bytes are received, the failure is final, so no text or tool call is
streamed twice. Any other status is also final. A retry repeats the identical
request to the same provider, model and credential binding: there is no fallback
to another model or account.

`RetryPolicy::DEFAULT` makes 4 attempts. Its backoff starts at 250 ms, doubles up
to 8 s and is spread by ±20%, never past the 8 s cap. A `Retry-After` in seconds from a direct
(uncredentialed) provider is honoured up to 30 s; brokered requests don't carry
the header, so they use the backoff. These are policy choices, not provider facts.
While a native round waits to retry, the conversation's activity line says why and
which attempt comes next, for example "The provider answered HTTP 503; retrying
(attempt 2 of 4)". The line is cleared when the round ends. `CancelRun` ends the
wait at once. When the attempts run out, the run fails with the last attempt's
structured error. Follows RustyClaw's `retry/policy.rs`; auth-profile failover is
not carried over.

Tests: `crates/runtime/tests/retries.rs` (scripted OpenAI, Anthropic and Gemini
providers: refusals, attempts, final statuses, broken connections, cancellation)
and unit tests in `retry.rs`.

## Not implemented

Leaked tool-markup filtering, schema type coercion and repeat-call warnings from
RustyClaw's healing; Anthropic thinking-block replay alongside tool results;
parallel tool execution.
