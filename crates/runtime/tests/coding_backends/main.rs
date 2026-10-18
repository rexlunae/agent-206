//! Coding backends and confined processes driven through the real OS sandbox.
//! The harness is custom because on Linux the gateway re-runs this binary as
//! its in-sandbox helper, and on both platforms the confinement probe runs as
//! this binary too.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod confined;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod sandboxed;

fn main() -> std::process::ExitCode {
    if let Some(result) = a206_sandbox::dispatch() {
        return result.expect("sandbox helper could not report to the gateway");
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if std::env::args().nth(1).as_deref() == Some(confined::PROBE) {
        confined::probe();
        return std::process::ExitCode::SUCCESS;
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(sandboxed::drives_both_agent_protocols_in_the_real_os_sandbox());
        runtime.block_on(confined::confines_this_executable_without_network_files_or_programs());
        println!("coding backends: ok");
    }
    std::process::ExitCode::SUCCESS
}
