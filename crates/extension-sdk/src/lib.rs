//! Guest SDK for Agent 206 WASM extensions (ABI v2).
//!
//! Implement [`Extension`] and call [`export!`] once; build the crate for
//! `wasm32-unknown-unknown` as a `cdylib`. Host calls in [`host`] are checked
//! per call against the grant of the invocation, so a guest must handle
//! [`HostErrorCode::Denied`] like any other failure.
//!
//! ```ignore
//! struct Hello;
//! impl a206_extension_sdk::Extension for Hello {}
//! a206_extension_sdk::export!(Hello);
//! ```
pub use a206_extension_abi::*;
use std::cell::RefCell;

/// The calls a guest answers. Unimplemented calls fail with `unknown_tool`
/// or `unknown_panel`; the host only routes names the manifest declares.
pub trait Extension {
    fn tool(name: &str, args: &Args) -> Result<ToolOutput, GuestError> {
        let _unused = args;
        Err(GuestError::new(
            "unknown_tool",
            format!("this extension has no tool `{name}`"),
        ))
    }
    fn panel(id: &str, event: Option<&UiEvent>) -> Result<View, GuestError> {
        let _unused = event;
        Err(GuestError::new(
            "unknown_panel",
            format!("this extension has no panel `{id}`"),
        ))
    }
}

/// Export the ABI entry points for an [`Extension`] implementation.
#[macro_export]
macro_rules! export {
    ($extension:ty) => {
        #[allow(unsafe_code)]
        #[unsafe(no_mangle)]
        pub extern "C" fn a206_buffer(len: i32) -> i32 {
            $crate::__private::buffer(len)
        }
        #[allow(unsafe_code)]
        #[unsafe(no_mangle)]
        pub extern "C" fn a206_invoke(len: i32) -> i64 {
            $crate::__private::invoke::<$extension>(len)
        }
    };
}

/// A string argument. The host has already checked declared types, so a
/// value of another type is reported rather than coerced.
pub fn string_arg<'a>(args: &'a Args, name: &str) -> Result<Option<&'a str>, GuestError> {
    match args.get(name) {
        None | Some(Value::Nil) => Ok(None),
        Some(Value::Str(text)) => Ok(Some(text)),
        Some(_) => Err(GuestError::new(
            "invalid_argument",
            format!("`{name}` must be a string"),
        )),
    }
}

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Guest addresses fit in 32 bits; the host reads them as unsigned.
fn address(pointer: *const u8) -> i32 {
    pointer as usize as u32 as i32
}

#[doc(hidden)]
pub mod __private {
    use super::*;

    pub fn buffer(len: i32) -> i32 {
        let len = usize::try_from(len).unwrap_or(0);
        INPUT.with_borrow_mut(|input| {
            input.clear();
            input.resize(len, 0);
            address(input.as_ptr())
        })
    }

    pub fn invoke<E: Extension>(len: i32) -> i64 {
        let invocation = INPUT.with_borrow(|input| {
            let bytes = usize::try_from(len)
                .ok()
                .and_then(|len| input.get(..len))
                .unwrap_or_default();
            rmp_serde::from_slice::<Invocation>(bytes)
        });
        let outcome = match invocation {
            Err(_) => Outcome::Failed(GuestError::new(
                "invalid_invocation",
                "the host sent an invocation this guest cannot decode",
            )),
            Ok(invocation) if invocation.abi != ABI => Outcome::Failed(GuestError::new(
                "unsupported_abi",
                format!("this guest implements ABI {ABI}"),
            )),
            Ok(Invocation {
                call: Call::Tool { name, args },
                ..
            }) => E::tool(&name, &args).map_or_else(Outcome::Failed, Outcome::Tool),
            Ok(Invocation {
                call: Call::Panel { id, event },
                ..
            }) => E::panel(&id, event.as_ref()).map_or_else(Outcome::Failed, Outcome::Panel),
        };
        // An encoding failure leaves an empty output, which the host rejects
        // as an ABI violation instead of guessing at the guest's intent.
        let encoded = rmp_serde::to_vec_named(&outcome).unwrap_or_default();
        OUTPUT.with_borrow_mut(|output| {
            *output = encoded;
            ((address(output.as_ptr()) as u32 as i64) << 32) | output.len() as i64
        })
    }
}

/// Host calls. Each one is authorized by the gateway for this invocation.
pub mod host {
    use super::*;
    use serde_bytes::ByteBuf;

    #[cfg(target_arch = "wasm32")]
    #[allow(unsafe_code)]
    mod ffi {
        // `safe fn`: the host only reads or writes the guest memory range
        // passed to it and traps on any out-of-bounds range.
        #[link(wasm_import_module = "a206")]
        unsafe extern "C" {
            pub safe fn host_call(pointer: i32, len: i32) -> i32;
            pub safe fn host_take(pointer: i32, len: i32) -> i32;
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn transport(request: &[u8]) -> Result<Vec<u8>, HostFailure> {
        let len = i32::try_from(request.len()).map_err(|_| too_large())?;
        let reply_len = ffi::host_call(address(request.as_ptr()), len);
        let mut reply = vec![0; usize::try_from(reply_len).map_err(|_| unexpected())?];
        ffi::host_take(address(reply.as_mut_ptr()), reply_len);
        Ok(reply)
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn transport(_request: &[u8]) -> Result<Vec<u8>, HostFailure> {
        Err(HostFailure {
            code: HostErrorCode::Unsupported,
            message: "host calls are only available inside the Agent 206 WASM host".into(),
        })
    }

    fn too_large() -> HostFailure {
        HostFailure {
            code: HostErrorCode::TooLarge,
            message: "host request is too large".into(),
        }
    }
    fn unexpected() -> HostFailure {
        HostFailure {
            code: HostErrorCode::Other,
            message: "the host sent an unexpected reply".into(),
        }
    }

    /// Send one request. A `Failed` reply is returned as `Err`.
    pub fn call(request: &HostRequest) -> Result<HostReply, HostFailure> {
        let bytes = rmp_serde::to_vec_named(request).map_err(|_| too_large())?;
        match rmp_serde::from_slice(&transport(&bytes)?).map_err(|_| unexpected())? {
            HostReply::Failed(failure) => Err(failure),
            reply => Ok(reply),
        }
    }

    /// Read a file relative to the invocation's workspace.
    pub fn read_file(path: &str) -> Result<Vec<u8>, HostFailure> {
        match call(&HostRequest::ReadFile { path: path.into() })? {
            HostReply::File(bytes) => Ok(bytes.into_vec()),
            _ => Err(unexpected()),
        }
    }
    /// List a directory relative to the invocation's workspace.
    pub fn list_directory(path: &str) -> Result<Vec<DirEntry>, HostFailure> {
        match call(&HostRequest::ListDirectory { path: path.into() })? {
            HostReply::Directory(entries) => Ok(entries),
            _ => Err(unexpected()),
        }
    }
    /// This extension's persisted value for `key`.
    pub fn state_get(key: &str) -> Result<Option<Vec<u8>>, HostFailure> {
        match call(&HostRequest::StateGet { key: key.into() })? {
            HostReply::State(value) => Ok(value.map(ByteBuf::into_vec)),
            _ => Err(unexpected()),
        }
    }
    /// Store (`Some`) or delete (`None`) this extension's value for `key`.
    pub fn state_put(key: &str, value: Option<&[u8]>) -> Result<(), HostFailure> {
        let value = value.map(|bytes| ByteBuf::from(bytes.to_vec()));
        match call(&HostRequest::StatePut {
            key: key.into(),
            value,
        })? {
            HostReply::Done => Ok(()),
            _ => Err(unexpected()),
        }
    }
    /// Record a diagnostic in the gateway's log.
    pub fn log(level: LogLevel, message: &str) -> Result<(), HostFailure> {
        match call(&HostRequest::Log {
            level,
            message: message.into(),
        })? {
            HostReply::Done => Ok(()),
            _ => Err(unexpected()),
        }
    }
}
