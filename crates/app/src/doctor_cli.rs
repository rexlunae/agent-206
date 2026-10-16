//! `agent206 doctor`: print the shared read-only health report.
use a206_runtime::doctor::{DoctorReport, Health};
use anyhow::{Result, bail};
use std::path::Path;

fn label(health: Health) -> &'static str {
    match health {
        Health::Ok => "ok",
        Health::Warning => "check",
        Health::Unknown => "unknown",
        Health::Problem => "fix",
    }
}

pub fn print(report: &DoctorReport) {
    for check in &report.checks {
        println!("{:<8} {}", label(check.health), check.finding);
        if let Some(error) = &check.error {
            println!("         cause: {error:#}");
        }
        if check.health != Health::Ok
            && let Some(recovery) = check.finding.recovery()
        {
            println!("         run: {recovery}");
        }
    }
}

/// Gather and evaluate the shared health report for this profile.
pub async fn report(root: &Path, remote: Option<String>) -> Result<DoctorReport> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let facts = a206_runtime::doctor::gather(root, remote, now).await;
    Ok(a206_runtime::doctor::evaluate(&facts))
}

pub async fn run(root: &Path, remote: Option<String>, toml: bool) -> Result<()> {
    let report = report(root, remote).await?;
    if toml {
        print!("{}", toml::to_string_pretty(&report)?);
    } else {
        print(&report);
    }
    if report.health() == Health::Problem {
        bail!("health checks found problems to fix");
    }
    Ok(())
}
