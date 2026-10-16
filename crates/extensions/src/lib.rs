//! Capability-free WASM execution. Host I/O is deliberately absent until brokered.
#![forbid(unsafe_code)]
use a206_core::{Capability, PluginSummary};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use wasmi::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
pub const ABI: u32 = 1;
pub const MAX_OUTPUT: usize = 256 * 1024;
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
    pub fn parse(text: &str) -> Result<Self> {
        let manifest: Self = toml::from_str(text)?;
        ensure!(manifest.abi == ABI, "unsupported extension ABI");
        ensure!(
            !manifest.id.is_empty()
                && manifest
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)),
            "invalid extension id"
        );
        ensure!(manifest.entry == "run", "unsupported entry point");
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
pub fn builtin() -> Result<(Manifest, &'static [u8])> {
    Ok((
        Manifest::parse(include_str!("../../../plugins/inspector/plugin.toml"))?,
        include_bytes!(concat!(env!("OUT_DIR"), "/inspector.wasm")),
    ))
}
/// ABI v1: run() -> packed (pointer << 32 | length), MessagePack key/value output.
/// Instantiation has no imports; a guest requesting WASI or host access fails closed.
pub fn execute(wasm: &[u8], fuel: u64) -> Result<Vec<(String, String)>> {
    ensure!(wasm.len() <= 16 * 1024 * 1024, "extension too large");
    let mut config = Config::default();
    config.consume_fuel(true);
    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm).context("invalid WASM")?;
    let limits = StoreLimitsBuilder::new()
        .memory_size(16 * 1024 * 1024)
        .instances(1)
        .memories(1)
        .tables(1)
        .table_elements(4096)
        .build();
    let mut store = Store::new(&engine, limits);
    store.limiter(|state: &mut StoreLimits| state);
    store.set_fuel(fuel)?;
    let linker = Linker::<StoreLimits>::new(&engine);
    let instance = linker.instantiate_and_start(&mut store, &module)?;
    let run = instance.get_typed_func::<(), i64>(&store, "run")?;
    let packed = run.call(&mut store, ())? as u64;
    let pointer = (packed >> 32) as usize;
    let length = (packed & 0xffff_ffff) as usize;
    ensure!(length <= MAX_OUTPUT, "extension output too large");
    let memory = instance
        .get_memory(&store, "memory")
        .context("missing memory export")?;
    let mut bytes = vec![0; length];
    memory.read(&store, pointer, &mut bytes)?;
    let output: Vec<(String, String)> = a206_protocol::decode_payload(&bytes)?;
    ensure!(output.len() <= 64, "too many extension fields");
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
