//! `agent206 setup` and `agent206 import`: terminal walkthroughs over the same
//! gateway setup, vault broker requests and settings API as the desktop and
//! terminal clients. Every security decision stays with the broker.
use crate::doctor_cli::confirm;
use crate::vault_cli;
use a206_core::{
    Command, GatewayAction, GatewayPhase, GatewayTarget, MessengerKind, ProviderPreset, Reply,
};
use a206_import::{
    InactiveCategory, MessengerAction, Preview, ProviderAction, Receipt, Unsupported,
    UnsupportedMessenger, UnsupportedProvider, VaultPlan, VaultProtection,
};
use a206_vault::CredentialKind;
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum ImportSource {
    /// Review one Claude Code settings.json scope on the gateway host.
    ReviewClaudeCode { path: PathBuf },
    /// Review the model providers in one Codex config.toml directory on the gateway host.
    ReviewCodex { path: PathBuf },
    /// Review RustyClaw settings on the gateway host, without changing them.
    ReviewRustyclaw { path: PathBuf },
    /// Apply selected zero-based item indices from a held gateway review.
    ApplyReview {
        review: uuid::Uuid,
        #[arg(long)]
        provider: Vec<usize>,
        #[arg(long)]
        messenger: Vec<usize>,
    },
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
            println!("Setup paused before the vault; rerun `agent206 setup` to create it.");
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

/// One line of input, trimmed; `None` once input is closed.
fn ask(prompt: &str) -> Result<Option<String>> {
    use std::io::{BufRead, Write};
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer)? == 0 {
        println!();
        return Ok(None);
    }
    Ok(Some(answer.trim().to_owned()))
}

/// A preset by its number in the printed list, by its id, or by `id:ID`.
/// A plain number is always a list position, so a numeric id is chosen with
/// the `id:` form.
fn pick<'a>(presets: &'a [ProviderPreset], answer: &str) -> Option<&'a ProviderPreset> {
    if let Some(id) = answer.strip_prefix("id:") {
        return presets.iter().find(|p| p.id == id);
    }
    if !answer.is_empty() && answer.bytes().all(|b| b.is_ascii_digit()) {
        // Digits are only ever a position, even past `usize`.
        return answer
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
            .and_then(|i| presets.get(i));
    }
    presets.iter().find(|p| p.id == answer)
}

fn choose(presets: &[ProviderPreset]) -> Result<Option<&ProviderPreset>> {
    loop {
        let Some(answer) = ask("Provider (number, id, or id:ID for an id made of digits): ")?
        else {
            return Ok(None);
        };
        match pick(presets, &answer) {
            Some(preset) => return Ok(Some(preset)),
            None => println!("Choose one of the numbers or ids listed above."),
        }
    }
}

/// The account's endpoint, the preset's unless another is typed, checked by
/// the same rule the gateway applies when the account is saved.
fn endpoint(preset: &ProviderPreset) -> Result<Option<(String, String)>> {
    loop {
        let prompt = if preset.base_url.is_empty() {
            "API base URL: ".to_owned()
        } else {
            format!("API base URL [{}]: ", preset.base_url)
        };
        let Some(answer) = ask(&prompt)? else {
            return Ok(None);
        };
        let typed = if answer.is_empty() {
            preset.base_url.clone()
        } else {
            answer
        };
        if typed.is_empty() {
            println!("This provider has no default endpoint; enter its base URL.");
            continue;
        }
        match a206_core::providers::base_url(&typed) {
            Ok(url) => return Ok(Some((typed, url.origin().ascii_serialization()))),
            Err(error) => println!(
                "{:#}",
                anyhow::Error::new(error).context("this endpoint cannot be used")
            ),
        }
    }
}

/// The vault credential the account uses: one already bound to `origin`, or
/// a new API key typed into a hidden prompt. Without an unlocked vault on
/// this computer the account is added without one. A gateway that does not
/// hold a pairing with this broker is paired on the same authenticated
/// connection, since it cannot spend the credential otherwise.
async fn provider_credential(
    root: &Path,
    preset: &ProviderPreset,
    origin: &str,
    gateway_paired: Option<bool>,
) -> Result<Option<String>> {
    let mut client = vault_cli::connect(root).await?;
    let status = vault_cli::status(&mut client).await?;
    if !status.unlocked {
        println!(
            "The vault is locked, so no API key is stored now. To add one later: agent206 vault add NAME --destination {origin}, then name it as the account's credential with agent206 settings --apply."
        );
        return Ok(None);
    }
    let typed = vault_cli::credentials(&mut client).await?;
    let bound = typed.iter().find(|(_, meta)| {
        !meta.disabled
            && matches!(meta.kind, CredentialKind::ApiKey | CredentialKind::Token)
            && meta.destinations.iter().any(|d| d == origin)
    });
    let (name, store) = match bound {
        Some((name, _))
            if confirm(&format!(
                "Use the vault credential {name}, which may be sent to {origin}?"
            ))? =>
        {
            (name.clone(), false)
        }
        _ => {
            if !confirm(&format!(
                "Store an API key for {} in the vault, sent only to {origin}?",
                preset.name
            ))? {
                return Ok(None);
            }
            let base = format!("{}-api-key", preset.id);
            let name = std::iter::once(base.clone())
                .chain((2..).map(|n| format!("{base}-{n}")))
                .find(|candidate| typed.iter().all(|(existing, _)| existing != candidate))
                .context("no free credential name")?;
            (name, true)
        }
    };
    let pair = !(gateway_paired == Some(true) && status.gateway_paired == Some(true));
    if !store && !pair {
        return Ok(Some(name));
    }
    // One step-up covers storing the key and pairing on this connection.
    vault_cli::step_up(&mut client).await?;
    if store {
        vault_cli::store_api_key(
            &mut client,
            name.clone(),
            format!("{} API key", preset.name),
            origin.to_owned(),
        )
        .await?;
        println!("API key stored in the vault as {name}");
    }
    if pair {
        a206_runtime::vault::pair_gateway(&mut client, root)
            .await
            .with_context(|| {
                format!(
                    "the gateway could not be paired with the vault, so the account was not added; {name} stays in the vault"
                )
            })?;
        println!("Gateway paired with the vault");
    }
    Ok(Some(name))
}

/// The provider part of setup on this computer: when the running gateway has
/// no provider account, offer one from the provider library, with its API key
/// kept in the vault and bound to the endpoint's origin, then fetch its model
/// list from the provider's API. Models are chosen per conversation later.
pub async fn provider(root: &Path) -> Result<()> {
    let target = a206_runtime::connection::load(root)?.map(|connection| connection.target);
    if target != Some(GatewayTarget::Local) {
        // A remote gateway's vault is on its host; setup there adds providers.
        return Ok(());
    }
    let client = a206_runtime::control::GatewayController::attach(root.to_owned(), None);
    let report = client.control(GatewayAction::Status).await?;
    if report.phase != GatewayPhase::Running {
        println!(
            "Start the gateway with `agent206 gateway start`, then rerun `agent206 setup` to add a model provider."
        );
        return Ok(());
    }
    let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await?.into_result()? else {
        bail!("gateway did not return its settings");
    };
    let mut settings = snapshot.settings;
    if !settings.providers.is_empty() {
        println!("Model providers: {} configured", settings.providers.len());
        return Ok(());
    }
    if !confirm("Add a model provider account now?")? {
        println!("No provider added; add one later with `agent206 providers add`.");
        return Ok(());
    }
    let presets = settings.provider_presets.effective();
    if presets.is_empty() {
        println!("The provider library is empty; add a preset with `agent206 settings --apply`.");
        return Ok(());
    }
    for (number, preset) in presets.iter().enumerate() {
        println!(
            "{:>3}. {} [{}] ({}, {})",
            number + 1,
            preset.name,
            preset.id,
            preset.group,
            preset.api.label()
        );
    }
    let Some(preset) = choose(&presets)? else {
        println!("No provider added.");
        return Ok(());
    };
    if !preset.setup.is_empty() {
        println!("{}", preset.setup);
    }
    let Some((base_url, origin)) = endpoint(preset)? else {
        println!("No provider added.");
        return Ok(());
    };
    let mut account = preset.configure(base_url);
    let gateway_paired = report.info.as_ref().and_then(|info| info.vault_paired);
    account.credential = provider_credential(root, preset, &origin, gateway_paired).await?;
    let (id, label, credential) = (
        account.id,
        account.label.clone(),
        account.credential.clone(),
    );
    settings.providers.push(account);
    let saved = client
        .call(Command::SaveSettings(Box::new(settings)))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|reply| Ok(reply.into_result().map(drop)?));
    match credential {
        Some(name) => saved.with_context(|| {
            format!("the account was not saved; its credential {name} stays in the vault")
        })?,
        None => saved.context("the provider account was not saved")?,
    }
    println!("Added {label}");
    // The account stays saved either way; a failed fetch is shown with its
    // cause and the health checks below list it as still to fix.
    match client
        .call(Command::RefreshCatalog { provider: id })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|reply| Ok(reply.into_result()?))
    {
        Ok(Reply::Catalog(catalog)) => {
            println!("{label} lists {} models", catalog.models.len());
        }
        Ok(_) => eprintln!("The gateway did not return {label}'s model list"),
        Err(error) => eprintln!("Could not fetch {label}'s model list: {error:#}"),
    }
    Ok(())
}

fn inactive(category: InactiveCategory) -> &'static str {
    use InactiveCategory::*;
    match category {
        Environment => "environment settings (not inherited)",
        Hooks => "hooks (never run)",
        Plugins => "plugins and marketplaces (not enabled)",
        Credentials => "credentials and helpers (not read or run)",
        ModelSelection => "model choices (not bound)",
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
                UnsupportedMessenger::NoSignalDaemon => {
                    "skipped: start a signal-cli daemon, then add it with `agent206 messenger add`"
                }
                UnsupportedMessenger::ThroughBeeper => {
                    "skipped: connect this network to Beeper Desktop, then add Beeper with `agent206 messenger add`"
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
        if let (Some(format), Some(endpoint)) = (messenger.format, &messenger.endpoint) {
            println!(
                "    a webhook that only sends, as {}; store its whole URL in the vault, bound to {endpoint}",
                format.label()
            );
        } else if messenger.allowed_chats.is_empty() {
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
                "    its token, password or webhook URL was stored in plain text and stays behind; add it to the vault yourself"
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
    vault_cli::import_reviewed(&mut client, root, source, true).await
}

pub async fn import(root: &Path, remote: Option<String>, source: ImportSource) -> Result<()> {
    let (path, apply, skip_vault, skip_providers, skip_messengers, toml) = match source {
        ImportSource::ReviewClaudeCode { path } => {
            return review_command(
                root,
                remote,
                Command::PreviewSourceImport {
                    tool: a206_import::Tool::ClaudeCode,
                    source: path,
                },
            )
            .await;
        }
        ImportSource::ReviewCodex { path } => {
            return review_command(
                root,
                remote,
                Command::PreviewSourceImport {
                    tool: a206_import::Tool::Codex,
                    source: path,
                },
            )
            .await;
        }
        ImportSource::ReviewRustyclaw { path } => {
            return review_command(root, remote, Command::PreviewImport { source: path }).await;
        }
        ImportSource::ApplyReview {
            review,
            provider,
            messenger,
        } => {
            return review_command(
                root,
                remote,
                Command::ApplyImport {
                    review,
                    selection: a206_core::imports::Selection {
                        providers: provider.into_iter().collect(),
                        messengers: messenger.into_iter().collect(),
                    },
                },
            )
            .await;
        }
        ImportSource::RustyClaw {
            path,
            apply,
            skip_vault,
            skip_providers,
            skip_messengers,
            toml,
        } => (
            path,
            apply,
            skip_vault,
            skip_providers,
            skip_messengers,
            toml,
        ),
    };
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
    let preview = a206_runtime::imports::preview(&path, &snapshot.settings, vault_exists).await?;
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
        if messenger.kind.is_some_and(MessengerKind::is_send_only) {
            println!(
                "To use webhook {name}: agent206 vault add {name}-url --kind token --destination {origin} with the whole webhook URL as its value, then agent206 messenger set {name} --credential {name}-url and agent206 messenger enable {name}"
            );
        } else {
            println!(
                "To use messenger {name}: agent206 vault add {name}-token --kind token --destination {origin}, then agent206 messenger set {name} --credential {name}-token, agent206 messenger allow {name} <chat> and agent206 messenger enable {name}"
            );
        }
    }
    Ok(())
}

async fn review_command(root: &Path, remote: Option<String>, command: Command) -> Result<()> {
    let expected_tool = match &command {
        Command::PreviewImport { .. } => Some(a206_core::imports::Tool::RustyClaw),
        Command::PreviewSourceImport { tool, .. } => Some(*tool),
        _ => None,
    };
    let target = a206_runtime::connection::require(root, remote)?;
    let client = a206_runtime::control::GatewayController::attach(
        root.to_owned(),
        target.remote().map(str::to_owned),
    );
    let reply = client.call(command).await?.into_result()?;
    match reply {
        Reply::ImportPreview(review) => {
            if review.version != a206_core::imports::REVIEW_VERSION
                || review.preview.version != a206_core::imports::PREVIEW_VERSION
                || expected_tool != Some(review.preview.tool)
            {
                bail!(
                    "gateway import review version or source tool is unsupported; update the gateway and preview again"
                );
            }
            print!("{}", toml::to_string_pretty(&review)?);
        }
        Reply::ImportApplied(result) => {
            print!("{}", toml::to_string_pretty(&result.receipt)?);
            if let Some(error) = result.receipt_error {
                return Err(anyhow::Error::new(error)
                    .context("Settings were imported, but their receipt could not be saved"));
            }
        }
        _ => bail!("gateway did not return an import review or application result"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(id: String) -> ProviderPreset {
        ProviderPreset {
            name: uuid::Uuid::new_v4().to_string(),
            id,
            group: uuid::Uuid::new_v4().to_string(),
            api: Default::default(),
            base_url: String::new(),
            setup: String::new(),
        }
    }

    #[test]
    fn a_number_is_a_list_position_and_id_prefix_names_an_id() {
        let named = preset(uuid::Uuid::new_v4().simple().to_string());
        // A numeric id that is also a valid position in the list.
        let numeric = preset("1".into());
        let presets = [named.clone(), numeric.clone()];
        assert_eq!(pick(&presets, "1"), Some(&named));
        assert_eq!(pick(&presets, "2"), Some(&numeric));
        assert_eq!(pick(&presets, "id:1"), Some(&numeric));
        assert_eq!(pick(&presets, &named.id), Some(&named));
        assert_eq!(pick(&presets, &format!("id:{}", named.id)), Some(&named));
        assert_eq!(pick(&presets, "0"), None);
        assert_eq!(pick(&presets, "3"), None);
        assert_eq!(pick(&presets, "id:3"), None);
        // Digits too long for a position never fall back to an id.
        let huge = preset("9".repeat(40));
        let presets = [named.clone(), huge.clone()];
        assert_eq!(pick(&presets, &huge.id), None);
        assert_eq!(pick(&presets, &format!("id:{}", huge.id)), Some(&huge));
    }
}
