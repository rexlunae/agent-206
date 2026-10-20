//! `agent206 doctor`: print the shared health report, and with `--repair`
//! offer the shared repairs one by one.
use a206_runtime::doctor::{DoctorReport, Health};
use anyhow::{Result, bail};
use std::io::BufRead;
use std::path::Path;

/// Ask a yes/no question; a closed input answers no.
pub(crate) fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    print!("{question} [Y/n] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer)? == 0 {
        println!();
        return Ok(false);
    }
    let answer = answer.trim();
    Ok(answer.is_empty() || answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

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

async fn facts(root: &Path, remote: Option<String>) -> Result<a206_runtime::doctor::Facts> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    Ok(a206_runtime::doctor::gather(root, remote, now).await)
}

/// Gather and evaluate the shared health report for this profile.
pub async fn report(root: &Path, remote: Option<String>) -> Result<DoctorReport> {
    Ok(a206_runtime::doctor::evaluate(&facts(root, remote).await?))
}

/// Offer each available repair, make the confirmed ones, then check again.
/// Fails if a repair failed or problems remain.
pub async fn repair(root: &Path, remote: Option<String>, yes: bool) -> Result<()> {
    let checked = report(root, remote.clone()).await?;
    print(&checked);
    let repairs = &checked.repairs;
    println!();
    if repairs.is_empty() {
        println!("No repairs are available; follow the steps above.");
    }
    let mut failures = 0;
    for repair in repairs {
        if !yes && !confirm(&format!("Repair: {repair}?"))? {
            println!("  skipped");
            continue;
        }
        match a206_runtime::doctor::repair(root, remote.clone(), repair).await {
            Ok(()) => println!("  done: {repair}"),
            Err(error) => {
                failures += 1;
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "doctor repair failed"
                );
                println!("  failed: {repair}");
                println!("  cause: {:#}", anyhow::Error::new(error));
            }
        }
    }
    if repairs.is_empty() {
        return finish(&checked, 0);
    }
    println!();
    println!("Health checks after repair:");
    let report = report(root, remote).await?;
    print(&report);
    finish(&report, failures)
}

fn finish(report: &DoctorReport, failures: usize) -> Result<()> {
    if failures > 0 {
        bail!("{failures} repair(s) failed");
    }
    if report.health() == Health::Problem {
        bail!("health checks found problems to fix");
    }
    Ok(())
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
