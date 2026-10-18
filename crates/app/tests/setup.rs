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

#[tokio::test]
async fn claude_code_review_and_selected_apply_use_the_same_gateway_commands() -> Result<()> {
    let app = App::start(tempfile::tempdir()?, Default::default()).await?;
    let source = tempfile::tempdir()?;
    let private = random();
    let bytes = serde_json::to_vec(
        &serde_json::json!({"env":{"ANTHROPIC_BASE_URL":"https://example.test", "ANTHROPIC_AUTH_TOKEN":private}, "model":private, "permissions":{"allow":[private]}}),
    )?;
    std::fs::write(source.path().join("settings.json"), &bytes)?;
    let output = app
        .cli(&[
            "import",
            "review-claude-code",
            source.path().to_str().context("source path")?,
        ])
        .await?;
    ensure!(!output.contains(&private));
    let review: a206_core::imports::Review = toml::from_str(&output)?;
    ensure!(review.preview.tool == a206_core::imports::Tool::ClaudeCode);
    ensure!(app.snapshot().await?.settings.providers.is_empty());
    let receipt: a206_core::imports::Receipt = toml::from_str(
        &app.cli(&[
            "import",
            "apply-review",
            &review.id.to_string(),
            "--provider",
            "0",
        ])
        .await?,
    )?;
    ensure!(receipt.providers_added.len() == 1 && !receipt.vault_imported);
    let snapshot = app.snapshot().await?;
    ensure!(snapshot.settings.providers[0].api == a206_core::ProviderApi::Anthropic);
    ensure!(snapshot.settings.providers[0].base_url == "https://example.test/v1");
    ensure!(snapshot.settings.providers[0].credential.is_none());
    ensure!(snapshot.catalogs.is_empty());
    ensure!(std::fs::read(source.path().join("settings.json"))? == bytes);
    ensure!(!app.root.join("vault").exists());
    Ok(())
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
messenger_type = "telegram"
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
async fn held_import_reviews_apply_only_selected_indices_and_refuse_changed_sources() -> Result<()>
{
    let app = App::start(tempfile::tempdir()?, Default::default()).await?;
    let directory = tempfile::tempdir()?;
    let source = rustyclaw(directory.path())?;
    let source_arg = source.to_str().context("UTF-8 source path")?;
    let review: a206_core::imports::Review =
        toml::from_str(&app.cli(&["import", "review-rustyclaw", source_arg]).await?)?;
    ensure!(app.snapshot().await?.settings.providers.is_empty());
    let id = review.id.to_string();
    let receipt: a206_core::imports::Receipt = toml::from_str(
        &app.cli(&[
            "import",
            "apply-review",
            &id,
            "--provider",
            "1",
            "--messenger",
            "0",
        ])
        .await?,
    )?;
    ensure!(receipt.providers_added.len() == 1 && receipt.messengers_added == ["bot"]);
    ensure!(!receipt.vault_imported);
    let settings = app.snapshot().await?.settings;
    ensure!(
        settings.providers.len() == 1
            && settings.providers[0].base_url == review.preview.providers[1].base_url
    );
    ensure!(!settings.messengers[0].enabled && settings.messengers[0].credential.is_none());
    let reviewed: a206_core::imports::Review =
        toml::from_str(&app.cli(&["import", "review-rustyclaw", source_arg]).await?)?;
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(source.join("config.toml"))?
        .write_all(b"\n# changed\n")?;
    let rejected = capture(app.command(&[
        "import",
        "apply-review",
        &reviewed.id.to_string(),
        "--provider",
        "0",
    ]))
    .await?;
    ensure!(!rejected.status.success());
    ensure!(String::from_utf8(rejected.stderr)?.contains("source changed"));
    ensure!(app.snapshot().await?.settings.providers.len() == 1);
    Ok(())
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
        preview.contains("messenger bot (telegram \"bot\") will be added, disabled"),
        "{preview}"
    );
    ensure!(preview.contains("stays behind"), "{preview}");
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
    ensure!(
        applied.contains("messenger set bot --credential bot-token"),
        "{applied}"
    );
    let settings = app.snapshot().await?.settings;
    let messengers = &settings.messengers;
    ensure!(messengers.len() == 1, "{messengers:?}");
    ensure!(
        messengers[0].name == "bot" && !messengers[0].enabled && messengers[0].credential.is_none(),
        "{messengers:?}"
    );
    let listed = app.cli(&["messenger", "list"]).await?;
    ensure!(listed.contains("state = \"stopped\""), "{listed}");
    let providers = settings.providers;
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
    ensure!(app.snapshot().await?.settings.messengers.len() == 1);
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
