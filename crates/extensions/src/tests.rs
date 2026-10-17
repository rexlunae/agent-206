//! Real WASM execution: the bundled guest and hand-written adversarial modules.
use super::*;
use crate::abi::*;
use std::collections::BTreeMap;

/// A host that answers from a closure and records every request.
struct Mock<F> {
    answer: F,
    seen: std::rc::Rc<std::cell::RefCell<Vec<HostRequest>>>,
}
impl<F: FnMut(&HostRequest) -> HostReply + 'static> HostBridge for Mock<F> {
    fn call(&mut self, request: HostRequest) -> Result<HostReply, BridgeError> {
        let reply = (self.answer)(&request);
        self.seen.borrow_mut().push(request);
        Ok(reply)
    }
}
fn mock<F: FnMut(&HostRequest) -> HostReply + 'static>(
    answer: F,
) -> (Mock<F>, std::rc::Rc<std::cell::RefCell<Vec<HostRequest>>>) {
    let seen = std::rc::Rc::default();
    (
        Mock {
            answer,
            seen: std::rc::Rc::clone(&seen),
        },
        seen,
    )
}

fn workspace() -> Builtin {
    builtin("agent206.workspace").unwrap().unwrap()
}
fn tool(name: &str, args: &[(&str, Value)]) -> Invocation {
    Invocation {
        abi: ABI,
        call: Call::Tool {
            name: name.into(),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        },
    }
}
fn panel(event: Option<&str>) -> Invocation {
    Invocation {
        abi: ABI,
        call: Call::Panel {
            id: "overview".into(),
            event: event.map(|element| UiEvent {
                element: element.into(),
            }),
        },
    }
}
fn entry(name: &str, kind: EntryKind) -> DirEntry {
    DirEntry {
        name: name.into(),
        kind,
        size: (kind == EntryKind::File).then_some(name.len() as u64),
    }
}
/// A unique name, so assertions cannot pass on fixed strings in the guest.
fn unique(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[test]
fn bundled_tool_lists_through_host_calls() {
    let (dir, file) = (unique("dir"), unique("file"));
    let entries = vec![
        entry(&file, EntryKind::File),
        entry(&dir, EntryKind::Directory),
    ];
    let (bridge, seen) = mock(move |_| HostReply::Directory(entries.clone()));
    let (outcome, _) = engine::run(
        workspace().wasm,
        &tool("list_directory", &[("path", Value::Str(dir.clone()))]),
        Limits::default(),
        bridge,
    );
    let outcome = outcome.unwrap();
    let Outcome::Tool(output) = outcome else {
        panic!("expected tool output, got {outcome:?}");
    };
    // Directories sort first; files carry the size the host reported.
    assert_eq!(
        output.text,
        format!("{dir}/\n{file} ({} bytes)", file.len())
    );
    assert_eq!(
        *seen.borrow(),
        vec![HostRequest::ListDirectory { path: dir }]
    );
}

#[test]
fn host_denials_reach_the_guest_as_typed_failures() {
    let message = unique("denied");
    let failure = HostFailure {
        code: HostErrorCode::Denied,
        message: message.clone(),
    };
    let (bridge, _) = mock(move |_| HostReply::Failed(failure.clone()));
    let (outcome, _) = engine::run(
        workspace().wasm,
        &tool("read_text_file", &[("path", Value::Str("a".into()))]),
        Limits::default(),
        bridge,
    );
    let outcome = outcome.unwrap();
    assert_eq!(
        outcome,
        Outcome::Failed(GuestError::new("denied", message.clone()))
    );
}

#[test]
fn panel_events_update_extension_state() {
    let hidden = format!(".{}", unique("hidden"));
    let mut state: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let listed = hidden.clone();
    let (bridge, seen) = mock(move |request| match request {
        HostRequest::StateGet { key } => HostReply::State(state.get(key).cloned().map(Into::into)),
        HostRequest::StatePut { key, value } => {
            match value {
                Some(value) => state.insert(key.clone(), value.to_vec()),
                None => state.remove(key),
            };
            HostReply::Done
        }
        HostRequest::ListDirectory { .. } => {
            HostReply::Directory(vec![entry(&listed, EntryKind::File)])
        }
        _ => HostReply::Failed(HostFailure {
            code: HostErrorCode::Unsupported,
            message: String::new(),
        }),
    });
    let (outcome, _) = engine::run(
        workspace().wasm,
        &panel(Some("toggle-hidden")),
        Limits::default(),
        bridge,
    );
    let outcome = outcome.unwrap();
    let Outcome::Panel(view) = outcome else {
        panic!("expected a panel, got {outcome:?}");
    };
    assert!(view.nodes.iter().any(
        |node| matches!(node, Node::List { items } if items.iter().any(|i| i.starts_with(&hidden)))
    ));
    assert!(view.nodes.contains(&Node::Button {
        id: "toggle-hidden".into(),
        label: "Hide hidden entries".into()
    }));
    assert!(
        seen.borrow()
            .iter()
            .any(|request| matches!(request, HostRequest::StatePut { value: Some(_), .. }))
    );
}

#[test]
fn a_toggle_that_cannot_be_shown_is_not_saved() {
    let message = unique("denied");
    let (bridge, seen) = mock(move |request| match request {
        HostRequest::StateGet { .. } => HostReply::State(None),
        _ => HostReply::Failed(HostFailure {
            code: HostErrorCode::Denied,
            message: message.clone(),
        }),
    });
    let (outcome, _) = engine::run(
        workspace().wasm,
        &panel(Some("toggle-hidden")),
        Limits::default(),
        bridge,
    );
    assert!(matches!(outcome, Ok(Outcome::Failed(_))), "{outcome:?}");
    assert!(
        !seen
            .borrow()
            .iter()
            .any(|request| matches!(request, HostRequest::StatePut { .. }))
    );
}

#[test]
fn long_files_are_cut_to_fit_the_outcome_limit() {
    let limits = Limits::default();
    let line = format!("{}\n", unique("é"));
    let body = line.repeat(limits.output_bytes / line.len() + 1);
    let size = body.len();
    let (bridge, _) = mock(move |_| HostReply::File(body.clone().into_bytes().into()));
    let (outcome, _) = engine::run(
        workspace().wasm,
        &tool("read_text_file", &[("path", Value::Str("a".into()))]),
        limits,
        bridge,
    );
    let outcome = outcome.unwrap();
    let Outcome::Tool(output) = outcome else {
        panic!("expected tool output, got {outcome:?}");
    };
    assert!(output.text.starts_with(&line));
    assert!(output.text.len() < size);
    assert!(output.text.ends_with(&format!("of {size} bytes]")));
}

#[test]
fn write_tool_creates_parents_then_writes() {
    let (dir, file, text) = (unique("dir"), unique("file"), unique("text"));
    let path = format!("{dir}/{file}");
    let (bridge, seen) = mock(|_| HostReply::Done);
    let (outcome, _) = engine::run(
        workspace().wasm,
        &tool(
            "write_text_file",
            &[
                ("path", Value::Str(path.clone())),
                ("text", Value::Str(text.clone())),
            ],
        ),
        Limits::default(),
        bridge,
    );
    assert!(matches!(outcome, Ok(Outcome::Tool(_))), "{outcome:?}");
    assert_eq!(
        *seen.borrow(),
        vec![
            HostRequest::CreateDirectory { path: dir },
            HostRequest::WriteFile {
                path,
                contents: text.into_bytes().into(),
                overwrite: false,
            },
        ]
    );
}

#[test]
fn search_tool_reports_matches_and_truncation() {
    let (path, text, query) = (unique("src/f"), unique("line"), unique("q"));
    let include = unique("*.");
    let reply = SearchResult {
        matches: vec![SearchMatch {
            path: path.clone(),
            line: 7,
            text: text.clone(),
        }],
        truncated: true,
    };
    let (bridge, seen) = mock(move |_| HostReply::Matches(reply.clone()));
    let (outcome, _) = engine::run(
        workspace().wasm,
        &tool(
            "search_text",
            &[
                ("query", Value::Str(query.clone())),
                ("max_matches", Value::Int(9000)),
                ("regex", Value::Bool(true)),
                ("include", Value::Str(include.clone())),
            ],
        ),
        Limits::default(),
        bridge,
    );
    let Ok(Outcome::Tool(output)) = outcome else {
        panic!("expected tool output, got {outcome:?}");
    };
    assert!(output.text.starts_with(&format!("{path}:7: {text}\n")));
    assert!(output.text.contains("limit"));
    assert_eq!(
        *seen.borrow(),
        vec![HostRequest::SearchPattern {
            path: ".".into(),
            pattern: query,
            regex: true,
            ignore_case: false,
            include: Some(include),
            max_matches: 500,
        }]
    );
}

#[test]
fn find_tool_lists_entries_and_truncation() {
    let (file, directory, link) = (unique("src/f"), unique("src/d"), unique("l"));
    let (glob, within) = (unique("*"), unique("src"));
    let reply = FindResult {
        entries: vec![
            FoundEntry {
                path: file.clone(),
                kind: EntryKind::File,
            },
            FoundEntry {
                path: directory.clone(),
                kind: EntryKind::Directory,
            },
            FoundEntry {
                path: link.clone(),
                kind: EntryKind::Symlink,
            },
        ],
        truncated: true,
    };
    let (bridge, seen) = mock(move |_| HostReply::Found(reply.clone()));
    let (outcome, _) = engine::run(
        workspace().wasm,
        &tool(
            "find_files",
            &[
                ("pattern", Value::Str(glob.clone())),
                ("path", Value::Str(within.clone())),
                ("max_results", Value::Int(0)),
            ],
        ),
        Limits::default(),
        bridge,
    );
    let Ok(Outcome::Tool(output)) = outcome else {
        panic!("expected tool output, got {outcome:?}");
    };
    assert!(
        output
            .text
            .starts_with(&format!("{file}\n{directory}/\n{link} (link)\n")),
        "{}",
        output.text
    );
    assert!(output.text.contains("limit"));
    assert_eq!(
        *seen.borrow(),
        vec![HostRequest::Find {
            path: within,
            glob,
            max_results: 1,
        }]
    );
}

#[test]
fn git_tool_renders_status_from_host_calls() {
    let builtin = builtin("agent206.git").unwrap().unwrap();
    let (branch, path) = (unique("branch"), unique("file"));
    let status = GitStatus {
        branch: Some(branch.clone()),
        head: None,
        entries: vec![GitEntry {
            path: path.clone(),
            area: GitArea::Staged,
            change: GitChange::Added,
        }],
        truncated: false,
    };
    let (bridge, seen) = mock(move |_| HostReply::GitStatus(status.clone()));
    let (outcome, _) = engine::run(
        builtin.wasm,
        &tool("git_status", &[]),
        Limits::default(),
        bridge,
    );
    let Ok(Outcome::Tool(output)) = outcome else {
        panic!("expected tool output, got {outcome:?}");
    };
    assert_eq!(
        output.text,
        format!("on {branch}, no commits yet\nstaged added {path}")
    );
    assert_eq!(*seen.borrow(), vec![HostRequest::GitStatus]);
}

/// A diff at the host's limit still fits the outcome limit once the guest
/// adds its headers, however long the path.
#[test]
fn long_diffs_fit_the_outcome_limit() {
    let builtin = builtin("agent206.git").unwrap().unwrap();
    let limits = Limits::default();
    let path = unique("p").repeat(limits.output_bytes / 16);
    let reply = GitDiff {
        text: "+x\n".repeat(limits.output_bytes / 4),
        binary: false,
        truncated: true,
    };
    let (bridge, _) = mock(move |_| HostReply::Diff(reply.clone()));
    let (outcome, _) = engine::run(
        builtin.wasm,
        &tool("git_diff", &[("path", Value::Str(path.clone()))]),
        limits,
        bridge,
    );
    let Ok(Outcome::Tool(output)) = outcome else {
        panic!("expected tool output, got {outcome:?}");
    };
    assert!(output.text.starts_with(&format!("--- a/{path}\n")));
    assert!(output.text.ends_with(" bytes]"));
}

#[test]
fn web_tool_reports_status_type_and_text() {
    let builtin = builtin("agent206.web").unwrap().unwrap();
    let (page, url) = (
        unique("page"),
        format!("https://{}.example/a", unique("site")),
    );
    let body = page.clone().into_bytes();
    let (bridge, seen) = mock(move |_| {
        HostReply::Fetched(FetchResponse {
            status: 404,
            content_type: Some("text/plain".into()),
            body: body.clone().into(),
            truncated: true,
        })
    });
    let invocation = tool("fetch_url", &[("url", Value::Str(format!("{url}#part")))]);
    let (outcome, _) = engine::run(builtin.wasm, &invocation, Limits::default(), bridge);
    let Ok(Outcome::Tool(output)) = outcome else {
        panic!("expected tool output, got {outcome:?}");
    };
    assert_eq!(
        output.text,
        format!(
            "HTTP 404\nContent-Type: text/plain\n\n{page}\n[the response continues; it was cut at a limit]"
        )
    );
    // The fragment never reaches the host call.
    assert_eq!(*seen.borrow(), vec![HostRequest::Fetch { url }]);
}

/// A body the host cut inside a multibyte character is still shown as text.
#[test]
fn web_tool_keeps_text_cut_mid_character() {
    let builtin = builtin("agent206.web").unwrap().unwrap();
    let page = unique("page");
    let mut body = format!("{page}€").into_bytes();
    body.pop();
    let (bridge, _) = mock(move |_| {
        HostReply::Fetched(FetchResponse {
            status: 200,
            content_type: None,
            body: body.clone().into(),
            truncated: true,
        })
    });
    let invocation = tool(
        "fetch_url",
        &[("url", Value::Str("https://a.example/".into()))],
    );
    let (outcome, _) = engine::run(builtin.wasm, &invocation, Limits::default(), bridge);
    let Ok(Outcome::Tool(output)) = outcome else {
        panic!("expected tool output, got {outcome:?}");
    };
    assert_eq!(
        output.text,
        format!("HTTP 200\n\n{page}\n[the response continues; it was cut at a limit]")
    );
}

#[test]
fn budgets_stop_the_guest() {
    let reply = |_: &HostRequest| HostReply::Directory(vec![]);
    let (fuel, _) = engine::run(
        workspace().wasm,
        &tool("list_directory", &[]),
        Limits {
            fuel: 1_000,
            ..Limits::default()
        },
        mock(reply).0,
    );
    assert!(matches!(fuel, Err(EngineError::OutOfFuel(_))), "{fuel:?}");
    let (calls, _) = engine::run(
        workspace().wasm,
        &panel(None),
        Limits {
            host_calls: 0,
            ..Limits::default()
        },
        mock(reply).0,
    );
    assert!(
        matches!(
            calls,
            Err(EngineError::Abi(AbiViolation::HostCalls { limit: 0 }))
        ),
        "{calls:?}"
    );
}

/// A module whose invoke returns `output` verbatim, plus `body` as extra code.
fn module(output: &[u8], body: &str) -> String {
    let escaped: String = output.iter().map(|b| format!("\\{b:02x}")).collect();
    format!(
        r#"(module
            (import "a206" "host_call" (func $call (param i32 i32) (result i32)))
            (import "a206" "host_take" (func $take (param i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "{escaped}")
            (func (export "a206_buffer") (param i32) (result i32) i32.const 4096)
            (func (export "a206_invoke") (param i32) (result i64) {body} i64.const {}))"#,
        output.len()
    )
}
fn run_module(wat: &str, invocation: &Invocation) -> Result<Outcome, EngineError> {
    let (bridge, _) = mock(move |_| HostReply::Done);
    engine::run(wat.as_bytes(), invocation, Limits::default(), bridge).0
}

#[test]
fn malformed_guest_output_is_rejected() {
    let garbage = run_module(&module(b"\xc1", ""), &panel(None));
    assert!(
        matches!(garbage, Err(EngineError::Output(_))),
        "{garbage:?}"
    );
    let tool_answer = rmp_serde::to_vec_named(&Outcome::Tool(ToolOutput {
        text: String::new(),
    }))
    .unwrap();
    let wrong = run_module(&module(&tool_answer, ""), &panel(None));
    assert!(
        matches!(wrong, Err(EngineError::WrongOutcome { expected: "panel" })),
        "{wrong:?}"
    );
    let button = Node::Button {
        id: "same".into(),
        label: String::new(),
    };
    let duplicate = rmp_serde::to_vec_named(&Outcome::Panel(View {
        nodes: vec![button.clone(), button],
    }))
    .unwrap();
    let invalid = run_module(&module(&duplicate, ""), &panel(None));
    assert!(matches!(invalid, Err(EngineError::View(_))), "{invalid:?}");
    let oversized = format!(
        r#"(module (memory (export "memory") 1)
            (func (export "a206_buffer") (param i32) (result i32) i32.const 0)
            (func (export "a206_invoke") (param i32) (result i64) i64.const {}))"#,
        Limits::default().output_bytes + 1
    );
    assert!(matches!(
        run_module(&oversized, &panel(None)),
        Err(EngineError::OutputTooLarge { .. })
    ));
}

#[test]
fn abi_misuse_traps() {
    let take = module(b"", "(drop (call $take (i32.const 0) (i32.const 1)))");
    assert!(matches!(
        run_module(&take, &panel(None)),
        Err(EngineError::Abi(AbiViolation::NoPendingReply))
    ));
    let out_of_bounds = module(b"", "(drop (call $call (i32.const 65535) (i32.const 64)))");
    assert!(matches!(
        run_module(&out_of_bounds, &panel(None)),
        Err(EngineError::Abi(AbiViolation::RequestBounds))
    ));
    let undecodable = module(b"\xc1", "(drop (call $call (i32.const 0) (i32.const 1)))");
    assert!(matches!(
        run_module(&undecodable, &panel(None)),
        Err(EngineError::Abi(AbiViolation::RequestDecode(_)))
    ));
}

#[test]
fn ambient_imports_are_denied() {
    let wasi = r#"(module (import "wasi_snapshot_preview1" "fd_write" (func)) (memory (export "memory") 1))"#;
    assert!(matches!(
        run_module(wasi, &panel(None)),
        Err(EngineError::Instantiate(_))
    ));
}

#[test]
fn worker_relays_host_calls_over_frames() {
    let name = unique("entry");
    let start = worker::Start {
        extension: "agent206.workspace".into(),
        module: None,
        invocation: tool("list_directory", &[]),
        limits: Limits::default(),
    };
    let mut input = a206_protocol::encode(&start).unwrap();
    input.extend(
        a206_protocol::encode(&HostReply::Directory(vec![entry(
            &name,
            EntryKind::Directory,
        )]))
        .unwrap(),
    );
    let output = worker::serve(std::io::Cursor::new(input), Vec::new()).unwrap();
    let frames: Vec<worker::Frame> = a206_protocol::Decoder::default().feed(&output).unwrap();
    assert_eq!(
        frames,
        vec![
            worker::Frame::Host(HostRequest::ListDirectory { path: ".".into() }),
            worker::Frame::Done(Ok(Outcome::Tool(ToolOutput {
                text: format!("{name}/")
            }))),
        ]
    );
}

#[test]
fn worker_runs_a_supplied_module_for_an_unbundled_id() {
    let workspace = crate::builtin("agent206.workspace").unwrap().unwrap();
    let name = unique("entry");
    let start = worker::Start {
        extension: unique("installed."),
        module: Some(serde_bytes::ByteBuf::from(workspace.wasm.to_vec())),
        invocation: tool("list_directory", &[]),
        limits: Limits::default(),
    };
    let mut input = a206_protocol::encode(&start).unwrap();
    input.extend(
        a206_protocol::encode(&HostReply::Directory(vec![entry(
            &name,
            EntryKind::Directory,
        )]))
        .unwrap(),
    );
    let output = worker::serve(std::io::Cursor::new(input), Vec::new()).unwrap();
    let frames: Vec<worker::Frame> = a206_protocol::Decoder::default().feed(&output).unwrap();
    assert_eq!(
        frames.last(),
        Some(&worker::Frame::Done(Ok(Outcome::Tool(ToolOutput {
            text: format!("{name}/")
        }))))
    );
    // Bytes that are not a module fail as the extension's invalid request.
    let start = worker::Start {
        module: Some(serde_bytes::ByteBuf::from(unique("not wasm").into_bytes())),
        ..start
    };
    let output = worker::serve(
        std::io::Cursor::new(a206_protocol::encode(&start).unwrap()),
        Vec::new(),
    )
    .unwrap();
    let frames: Vec<worker::Frame> = a206_protocol::Decoder::default().feed(&output).unwrap();
    let [worker::Frame::Done(Err(report))] = frames.as_slice() else {
        panic!("expected one failed result, got {frames:?}");
    };
    assert_eq!(report.kind, ErrorKind::InvalidRequest);
}

#[test]
fn worker_reports_failures_as_structured_values() {
    let start = worker::Start {
        extension: unique("missing."),
        module: None,
        invocation: panel(None),
        limits: Limits::default(),
    };
    let output = worker::serve(
        std::io::Cursor::new(a206_protocol::encode(&start).unwrap()),
        Vec::new(),
    )
    .unwrap();
    let frames: Vec<worker::Frame> = a206_protocol::Decoder::default().feed(&output).unwrap();
    let [worker::Frame::Done(Err(report))] = frames.as_slice() else {
        panic!("expected one failed result, got {frames:?}");
    };
    assert_eq!(report.kind, ErrorKind::NotFound);
    // A channel that closes mid-invocation is a worker error, not a result.
    let truncated = worker::Start {
        extension: "agent206.workspace".into(),
        module: None,
        invocation: tool("list_directory", &[]),
        limits: Limits::default(),
    };
    let output = worker::serve(
        std::io::Cursor::new(a206_protocol::encode(&truncated).unwrap()),
        Vec::new(),
    )
    .unwrap();
    let frames: Vec<worker::Frame> = a206_protocol::Decoder::default().feed(&output).unwrap();
    let Some(worker::Frame::Done(Err(report))) = frames.last() else {
        panic!("expected a failed result, got {frames:?}");
    };
    assert_eq!(report.kind, ErrorKind::Unavailable);
}

#[test]
fn bundled_skill_guest_only_uses_the_scoped_skill_imports() {
    let builtin = builtin("agent206.skills").unwrap().unwrap();
    let (bridge, seen) =
        mock(|_: &HostRequest| HostReply::File(b"complete approved instructions".to_vec().into()));
    let (result, _) = engine::run(
        builtin.wasm,
        &Invocation {
            abi: ABI,
            call: Call::Tool {
                name: "skill_info".into(),
                args: [("name".into(), Value::Str("notes".into()))].into(),
            },
        },
        Limits::default(),
        bridge,
    );
    assert!(
        matches!(result.unwrap(), Outcome::Tool(output) if output.text == "complete approved instructions")
    );
    assert_eq!(
        *seen.borrow(),
        vec![HostRequest::SkillRead {
            name: "notes".into(),
            path: None
        }]
    );
}

fn memory() -> Builtin {
    builtin("agent206.memory").unwrap().unwrap()
}
/// A workspace of far more memory notes than one search may read, so the
/// guest's own bounds stop it; each `ReadFile` returns `text(index)`.
fn memory_corpus(
    text: impl Fn(usize) -> String + 'static,
) -> impl FnMut(&HostRequest) -> HostReply + 'static {
    const NOTES: usize = 400;
    move |request| match request {
        HostRequest::ListDirectory { path } if path == "." => HostReply::Directory(vec![
            entry("MEMORY.md", EntryKind::File),
            entry("memory", EntryKind::Directory),
        ]),
        HostRequest::ListDirectory { .. } => HostReply::Directory(
            (0..NOTES)
                .map(|index| DirEntry {
                    name: format!("{index:04}.md"),
                    kind: EntryKind::File,
                    // Unknown sizes: the guest must bound what it reads itself.
                    size: None,
                })
                .collect(),
        ),
        HostRequest::ReadFile { path } => {
            let index = path
                .trim_start_matches("memory/")
                .trim_end_matches(".md")
                .parse()
                .unwrap_or(NOTES);
            HostReply::File(text(index).into_bytes().into())
        }
        other => panic!("memory_search sent {other:?}"),
    }
}

/// The costliest inputs for the guest's loading and ranking still finish
/// inside the default fuel, host-call and output limits: the guest's own
/// work meter stops it first and says how much it searched.
#[test]
fn memory_search_at_its_bounds_fits_the_invocation_limits() {
    // Non-ASCII terms of every length from 1 to 24 characters, so no word
    // is ruled out by its length and odd-length ASCII words are compared.
    let query = (1..=24)
        .map(|n| format!("{}q", "ж".repeat(n - 1)))
        .collect::<Vec<_>>()
        .join(" ");
    let note = |unit: &str, index: usize| unit.repeat(8 * 1024 / unit.len() + index % 3);
    // Each note about 8 KiB.
    type Generate = Box<dyn Fn(usize) -> String>;
    let corpora: Vec<(&str, Generate)> = vec![
        // The most words per byte.
        (
            "short words",
            Box::new(move |i| note("ab Cd e ij kl mn op q\n", i)),
        ),
        // The most lines per byte.
        ("blank lines", Box::new(move |i| note("\n", i))),
        // A separator character that is not ASCII between every letter.
        ("wide separators", Box::new(move |i| note("a—", i))),
        // Single non-ASCII characters, each its own word.
        (
            "distinct characters",
            Box::new(|i| {
                (0..2700)
                    .filter_map(|k| char::from_u32(0xc0 + ((i * 2700 + k) % 0x1f00) as u32))
                    .flat_map(|c| [c, ' '])
                    .collect()
            }),
        ),
        // Cyrillic words of the query's lengths, lowercased to compare.
        (
            "cased words",
            Box::new(|i| {
                (0..900)
                    .map(|k| "Жжэ".chars().cycle().skip(i + k).take(1 + (i * 7 + k) % 24))
                    .map(String::from_iter)
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
        ),
        (
            "mixed scripts",
            Box::new(move |i| note("ÄÖÜ ÆØÅ Ωμέγα Жук 日本 語 ", i)),
        ),
    ];
    for (label, text) in corpora {
        let (bridge, seen) = mock(memory_corpus(text));
        let (outcome, _) = engine::run(
            memory().wasm,
            &tool(
                "memory_search",
                &[
                    ("query", Value::Str(query.clone())),
                    ("max_results", Value::Int(20)),
                ],
            ),
            Limits::default(),
            bridge,
        );
        let Ok(Outcome::Tool(output)) = outcome else {
            panic!("{label}: expected tool output, got {outcome:?}");
        };
        assert!(
            output.text.contains("[stopped at a bound after "),
            "{label}: {}",
            output.text
        );
        assert!(
            seen.borrow().iter().all(|r| matches!(
                r,
                HostRequest::ListDirectory { .. } | HostRequest::ReadFile { .. }
            )),
            "{label}: searching only lists and reads"
        );
    }
}

#[test]
fn memory_tools_refuse_paths_before_any_host_call() {
    let note = || ("text", Value::Str(unique("note")));
    for (name, args) in [
        (
            "memory_get",
            vec![("path", Value::Str(format!("src/{}.md", unique("s"))))],
        ),
        (
            "memory_save",
            vec![("path", Value::Str("MEMORY.md".into())), note()],
        ),
        (
            "memory_save",
            vec![
                ("path", Value::Str(format!("memory/../{}.md", unique("s")))),
                note(),
            ],
        ),
    ] {
        let (bridge, seen) = mock(|request| panic!("host call {request:?}"));
        let (outcome, _) =
            engine::run(memory().wasm, &tool(name, &args), Limits::default(), bridge);
        let Ok(Outcome::Failed(error)) = outcome else {
            panic!("expected a refusal, got {outcome:?}");
        };
        assert_eq!(error.code, "not_memory_path");
        assert!(seen.borrow().is_empty());
    }
}

#[test]
fn memory_save_creates_a_note_through_write_calls() {
    let path = format!("memory/{}/{}.md", unique("d"), unique("n"));
    let text = unique("note ");
    let (bridge, seen) = mock(|request| match request {
        HostRequest::ReadFile { .. } => HostReply::Failed(HostFailure {
            code: HostErrorCode::NotFound,
            message: "no such file or directory".into(),
        }),
        _ => HostReply::Done,
    });
    let (outcome, _) = engine::run(
        memory().wasm,
        &tool(
            "memory_save",
            &[
                ("path", Value::Str(path.clone())),
                ("text", Value::Str(text.clone())),
            ],
        ),
        Limits::default(),
        bridge,
    );
    assert_eq!(
        outcome.unwrap(),
        Outcome::Tool(ToolOutput {
            text: format!("created {path}, lines 1-1")
        })
    );
    let parent = path.rsplit_once('/').unwrap().0.to_string();
    assert_eq!(
        *seen.borrow(),
        vec![
            HostRequest::ReadFile { path: path.clone() },
            HostRequest::CreateDirectory { path: parent },
            HostRequest::WriteFile {
                path,
                contents: format!("{text}\n").into_bytes().into(),
                overwrite: false,
            },
        ]
    );
}
