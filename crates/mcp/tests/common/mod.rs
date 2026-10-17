//! Helpers shared by this crate's integration tests.

pub const FIXTURE: &str = env!("CARGO_BIN_EXE_a206-mcp-fixture");

pub fn arguments(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let serde_json::Value::Object(map) = value else {
        panic!("arguments are an object")
    };
    map
}
