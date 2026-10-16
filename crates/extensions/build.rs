fn main() {
    println!("cargo:rerun-if-changed=../../plugins/inspector/plugin.toml");
    // Bundle a genuine compiled WASM extension, not a native code bypass.
    let payload = rmp_serde::to_vec_named(&vec![("title", "Workspace inspector"), ("body", "Extensions run without ambient filesystem, network or vault access. Host capabilities must be granted explicitly.")]).expect("static plugin payload");
    let escaped: String = payload.iter().map(|b| format!("\\{b:02x}")).collect();
    let wat = format!(
        r#"(module (memory (export "memory") 1 16)
        (data (i32.const 0) "{escaped}")
        (func (export "run") (result i64) i64.const {}))"#,
        payload.len()
    );
    let wasm = wat::parse_str(wat).expect("builtin WASM");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo OUT_DIR"));
    std::fs::write(out.join("inspector.wasm"), wasm).expect("write builtin WASM");
}
