#![cfg(unix)]
//! `doctor`, `import` and `setup` through the real binary, against a real
//! gateway and broker in private test profiles.
#[allow(dead_code)]
mod support;
use a206_runtime::doctor::{DoctorReport, Finding};
use anyhow::{Context, Result, ensure};
use std::path::Path;
use support::{App, binary, capture};

fn random() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A RustyClaw settings directory with one hosted and one local provider.
fn rustyclaw(dir: &Path) -> Result<std::path::PathBuf> {
    let root = dir.join("rustyclaw");
    std::fs::create_dir_all(root.join("workspace"))?;
    let port = 1024 + (uuid::Uuid::new_v4().as_u128() % 50_000) as u16;
    std::fs::write(
        root.join("config.toml"),
        format!(
            r#"
clawhub_token = "{token}"

[model]
provider = "openai"
model = "{model}"

[[custom_providers]]
id = "{id}"
base_url = "http://127.0.0.1:{port}/v1"

[[messengers]]
name = "bot"
token = "{token}"
"#,
            token = random(),
            model = random(),
            id = random(),
        ),
    )?;
    std::fs::write(root.join("workspace").join("SOUL.md"), random())?;
    std::fs::create_dir_all(root.join("credentials"))?;
    std::fs::write(root.join("credentials").join("secrets.json"), "{}")?;
    Ok(root)
}

#[tokio::test]
async fn doctor_reports_an_unconfigured_profile_without_creating_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("profile");
    let mut command = tokio::process::Command::new(binary()?);
    command.arg("--data-dir").arg(&root).arg("doctor");
    let output = capture(command).await?;
    ensure!(!output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    ensure!(stdout.contains("no gateway is configured"), "{stdout}");
    ensure!(stdout.contains("run: agent206 setup"), "{stdout}");
    ensure!(!root.exists(), "doctor created the profile");
    Ok(())
}

#[tokio::test]
async fn doctor_and_import_against_a_running_gateway() -> Result<()> {
    let app = App::start(tempfile::tempdir()?, Default::default()).await?;
    let report: DoctorReport = toml::from_str(&{
        let output = capture(app.command(&["doctor", "--toml"])).await?;
        // No vault and no providers are problems, so doctor fails.
        ensure!(!output.status.success());
        String::from_utf8(output.stdout)?
    })?;
    let found: Vec<_> = report.checks.iter().map(|c| &c.finding).collect();
    ensure!(
        found
            .iter()
            .any(|f| matches!(f, Finding::GatewayRunning { .. }))
    );
    ensure!(found.contains(&&Finding::VaultMissing));
    ensure!(found.contains(&&Finding::NoProviders));
    ensure!(
        !app.root.join("vault").join("broker.sock").exists(),
        "doctor started the broker"
    );

    let source_dir = tempfile::tempdir()?;
    let source = rustyclaw(source_dir.path())?;
    let source_arg = source.to_str().unwrap_or_default();
    let config = std::fs::read(source.join("config.toml"))?;
    // A relative source resolves to the absolute paths the broker accepts.
    let mut relative = app.command(&["import", "rustyclaw", "rustyclaw", "--toml"]);
    relative.current_dir(source_dir.path());
    let listed: a206_import::Preview =
        toml::from_str(&support::checked(capture(relative).await?)?)?;
    let vault = listed.vault.context("vault not found")?;
    ensure!(vault.path.is_absolute(), "{vault:?}");
    ensure!(vault.plan == a206_import::VaultPlan::Import, "{vault:?}");
    let preview = app.cli(&["import", "rustyclaw", source_arg]).await?;
    ensure!(preview.contains("will be added"), "{preview}");
    ensure!(
        preview.contains("left inactive: 1 × messenger"),
        "{preview}"
    );
    ensure!(
        preview.contains("not imported yet: workspace instructions"),
        "{preview}"
    );
    ensure!(app.snapshot().await?.settings.providers.is_empty());
    ensure!(!app.root.join("imports").exists());

    let applied = app
        .cli(&["import", "rustyclaw", source_arg, "--apply", "--skip-vault"])
        .await?;
    ensure!(applied.contains("Receipt:"), "{applied}");
    ensure!(
        applied.contains("vault add OPENAI_API_KEY --destination https://"),
        "{applied}"
    );
    let providers = app.snapshot().await?.settings.providers;
    ensure!(providers.len() == 2, "{providers:?}");
    let openai = providers
        .iter()
        .find(|p| p.credential.as_deref() == Some("OPENAI_API_KEY"));
    ensure!(openai.is_some(), "{providers:?}");
    // A repeated import adds nothing and leaves the source as it was.
    let again = app
        .cli(&["import", "rustyclaw", source_arg, "--apply", "--skip-vault"])
        .await?;
    ensure!(again.contains("Nothing needed importing"), "{again}");
    ensure!(app.snapshot().await?.settings.providers.len() == 2);
    ensure!(std::fs::read_dir(app.root.join("imports"))?.count() == 1);
    ensure!(std::fs::read(source.join("config.toml"))? == config);
    Ok(())
}

#[tokio::test]
async fn setup_without_answers_pauses_before_creating_a_vault() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("profile");
    let mut command = tokio::process::Command::new(binary()?);
    command
        .arg("--data-dir")
        .arg(&root)
        .args(["setup", "--local"]);
    let output = capture(command).await?;
    ensure!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    // stdin is closed, so setup declines; the report then shows the gap
    // without failing setup itself.
    ensure!(stdout.contains("Gateway configured"), "{stdout}");
    ensure!(stdout.contains("Setup paused before the vault"), "{stdout}");
    ensure!(stdout.contains("no vault has been created"), "{stdout}");
    ensure!(!a206_broker::vault_path(&root).exists());
    // Import never starts the configured but stopped gateway, even to preview.
    let source_dir = tempfile::tempdir()?;
    let source = rustyclaw(source_dir.path())?;
    let mut import = tokio::process::Command::new(binary()?);
    import
        .arg("--data-dir")
        .arg(&root)
        .args(["import", "rustyclaw"])
        .arg(&source);
    let output = capture(import).await?;
    ensure!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    ensure!(stderr.contains("gateway is not running"), "{stderr}");
    let mut doctor = tokio::process::Command::new(binary()?);
    doctor
        .arg("--data-dir")
        .arg(&root)
        .args(["doctor", "--toml"]);
    let report: DoctorReport = toml::from_str(&String::from_utf8(capture(doctor).await?.stdout)?)?;
    ensure!(
        !report
            .checks
            .iter()
            .any(|c| matches!(c.finding, Finding::GatewayRunning { .. })),
        "import started the gateway"
    );
    let mut stop = tokio::process::Command::new(binary()?);
    stop.arg("--data-dir").arg(&root).args(["vault", "stop"]);
    support::checked(capture(stop).await?)?;
    Ok(())
}
