# Upstream evidence and reuse

Inspected on 2026-10-15:

- [RustyClaw](https://github.com/rexlunae/RustyClaw): MIT security architecture,
  SecureStore vault, SSH process transport and legacy CLI. Reference clone is
  ignored under `.reference`; no monolithic dependency was imported.
- [fc-dioxus](https://github.com/rexlunae/fc-dioxus): shared Dioxus 0.7.9 components,
  future-css themes and accessible splitters. Pinned in Cargo.lock.
- [future-css](https://github.com/rexlunae/future-css): tokens, semantic components,
  theme system and dual-audience state attributes, through the toolkit's own pin.
- [Joshua](https://github.com/rexlunae/joshua): `serve --model ... --addr ...` and
  the standard `/v1/models` / `/v1/chat/completions` APIs. No model catalog copied.
- [Dioxus TUI fork](https://github.com/JakkuSakura/dioxus-tui): Dioxus 0.7-compatible
  renderer. The registry's old alpha is incompatible with this component tree.
- [ACP official Rust SDK](https://github.com/agentclientprotocol/rust-sdk): ACP v1
  initialize, sessions, prompt updates and cancellation through SDK handlers.
- [MCP official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk): scoped
  harness introspection over standard stdio, with per-session opt-in title editing.
- [Wasmi](https://github.com/wasmi-labs/wasmi): a bounded interpreter suitable for
  the initial no-WASI module ABI. Chosen over a heavyweight JIT for this stage;
  a component-model/Wasmtime migration should follow measured requirements.
- [cap-std](https://github.com/bytecodealliance/cap-std): directory-scoped document
  reads that resist symlink escape without application unsafe code.
- [SecureStore](https://github.com/neosmart/securestore-rs): compatibility with the
  upstream vault's encrypted file format. Its pure-Rust crypto backend is selected.

RustyClaw-inspired code is attributed in NOTICE and source module comments. The
vault atomic-write approach is adapted; the original insecure-memory extraction
helper using unsafe code was not carried forward. The SSH invocation preserves
keepalives and uses stricter host verification; the wire format is a fresh,
explicitly versioned MessagePack protocol rather than a legacy codec fork.

ArtCraft and forge/document plugin candidates still need source/license evaluation
before selecting those integrations. They are not dependencies in this build.

Coding-backend interfaces (our Rust adapters consume separately installed runtimes;
no upstream engine implementation was copied):

- [Codex app-server](https://developers.openai.com/codex/app-server): documented
  bidirectional JSON-RPC integration for sessions, turns, streaming and approvals.
  It requires an adapter into Agent 206's agent interface; it is not ACP itself.
- [Claude Agent SDK](https://code.claude.com/docs/en/agent-sdk/overview): official
  Python/TypeScript libraries running the Claude Code runtime with tools, sessions,
  permissions, hooks and subagents. Third-party products use API-key authentication
  unless otherwise approved. The integrated agent should be labeled Claude Agent.
- [Claude Agent ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp):
  Apache-2.0 TypeScript bridge using the official SDK, including permission requests,
  edit review, terminals, MCP and subagent transcripts. Negotiate optional protocol
  extensions; the current Rust SDK may not support all of them.
- [Claude headless interface](https://code.claude.com/docs/en/headless): a subprocess
  alternative for Rust. Bare mode suppresses ambient configuration discovery;
  neither bare mode nor tool approval callbacks replace OS sandbox enforcement.
- [Claude runtime license](https://github.com/anthropics/claude-code/blob/main/LICENSE.md):
  separate Anthropic terms; the ACP adapter's license does not license the runtime.

The fc-dioxus dependency pins `8349dc00bfe4e4831857239c9208d0e3e3dbc7a8`
from the feature-surface-contrast fix branch. Its future-css pin is
`af9f44bd151bc14bcfe0c495f8301463e61f7ec3`. These revisions retain fixes for
bottom-edge popup placement in the source toolkit and host bridge, with parity
and viewport regression tests. The source fixes are in [future-css #63](https://github.com/rexlunae/future-css/pull/63) and [fc-dioxus #8](https://github.com/rexlunae/fc-dioxus/pull/8).
The toolkit revision also includes [fc-dioxus #10](https://github.com/rexlunae/fc-dioxus/pull/10):
blur and Tab dismiss a Select without reclaiming focus, so switching selectors
does not leave an unfocused popup open. Native browser regression tests cover
selector switching, blur, and Tab traversal.
They also consume [future-css #64](https://github.com/rexlunae/future-css/pull/64)
through [fc-dioxus #11](https://github.com/rexlunae/fc-dioxus/pull/11): feature
surfaces provide their own contrast-checked text context to labels, metadata,
helper copy and transparent actions, while nested painted surfaces reset to
their own palette and clear decorative text shadows. Upstream browser checks
cover gradient stops, base typography and distinct button interaction states.
Agent 206's muted prose uses that context too. The toolkit
snapshot was regenerated by its sync tool; generated CSS was not hand-edited.
Selectable-card styling applies to label cards. Checking an ordinary preference
inside a content card leaves that card's palette and gradient intact. Upstream
state tests respect the device's hover capability, including headless touch-style
devices; this does not lower any contrast requirement.

Provider adapters follow the primary [Anthropic model](https://platform.claude.com/docs/en/api/models/list)
and [Messages](https://platform.claude.com/docs/en/api/messages/create) contracts,
[Gemini model](https://ai.google.dev/api/models) and
[generation](https://ai.google.dev/api/generate-content) contracts, and
[OpenRouter model metadata](https://openrouter.ai/docs/api/api-reference/models/get-models).
Endpoint defaults are connector setup data; no example model IDs or facts from
these documents are copied into the app or tests.

Expanded connector defaults were checked against official provider documentation:
[DeepSeek](https://api-docs.deepseek.com/),
[Moonshot/Kimi](https://platform.kimi.ai/docs/api/overview),
[Alibaba Cloud](https://www.alibabacloud.com/help/en/model-studio/compatibility-of-openai-with-dashscope),
[Z.ai](https://docs.z.ai/guides/overview/quick-start),
[MiniMax](https://platform.minimax.io/docs/api-reference/text-openai-api),
[SiliconFlow](https://docs.siliconflow.com/en/userguide/quickstart),
[StepFun](https://platform.stepfun.ai/docs/en/guides/developer/openai),
[Hunyuan](https://cloud.tencent.com/document/product/1729/111007),
[Volcengine](https://www.volcengine.com/docs/ark/quick-start),
[Qianfan](https://cloud.baidu.com/doc/qianfan-docs/s/Fm9l6ocai),
[Mistral](https://docs.mistral.ai/resources/migration-guides),
[Groq](https://console.groq.com/docs/openai),
[Together](https://docs.together.ai/docs/inference/openai-compatibility),
[Fireworks](https://docs.fireworks.ai/tools-sdks/openai-compatibility),
[xAI](https://docs.x.ai/overview),
[Perplexity](https://docs.perplexity.ai/docs/getting-started/quickstart),
[Ollama](https://docs.ollama.com/api/openai-compatibility) and
[LM Studio](https://lmstudio.ai/docs/developer/openai-compat).
Alibaba's workspace/region URL remains user-entered instead of embedding an account
identifier or assuming a legacy regional endpoint. Compatible metadata parsing also
follows [DeepSeek's live model schema](https://api-docs.deepseek.com/api/list-models/)
for `context_window` and nested `effort.supported_levels`; no example facts are copied.

Encrypted browser vault setup uses [Snow 0.10](https://docs.rs/snow/0.10.0/snow/)
for the standard Noise NK handshake and sequential authenticated transport.
The application supplies a versioned prologue, an independently verified broker
public key and a deliberately limited MessagePack setup schema; it does not
implement new cryptographic primitives. The native and WASM builds share this
crate. See [web-vault-setup.md](web-vault-setup.md) for trust and failure semantics.

Authenticator enrollment QR encoding uses [qrcode](https://docs.rs/qrcode/0.14.1/qrcode/)
without its image features; [rqrr](https://docs.rs/rqrr/0.11.0/rqrr/) is a test-only
independent decoder. The app renders typed SVG geometry within its trusted setup
surface and uses fc-dioxus Disclosure for the manual alternative; no QR service
or seed-bearing image URL is involved.
