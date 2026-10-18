//! The bounded wasmi interpreter that runs one invocation of a guest.
//!
//! The engine links exactly the two ABI host functions and forwards each
//! request to a [`HostBridge`]. It never performs host I/O itself: in the
//! gateway's design the bridge relays requests to the parent process, which
//! authorizes them.
use a206_extension_abi::{
    Call, EXPORT_BUFFER, EXPORT_INVOKE, EXPORT_MEMORY, HostReply, HostRequest, IMPORT_HOST_CALL,
    IMPORT_HOST_TAKE, IMPORT_MODULE, Invocation, MAX_TRIGGER_CURSOR, Outcome, ViewError,
};
use serde::{Deserialize, Serialize};
use wasmi::{
    Caller, Config, Engine, Extern, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
    TrapCode,
};

/// Resource bounds for one invocation, chosen by the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub module_bytes: usize,
    pub memory_bytes: usize,
    pub fuel: u64,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub host_calls: u32,
    pub host_request_bytes: usize,
    pub host_reply_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            module_bytes: 16 * 1024 * 1024,
            memory_bytes: 16 * 1024 * 1024,
            fuel: 50_000_000,
            input_bytes: 1024 * 1024,
            output_bytes: 256 * 1024,
            host_calls: 256,
            host_request_bytes: 1024 * 1024,
            host_reply_bytes: 1024 * 1024,
        }
    }
}

/// Carries host requests out of the interpreter.
pub trait HostBridge {
    fn call(&mut self, request: HostRequest) -> Result<HostReply, BridgeError>;
}
/// Why the bridge to the authorizing host broke. The invocation cannot continue.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("the extension host channel failed")]
    Io(#[source] std::io::Error),
    #[error("the extension host sent an invalid frame")]
    Frame(#[source] a206_protocol::Error),
    #[error("the extension host closed the channel")]
    Closed,
}

/// A guest broke the calling convention; the call traps.
#[derive(Debug, thiserror::Error)]
pub enum AbiViolation {
    #[error("host request lies outside guest memory")]
    RequestBounds,
    #[error("host request exceeds {limit} bytes")]
    RequestTooLarge { limit: usize },
    #[error("host request is not a valid ABI value")]
    RequestDecode(#[source] rmp_serde::decode::Error),
    #[error("more than {limit} host calls in one invocation")]
    HostCalls { limit: u32 },
    #[error("`{IMPORT_HOST_TAKE}` called without a pending reply")]
    NoPendingReply,
    #[error("`{IMPORT_HOST_TAKE}` length does not match the pending reply")]
    TakeLength,
    #[error("host reply destination lies outside guest memory")]
    TakeBounds,
    #[error("the guest exports no memory")]
    NoMemory,
}

/// Why an invocation did not produce a valid outcome.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("extension module is {size} bytes; the limit is {limit}")]
    ModuleTooLarge { size: usize, limit: usize },
    #[error("extension module is not valid WASM")]
    Module(#[source] wasmi::Error),
    #[error(
        "extension module could not be instantiated; it may import something the host does not provide"
    )]
    Instantiate(#[source] wasmi::Error),
    #[error("extension does not export `{name}` with the ABI v2 signature")]
    MissingExport {
        name: &'static str,
        #[source]
        source: wasmi::Error,
    },
    #[error("extension does not export `{EXPORT_MEMORY}`")]
    MissingMemory,
    #[error("invocation input is {size} bytes; the limit is {limit}")]
    InputTooLarge { size: usize, limit: usize },
    #[error("invocation could not be encoded")]
    Encode(#[source] rmp_serde::encode::Error),
    #[error("guest input buffer lies outside its memory")]
    InputBounds(#[source] wasmi::errors::MemoryError),
    #[error("extension ran out of fuel")]
    OutOfFuel(#[source] wasmi::Error),
    #[error("extension trapped")]
    Trap(#[source] wasmi::Error),
    #[error("extension broke the ABI")]
    Abi(#[source] AbiViolation),
    #[error("extension host call failed")]
    Bridge(#[source] BridgeError),
    #[error("extension output is {size} bytes; the limit is {limit}")]
    OutputTooLarge { size: usize, limit: usize },
    #[error("extension output lies outside its memory")]
    OutputBounds(#[source] wasmi::errors::MemoryError),
    #[error("extension output is not a valid ABI value")]
    Output(#[source] a206_protocol::Error),
    #[error("extension answered a {expected} call with another kind of result")]
    WrongOutcome { expected: &'static str },
    #[error("extension panel is invalid")]
    View(#[source] ViewError),
    #[error("extension trigger kept more than {MAX_TRIGGER_CURSOR} bytes for its next check")]
    Cursor,
}

/// Why a host function trapped. The interpreter only carries an opaque
/// trap, so the typed cause waits in the store until the call returns.
#[derive(Debug)]
enum HostTrap {
    Abi(AbiViolation),
    Bridge(BridgeError),
}

struct State<B> {
    limits: StoreLimits,
    bounds: Limits,
    bridge: B,
    calls: u32,
    pending: Option<Vec<u8>>,
    trap: Option<HostTrap>,
}

/// Guest addresses and lengths are unsigned 32-bit values.
fn unsigned(value: i32) -> usize {
    value as u32 as usize
}
fn memory<B>(caller: &Caller<'_, State<B>>) -> Result<Memory, HostTrap> {
    match caller.get_export(EXPORT_MEMORY) {
        Some(Extern::Memory(memory)) => Ok(memory),
        _ => Err(HostTrap::Abi(AbiViolation::NoMemory)),
    }
}
fn trap<B>(
    caller: &mut Caller<'_, State<B>>,
    result: Result<i32, HostTrap>,
) -> Result<i32, wasmi::Error> {
    result.map_err(|trap| {
        caller.data_mut().trap = Some(trap);
        wasmi::Error::new("extension host call trapped")
    })
}

fn host_call<B: HostBridge>(
    mut caller: Caller<'_, State<B>>,
    pointer: i32,
    len: i32,
) -> Result<i32, wasmi::Error> {
    let result = relay(&mut caller, pointer, len);
    trap(&mut caller, result)
}
fn relay<B: HostBridge>(
    caller: &mut Caller<'_, State<B>>,
    pointer: i32,
    len: i32,
) -> Result<i32, HostTrap> {
    let bounds = caller.data().bounds;
    let len = unsigned(len);
    if len > bounds.host_request_bytes {
        return Err(HostTrap::Abi(AbiViolation::RequestTooLarge {
            limit: bounds.host_request_bytes,
        }));
    }
    caller.data_mut().calls += 1;
    if caller.data().calls > bounds.host_calls {
        return Err(HostTrap::Abi(AbiViolation::HostCalls {
            limit: bounds.host_calls,
        }));
    }
    let mut request = vec![0; len];
    memory(caller)?
        .read(&*caller, unsigned(pointer), &mut request)
        .map_err(|_| HostTrap::Abi(AbiViolation::RequestBounds))?;
    let request: HostRequest = rmp_serde::from_slice(&request)
        .map_err(|e| HostTrap::Abi(AbiViolation::RequestDecode(e)))?;
    let reply = caller
        .data_mut()
        .bridge
        .call(request)
        .map_err(HostTrap::Bridge)?;
    // The relay bounds what it sends; an oversized reply is reported as a
    // broken channel, never truncated.
    let reply = rmp_serde::to_vec_named(&reply)
        .ok()
        .filter(|reply| reply.len() <= bounds.host_reply_bytes)
        .ok_or(HostTrap::Bridge(BridgeError::Frame(
            a206_protocol::Error::TooLarge,
        )))?;
    let len = reply.len() as i32;
    caller.data_mut().pending = Some(reply);
    Ok(len)
}

fn host_take<B: HostBridge>(
    mut caller: Caller<'_, State<B>>,
    pointer: i32,
    len: i32,
) -> Result<i32, wasmi::Error> {
    let result = take(&mut caller, pointer, len);
    trap(&mut caller, result)
}
fn take<B>(caller: &mut Caller<'_, State<B>>, pointer: i32, len: i32) -> Result<i32, HostTrap> {
    let reply = caller
        .data_mut()
        .pending
        .take()
        .ok_or(HostTrap::Abi(AbiViolation::NoPendingReply))?;
    if reply.len() != unsigned(len) {
        return Err(HostTrap::Abi(AbiViolation::TakeLength));
    }
    memory(caller)?
        .write(&mut *caller, unsigned(pointer), &reply)
        .map_err(|_| HostTrap::Abi(AbiViolation::TakeBounds))?;
    Ok(len)
}

/// Run one invocation in a fresh instance and validate its outcome. The
/// bridge is handed back so its channel can carry the result.
pub fn run<B: HostBridge + 'static>(
    wasm: &[u8],
    invocation: &Invocation,
    bounds: Limits,
    bridge: B,
) -> (Result<Outcome, EngineError>, B) {
    let mut config = Config::default();
    config.consume_fuel(true);
    let engine = Engine::new(&config);
    let limits = StoreLimitsBuilder::new()
        .memory_size(bounds.memory_bytes)
        .instances(1)
        .memories(1)
        .tables(1)
        .table_elements(4096)
        .build();
    let mut store = Store::new(
        &engine,
        State {
            limits,
            bounds,
            bridge,
            calls: 0,
            pending: None,
            trap: None,
        },
    );
    store.limiter(|state: &mut State<B>| &mut state.limits);
    let result = execute(&engine, &mut store, wasm, invocation);
    (result, store.into_data().bridge)
}

fn execute<B: HostBridge + 'static>(
    engine: &Engine,
    store: &mut Store<State<B>>,
    wasm: &[u8],
    invocation: &Invocation,
) -> Result<Outcome, EngineError> {
    let bounds = store.data().bounds;
    if wasm.len() > bounds.module_bytes {
        return Err(EngineError::ModuleTooLarge {
            size: wasm.len(),
            limit: bounds.module_bytes,
        });
    }
    let module = Module::new(engine, wasm).map_err(EngineError::Module)?;
    store.set_fuel(bounds.fuel).map_err(EngineError::Trap)?;
    let mut linker = Linker::<State<B>>::new(engine);
    for (name, function) in [
        (
            IMPORT_HOST_CALL,
            wasmi::Func::wrap(&mut *store, host_call::<B>),
        ),
        (
            IMPORT_HOST_TAKE,
            wasmi::Func::wrap(&mut *store, host_take::<B>),
        ),
    ] {
        linker
            .define(IMPORT_MODULE, name, function)
            .map_err(|e| EngineError::Instantiate(e.into()))?;
    }
    let instance = linker
        .instantiate_and_start(&mut *store, &module)
        .map_err(EngineError::Instantiate)?;
    let missing = |name| move |source| EngineError::MissingExport { name, source };
    let buffer = instance
        .get_typed_func::<i32, i32>(&*store, EXPORT_BUFFER)
        .map_err(missing(EXPORT_BUFFER))?;
    let invoke = instance
        .get_typed_func::<i32, i64>(&*store, EXPORT_INVOKE)
        .map_err(missing(EXPORT_INVOKE))?;
    let memory = instance
        .get_memory(&*store, EXPORT_MEMORY)
        .ok_or(EngineError::MissingMemory)?;

    let input = rmp_serde::to_vec_named(invocation).map_err(EngineError::Encode)?;
    if input.len() > bounds.input_bytes {
        return Err(EngineError::InputTooLarge {
            size: input.len(),
            limit: bounds.input_bytes,
        });
    }
    let len = input.len() as i32;
    let pointer = buffer
        .call(&mut *store, len)
        .map_err(|e| trapped(store, e))?;
    memory
        .write(&mut *store, unsigned(pointer), &input)
        .map_err(EngineError::InputBounds)?;
    let packed = invoke
        .call(&mut *store, len)
        .map_err(|e| trapped(store, e))? as u64;
    let (pointer, len) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
    if len > bounds.output_bytes {
        return Err(EngineError::OutputTooLarge {
            size: len,
            limit: bounds.output_bytes,
        });
    }
    let mut output = vec![0; len];
    memory
        .read(&*store, pointer, &mut output)
        .map_err(EngineError::OutputBounds)?;
    let outcome: Outcome = a206_protocol::decode_payload(&output).map_err(EngineError::Output)?;
    match (&invocation.call, &outcome) {
        (_, Outcome::Failed(_)) | (Call::Tool { .. }, Outcome::Tool(_)) => {}
        (Call::Trigger { .. }, Outcome::Trigger(output)) => {
            if output
                .cursor
                .as_ref()
                .is_some_and(|c| c.len() > MAX_TRIGGER_CURSOR)
            {
                return Err(EngineError::Cursor);
            }
        }
        (Call::Panel { .. }, Outcome::Panel(view)) => view.validate().map_err(EngineError::View)?,
        (Call::Tool { .. }, _) => return Err(EngineError::WrongOutcome { expected: "tool" }),
        (Call::Panel { .. }, _) => return Err(EngineError::WrongOutcome { expected: "panel" }),
        (Call::Trigger { .. }, _) => {
            return Err(EngineError::WrongOutcome {
                expected: "trigger",
            });
        }
    }
    Ok(outcome)
}

fn trapped<B>(store: &mut Store<State<B>>, error: wasmi::Error) -> EngineError {
    match store.data_mut().trap.take() {
        Some(HostTrap::Abi(violation)) => EngineError::Abi(violation),
        Some(HostTrap::Bridge(bridge)) => EngineError::Bridge(bridge),
        None if error.as_trap_code() == Some(TrapCode::OutOfFuel) => EngineError::OutOfFuel(error),
        None => EngineError::Trap(error),
    }
}
