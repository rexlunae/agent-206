//! `agent206 setup` and `agent206 import`: terminal walkthroughs over the same
//! gateway setup, vault broker requests and settings API as the desktop and
//! terminal clients. Every security decision stays with the broker.
use crate::vault_cli;
use a206_broker::{BrokerError, ClientError, Reply as BrokerReply, Request, WireSecret};
use a206_core::{Command, GatewayAction, GatewayPhase, GatewayTarget, Reply};
use a206_import::{
    InactiveCategory, MessengerAction, Preview, ProviderAction, Receipt, Unsupported,
    UnsupportedMessenger, UnsupportedProvider, VaultPlan, VaultProtection,
};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::io::BufRead;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum ImportSource {
    /// Import from a RustyClaw installation (default ~/.rustyclaw).
    #[command(name = "rustyclaw")]
    RustyClaw {
        /// RustyClaw settings directory.
        path: Option<PathBuf>,
        /// Apply the previewed changes; without it, only preview.
        #[arg(long)]
        apply: bool,
        /// Leave the vault behind.
        #[arg(long)]
        skip_vault: bool,
        /// Leave provider accounts behind.
        #[arg(long)]
        skip_providers: bool,
        /// Leave messenger accounts behind.
        #[arg(long)]
        skip_messengers: bool,
        /// Print the preview as TOML.
        #[arg(long)]
        toml: bool,
    },
}

/// Ask a yes/no question; a closed input answers no.
fn confirm(question: &str) -> Result<bool> {
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

/// The vault part of first-run setup on this computer: create or unlock the
/// vault, offer two-factor enrollment and pair a running gateway. Steps that
/// are already done are skipped, so rerunning setup resumes it.
pub async fn secure(root: &Path, skip_totp: bool) -> Result<()> {
    let target = a206_runtime::connection::load(root)?
        .map(|connection| connection.target)
        .context("choose a gateway first: agent206 setup --local, or --ssh ssh://user@host")?;
    if let GatewayTarget::Ssh { endpoint } = target {
        println!(
            "The vault broker runs on the gateway host. Run `agent206 setup` on {endpoint} to create, unlock and pair its vault."
        );
        return Ok(());
    }
    let mut client = vault_cli::connect(root).await?;
    let status = vault_cli::status(&mut client).await?;
    if !status.exists {
        println!(
            "No vault yet. To bring over a RustyClaw vault instead, answer no and run `agent206 import rustyclaw`."
        );
        if !confirm("Create a new encrypted vault now?")? {
            println!("Setup paused before the vault; rerun `agent206 setup` to continue.");
            return Ok(());
        }
        vault_cli::unlock(&mut client, true).await?;
        println!("Vault created and unlocked");
    } else if !status.unlocked {
        vault_cli::unlock(&mut client, false).await?;
        println!("Vault unlocked");
    }
    // Enrollment is readable only once unlocked: it lives inside the vault.
    let enrolled = vault_cli::status(&mut client).await?.totp_enrolled;
    if enrolled == Some(false)
        && !skip_totp
        && confirm("Enroll an authenticator app for two-factor authentication?")?
    {
        vault_cli::enroll_totp(&mut client, "agent206".into()).await?;
    }
    vault_cli::pair_if_running(&mut client, root).await
}

fn inactive(category: InactiveCategory) -> &'static str {
    use InactiveCategory::*;
    match category {
        Messengers => "messenger accounts (and their tokens)",
        McpServers => "MCP servers (commands are never run on import)",
        ToolPermissions => "per-tool permissions (no grant is given instead)",
        DisabledToolGroups => "disabled tool groups (no tool is enabled instead)",
        Services => "managed services",
        Engines => "local inference engines",
        Sandbox => "sandbox settings (Agent 206 chooses its own isolation)",
        PlaintextToken => "registry token stored in plain text (add it with `agent206 vault add`)",
        Skills => "skills",
    }
}

fn unsupported(item: Unsupported) -> &'static str {
    match item {
        Unsupported::WorkspaceInstructions => "workspace instructions such as SOUL.md",
        Unsupported::Conversations => "conversation history",
        Unsupported::Memory => "agent memory",
        Unsupported::SystemPrompt => "messenger system prompt",
    }
}

fn print_preview(preview: &Preview) {
    println!("Import from RustyClaw at {}", preview.source.display());
    match &preview.vault {
        None => println!("  vault: none found"),
        Some(vault) => {
            let protection = match vault.protection {
                VaultProtection::Password => "password",
                VaultProtection::KeyFile => "key file",
            };
            let plan = match vault.plan {
                VaultPlan::Import => "will be copied in encrypted after you enter its password",
                VaultPlan::TargetExists => {
                    "skipped: this profile already has a vault (vaults are never merged)"
                }
                VaultPlan::KeyFileUnsupported => "skipped: key-file vaults cannot be imported yet",
                VaultPlan::TargetUnknown => {
                    "skipped: run the import on the gateway host, where its vault lives"
                }
            };
            println!(
                "  vault: {} ({} bytes, {protection}) {plan}",
                vault.path.display(),
                vault.bytes
            );
        }
    }
    for provider in &preview.providers {
        let action = match &provider.action {
            ProviderAction::Add => "will be added".to_owned(),
            ProviderAction::AlreadyConfigured { existing } => {
                format!("already configured as {existing}")
            }
            ProviderAction::Unsupported { reason } => match reason {
                UnsupportedProvider::UnknownProvider => "skipped: no matching preset".to_owned(),
                UnsupportedProvider::DeviceFlow => {
                    "skipped: device sign-in is not supported yet".to_owned()
                }
                UnsupportedProvider::NoEndpoint => "skipped: no endpoint configured".to_owned(),
                UnsupportedProvider::InvalidEndpoint => {
                    "skipped: endpoint must be HTTPS or loopback HTTP without credentials"
                        .to_owned()
                }
            },
        };
        println!(
            "  provider {} ({}) {} {action}",
            provider.label, provider.source_id, provider.base_url
        );
        if let Some(credential) = &provider.credential {
            println!(
                "    uses vault credential {credential}, only while it is bound to {}",
                provider.origin().as_deref().unwrap_or("this endpoint")
            );
        }
        if let Some(model) = &provider.model_reference {
            println!("    previous default model {model} is kept as a note, not selected");
        }
    }
    for messenger in &preview.messengers {
        let action = match &messenger.action {
            MessengerAction::Add => "will be added, disabled",
            MessengerAction::AlreadyConfigured => "already configured",
            MessengerAction::Unsupported { reason } => match reason {
                UnsupportedMessenger::UnknownKind => "skipped: this service is not supported yet",
                UnsupportedMessenger::ExternalConfig => {
                    "skipped: its settings are in a separate file, which is not read"
                }
                UnsupportedMessenger::InvalidChats => {
                    "skipped: its allowed chats are not ones Agent 206 accepts"
                }
                UnsupportedMessenger::InvalidEndpoint => {
                    "skipped: it names no server, or one that is not an HTTPS origin"
                }
                UnsupportedMessenger::PlainTextIrc => {
                    "skipped: it connects without TLS, which would send its password in the clear"
                }
                UnsupportedMessenger::InvalidLogin => {
                    "skipped: its nickname is not one Agent 206 can sign in with"
                }
            },
        };
        println!(
            "  messenger {} ({} {:?}) {action}",
            messenger.name, messenger.source_kind, messenger.source_name
        );
        if messenger.action != MessengerAction::Add {
            continue;
        }
        if messenger.allowed_chats.is_empty() {
            println!("    no chat is allowed until you allow one");
        } else {
            println!(
                "    allows chats {}",
                messenger
                    .allowed_chats
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !messenger.allowed_users.is_empty() {
            println!(
                "    accepts only senders {}",
                messenger
                    .allowed_users
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if messenger.token_left_behind {
            println!(
                "    its token was stored in plain text and stays behind; add it to the vault yourself"
            );
        }
    }
    for item in &preview.inactive {
        println!(
            "  left inactive: {} × {}",
            item.count,
            inactive(item.category)
        );
    }
    for item in &preview.unsupported {
        println!("  not imported yet: {}", unsupported(*item));
    }
}

/// Copy the vault in through the broker, asking for its password and, when
/// it has two-factor enrolled, a code from its authenticator.
async fn import_vault(root: &Path, source: &Path) -> Result<()> {
    let mut client = vault_cli::connect(root).await?;
    if client.version() < a206_broker::IMPORT_VERSION {
        bail!(
            "the running vault broker predates vault import; stop it with `agent206 vault stop` and rerun the import"
        );
    }
    let password = vault_cli::hidden("Password of the RustyClaw vault: ")?;
    let request = |totp| Request::ImportVault {
        source: source.to_owned(),
        password: WireSecret::new(password.expose().to_owned()),
        totp,
    };
    match client.call(request(None)).await {
        Err(ClientError::Broker(BrokerError::TotpRequired)) => {
            let code = rpassword::prompt_password("Authenticator code for that vault: ")?;
            client.call(request(Some(code))).await?;
        }
        result => {
            result?;
        }
    }
    if let BrokerReply::Credentials { typed, legacy } = client.call(Request::Credentials).await? {
        println!(
            "Vault imported and unlocked: {} typed credentials, {} untyped entries. It keeps its RustyClaw password and two-factor setting.",
            typed.len(),
            legacy.len()
        );
    }
    vault_cli::pair_if_running(&mut client, root).await
}

pub async fn import(root: &Path, remote: Option<String>, source: ImportSource) -> Result<()> {
    let ImportSource::RustyClaw {
        path,
        apply,
        skip_vault,
        skip_providers,
        skip_messengers,
        toml,
    } = source;
    let path = match path {
        Some(path) => path,
        None => a206_import::rustyclaw::default_root(Path::new(
            &std::env::var_os("HOME").context("HOME is not set; pass the RustyClaw directory")?,
        )),
    };
    // The broker accepts only absolute paths; resolve once so the preview
    // shows the paths the import will use.
    let path = std::path::absolute(&path)?;
    let target = a206_runtime::connection::require(root, remote)?;
    // Importing never starts or restarts a gateway, so a preview changes
    // nothing and a deliberate stop stays in force.
    let client = a206_runtime::control::GatewayController::attach(
        root.to_owned(),
        target.remote().map(str::to_owned),
    );
    let report = client.control(GatewayAction::Status).await?;
    if report.phase != GatewayPhase::Running {
        let stopped = "the gateway is not running; start it with `agent206 gateway start`, then rerun the import";
        return Err(match report.failure_report() {
            Some(failure) => anyhow::Error::new(failure).context(stopped),
            None => anyhow::anyhow!(stopped),
        });
    }
    let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await?.into_result()? else {
        bail!("gateway did not return its settings");
    };
    // The vault belongs to the gateway host; only a local profile can check it.
    let vault_exists = match target {
        GatewayTarget::Local => {
            Some(std::fs::symlink_metadata(a206_broker::vault_path(root)).is_ok())
        }
        GatewayTarget::Ssh { .. } => None,
    };
    let preview = a206_import::rustyclaw::preview(&path, &snapshot.settings, vault_exists)?;
    if toml {
        print!("{}", toml::to_string_pretty(&preview)?);
    } else {
        print_preview(&preview);
    }
    if !apply {
        if !toml {
            println!("Nothing was changed. Rerun with --apply to import what is listed above.");
        }
        return Ok(());
    }
    let mut receipt = Receipt {
        version: a206_import::PREVIEW_VERSION,
        id: uuid::Uuid::new_v4(),
        tool: preview.tool,
        source: preview.source.clone(),
        config_sha256: preview.config_sha256.clone(),
        applied_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
        providers_added: vec![],
        messengers_added: vec![],
        vault_imported: false,
    };
    if let Some(vault) = preview
        .vault
        .as_ref()
        .filter(|v| v.plan == VaultPlan::Import)
        && !skip_vault
    {
        import_vault(root, &vault.path).await?;
        receipt.vault_imported = true;
    }
    let mut saved = Ok(());
    let mut settings = snapshot.settings.clone();
    let providers = if skip_providers {
        vec![]
    } else {
        preview.apply_providers(&mut settings)
    };
    let messengers = if skip_messengers {
        vec![]
    } else {
        preview.apply_messengers(&mut settings)
    };
    if !providers.is_empty() || !messengers.is_empty() {
        saved = client
            .call(Command::SaveSettings(Box::new(settings)))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|reply| Ok(reply.into_result().map(drop)?));
        if saved.is_ok() {
            receipt.providers_added = providers;
            receipt.messengers_added = messengers;
        }
    }
    if receipt.vault_imported
        || !receipt.providers_added.is_empty()
        || !receipt.messengers_added.is_empty()
    {
        let written = a206_import::write_receipt(root, &receipt)?;
        println!("Receipt: {}", written.display());
    } else {
        println!("Nothing needed importing.");
    }
    // A vault import already happened; report it alongside the failure.
    saved.context(if receipt.vault_imported {
        "the vault was imported, but provider and messenger accounts were not saved"
    } else {
        "provider and messenger accounts were not saved"
    })?;
    let command = if receipt.vault_imported {
        "adopt"
    } else {
        "add"
    };
    for provider in preview
        .additions()
        .filter(|_| !receipt.providers_added.is_empty())
    {
        if let (Some(credential), Some(origin)) = (&provider.credential, provider.origin()) {
            println!(
                "To let {} use its key: agent206 vault {command} {credential} --destination {origin}",
                provider.label
            );
        }
    }
    for messenger in preview
        .messengers
        .iter()
        .filter(|m| receipt.messengers_added.contains(&m.name))
    {
        let (name, origin) = (
            &messenger.name,
            messenger
                .endpoint
                .as_deref()
                .or_else(|| messenger.kind.and_then(|kind| kind.default_endpoint()))
                .unwrap_or_default(),
        );
        println!(
            "To use messenger {name}: agent206 vault add {name}-token --kind token --destination {origin}, then agent206 messenger set {name} --credential {name}-token, agent206 messenger allow {name} <chat> and agent206 messenger enable {name}"
        );
    }
    Ok(())
}
