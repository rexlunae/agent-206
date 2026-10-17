//! The extension worker: a child process that runs one invocation and relays
//! each host call to its parent over stdin/stdout.
//!
//! The parent writes a [`Start`] frame, then answers every [`Frame::Host`]
//! with one [`HostReply`] frame; the worker ends with [`Frame::Done`]. Frames
//! use the bounded `a206-protocol` framing. The worker has no authority of
//! its own: it holds no workspace, state or network handles.
use crate::engine::{self, BridgeError, EngineError, HostBridge, Limits};
use a206_core::ErrorReport;
use a206_extension_abi::{HostReply, HostRequest, Invocation, Outcome};
use a206_protocol::HEADER;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{Read, Write};

/// What the parent asks the worker to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Start {
    /// The extension id: a bundled one when `module` is `None`.
    pub extension: String,
    /// An installed extension's module, verified against its pinned digest by
    /// the gateway. The worker has no access to the gateway's files.
    pub module: Option<serde_bytes::ByteBuf>,
    pub invocation: Invocation,
    pub limits: Limits,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Frame {
    Host(HostRequest),
    /// The invocation's result. A failure is a report: the worker's typed
    /// errors cannot cross the process boundary any other way.
    Done(Result<Outcome, ErrorReport>),
}

/// Why the worker could not run the requested extension.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("the bundled extensions are invalid")]
    Builtins(#[source] crate::ManifestError),
    #[error("extension `{0}` is not bundled with this worker")]
    Unknown(String),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// The channel to the parent failed, so no result can be delivered.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("could not read the extension request")]
    Start(#[source] BridgeError),
    #[error("could not deliver the extension result")]
    Done(#[source] BridgeError),
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T, BridgeError> {
    let eof = |error: std::io::Error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            BridgeError::Closed
        } else {
            BridgeError::Io(error)
        }
    };
    let mut header = [0; HEADER];
    reader.read_exact(&mut header).map_err(eof)?;
    let len = a206_protocol::payload_size(&header).map_err(BridgeError::Frame)?;
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload).map_err(eof)?;
    a206_protocol::decode_payload(&payload).map_err(BridgeError::Frame)
}
pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<(), BridgeError> {
    let frame = a206_protocol::encode(value).map_err(BridgeError::Frame)?;
    writer.write_all(&frame).map_err(BridgeError::Io)?;
    writer.flush().map_err(BridgeError::Io)
}

struct Pipe<R, W> {
    input: R,
    output: W,
}
impl<R: Read, W: Write> HostBridge for Pipe<R, W> {
    fn call(&mut self, request: HostRequest) -> Result<HostReply, BridgeError> {
        write_frame(&mut self.output, &Frame::Host(request))?;
        read_frame(&mut self.input)
    }
}

/// Serve one invocation from `input` to `output`, returning the output.
pub fn serve<R: Read + 'static, W: Write + 'static>(
    mut input: R,
    output: W,
) -> Result<W, WorkerError> {
    let start: Start = read_frame(&mut input).map_err(WorkerError::Start)?;
    let module = match &start.module {
        Some(module) => Ok(module.as_slice()),
        None => crate::builtin(&start.extension)
            .map_err(RunError::Builtins)
            .and_then(|builtin| builtin.ok_or_else(|| RunError::Unknown(start.extension.clone())))
            .map(|builtin| builtin.wasm),
    };
    let (result, mut pipe) = match module {
        Ok(wasm) => {
            let (result, pipe) = engine::run(
                wasm,
                &start.invocation,
                start.limits,
                Pipe { input, output },
            );
            (result.map_err(RunError::Engine), pipe)
        }
        Err(error) => (Err(error), Pipe { input, output }),
    };
    let result = result.map_err(|error| ErrorReport::capture(&error, crate::classify));
    write_frame(&mut pipe.output, &Frame::Done(result)).map_err(WorkerError::Done)?;
    Ok(pipe.output)
}
