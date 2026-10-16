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
fn worker_reports_failures_as_structured_values() {
    let start = worker::Start {
        extension: unique("missing."),
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
