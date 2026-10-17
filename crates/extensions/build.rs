//! Compile the bundled extensions to WASM with the guest SDK, so builtins are
//! genuine `wasm32-unknown-unknown` modules rather than native code.
use std::{path::PathBuf, process::Command};

const BUILTINS: &[(&str, &str)] = &[
    ("a206-plugin-skills", "a206_plugin_skills"),
    ("a206-plugin-workspace", "a206_plugin_workspace"),
    ("a206-plugin-git", "a206_plugin_git"),
    ("a206-plugin-web", "a206_plugin_web"),
];

fn main() {
    for path in [
        "../../plugins",
        "../extension-sdk",
        "../extension-abi",
        "../../Cargo.lock",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let target_dir = out.join("guest");
    let mut command = Command::new(cargo);
    command
        .current_dir(&workspace)
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .arg("--target-dir")
        .arg(&target_dir);
    for (package, _) in BUILTINS {
        command.args(["-p", package]);
    }
    // The outer build's flags, wrappers and target belong to the host build.
    for variable in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_PRIMARY_PACKAGE",
    ] {
        command.env_remove(variable);
    }
    // Small, reproducible guests: no machine paths in panic locations.
    let home = std::env::var("CARGO_HOME")
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| format!("{home}/.cargo"))
        })
        .unwrap_or_default();
    let mut flags = vec![format!("--remap-path-prefix={}=/a206", workspace.display())];
    if !home.is_empty() {
        flags.push(format!("--remap-path-prefix={home}=/cargo"));
    }
    command
        .env("CARGO_ENCODED_RUSTFLAGS", flags.join("\x1f"))
        .env("CARGO_PROFILE_RELEASE_OPT_LEVEL", "s")
        .env("CARGO_PROFILE_RELEASE_LTO", "true")
        .env("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "1")
        .env("CARGO_PROFILE_RELEASE_PANIC", "abort")
        .env("CARGO_PROFILE_RELEASE_STRIP", "true")
        .env("CARGO_PROFILE_RELEASE_DEBUG", "0");
    let status = command
        .status()
        .expect("run cargo for the bundled extensions");
    assert!(
        status.success(),
        "building the bundled WASM extensions failed; install the target with `rustup target add wasm32-unknown-unknown`"
    );
    for (_, artifact) in BUILTINS {
        let built = target_dir.join(format!("wasm32-unknown-unknown/release/{artifact}.wasm"));
        std::fs::copy(&built, out.join(format!("{artifact}.wasm"))).expect("copy bundled WASM");
    }
}
