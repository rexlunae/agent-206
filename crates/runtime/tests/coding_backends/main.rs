//! Coding backends driven through the real OS sandbox. The harness is custom
//! because on Linux the gateway re-runs this binary as its in-sandbox helper.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod sandboxed;

fn main() -> std::process::ExitCode {
    if let Some(result) = a206_sandbox::dispatch() {
        return result.expect("sandbox helper could not report to the gateway");
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(sandboxed::drives_both_agent_protocols_in_the_real_os_sandbox());
        println!("coding backends: ok");
    }
    std::process::ExitCode::SUCCESS
}
