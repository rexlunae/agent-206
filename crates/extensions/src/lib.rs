//! Capability-free WASM execution. Host I/O is deliberately absent until brokered.
#![forbid(unsafe_code)]
use a206_core::{Capability, PluginSummary};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use wasmi::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
pub const ABI: u32 = 1;
pub const MAX_OUTPUT: usize = 256 * 1024;

/// Why a manifest was rejected or an extension failed to run.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    #[error("invalid extension manifest")]
    Manifest(#[source] toml::de::Error),
    #[error("unsupported extension ABI {0}")]
    Abi(u32),
    #[error("invalid extension id")]
    Id,
    #[error("unsupported entry point")]
    Entry,
    #[error("extension too large")]
    TooLarge,
    #[error("invalid WASM")]
    Module(#[source] wasmi::Error),
    #[error("extension failed")]
    Runtime(#[source] wasmi::Error),
    #[error("extension does not export `run`")]
    MissingRun(#[source] wasmi::Error),
    #[error("missing memory export")]
    MissingMemory,
    #[error("extension output is outside its memory")]
    OutputBounds(#[source] wasmi::errors::MemoryError),
    #[error("extension output too large")]
    OutputTooLarge,
    #[error("invalid extension output")]
    Output(#[source] a206_protocol::Error),
    #[error("too many extension fields")]
    TooManyFields,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub abi: u32,
    pub capabilities: BTreeSet<Capability>,
    pub entry: String,
}
impl Manifest {
    pub fn parse(text: &str) -> Result<Self, ExtensionError> {
        let manifest: Self = toml::from_str(text).map_err(ExtensionError::Manifest)?;
        if manifest.abi != ABI {
            return Err(ExtensionError::Abi(manifest.abi));
        }
        if manifest.id.is_empty()
            || !manifest
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        {
            return Err(ExtensionError::Id);
        }
        if manifest.entry != "run" {
            return Err(ExtensionError::Entry);
        }
        Ok(manifest)
    }
    pub fn summary(&self) -> PluginSummary {
        PluginSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            capabilities: self.capabilities.clone(),
            enabled: true,
        }
    }
}
pub fn builtin() -> Result<(Manifest, &'static [u8]), ExtensionError> {
    Ok((
        Manifest::parse(include_str!("../../../plugins/inspector/plugin.toml"))?,
        include_bytes!(concat!(env!("OUT_DIR"), "/inspector.wasm")),
    ))
}
/// ABI v1: run() -> packed (pointer << 32 | length), MessagePack key/value output.
/// Instantiation has no imports; a guest requesting WASI or host access fails closed.
pub fn execute(wasm: &[u8], fuel: u64) -> Result<Vec<(String, String)>, ExtensionError> {
    if wasm.len() > 16 * 1024 * 1024 {
        return Err(ExtensionError::TooLarge);
    }
    let mut config = Config::default();
    config.consume_fuel(true);
    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm).map_err(ExtensionError::Module)?;
    let limits = StoreLimitsBuilder::new()
        .memory_size(16 * 1024 * 1024)
        .instances(1)
        .memories(1)
        .tables(1)
        .table_elements(4096)
        .build();
    let mut store = Store::new(&engine, limits);
    store.limiter(|state: &mut StoreLimits| state);
    store.set_fuel(fuel).map_err(ExtensionError::Runtime)?;
    let linker = Linker::<StoreLimits>::new(&engine);
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .map_err(ExtensionError::Runtime)?;
    let run = instance
        .get_typed_func::<(), i64>(&store, "run")
        .map_err(ExtensionError::MissingRun)?;
    let packed = run.call(&mut store, ()).map_err(ExtensionError::Runtime)? as u64;
    let pointer = (packed >> 32) as usize;
    let length = (packed & 0xffff_ffff) as usize;
    if length > MAX_OUTPUT {
        return Err(ExtensionError::OutputTooLarge);
    }
    let memory = instance
        .get_memory(&store, "memory")
        .ok_or(ExtensionError::MissingMemory)?;
    let mut bytes = vec![0; length];
    memory
        .read(&store, pointer, &mut bytes)
        .map_err(ExtensionError::OutputBounds)?;
    let output: Vec<(String, String)> =
        a206_protocol::decode_payload(&bytes).map_err(ExtensionError::Output)?;
    if output.len() > 64 {
        return Err(ExtensionError::TooManyFields);
    }
    Ok(output)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundled_plugin_is_executable() {
        let (_, wasm) = builtin().unwrap();
        assert!(!execute(wasm, 100_000).unwrap().is_empty());
    }
    #[test]
    fn fuel_stops_untrusted_loop() {
        assert!(execute(br#"(module (memory (export "memory") 1) (func (export "run") (result i64) (loop $l br $l) i64.const 0))"#, 100).is_err());
    }
    #[test]
    fn ambient_imports_are_denied() {
        assert!(execute(br#"(module (import "wasi_snapshot_preview1" "fd_write" (func)) (memory (export "memory") 1) (func (export "run") (result i64) i64.const 0))"#, 100).is_err());
    }
}
