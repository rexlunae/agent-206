#![forbid(unsafe_code)]
#[cfg(target_arch = "wasm32")]
fn main() {
    dioxus::launch(a206_ui::App);
}
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use a206_core::{
        BackendConfig, Capability, CodingBackend, Command, Mode, ProviderPreset, Reply,
    };
    use clap::{Parser, Subcommand};
    use std::path::PathBuf;
    #[derive(Parser)]
    #[command(
        name = "agent206",
        version,
        about = "Agent 206 — your work, your agents"
    )]
    struct Args {
        #[arg(long, global = true)]
        data_dir: Option<PathBuf>,
        #[arg(long, global = true)]
        remote: Option<String>,
        #[command(subcommand)]
        command: Option<Action>,
    }
    #[derive(Subcommand)]
    enum Action {
        Providers {
            #[command(subcommand)]
            command: ProviderAction,
        },
        Backend {
            #[command(subcommand)]
            command: BackendAction,
        },
        /// Launch the primary desktop client.
        Desktop,
        /// Launch the shared terminal client.
        #[command(visible_alias = "ui")]
        Tui,
        Joshua {
            #[command(subcommand)]
            command: JoshuaAction,
        },
        Acp {
            thread: uuid::Uuid,
        },
        Mcp {
            thread: uuid::Uuid,
            /// Permit this MCP session to edit only its bound conversation's title.
            #[arg(long)]
            allow_title_edit: bool,
        },
        /// Manage the configured local or SSH gateway.
        Gateway {
            #[command(subcommand)]
            command: Option<GatewayAction>,
            #[arg(long)]
            stdio: bool,
            #[arg(long, hide = true)]
            daemon: bool,
            #[arg(long, hide = true, conflicts_with = "start")]
            no_start: bool,
            #[arg(long, hide = true)]
            start: bool,
        },
        /// Show gateway and workspace status without starting a service.
        Status,
        /// Export the current workspace snapshot as TOML.
        Snapshot,
        /// Stop the shared gateway (compatibility command).
        StopGateway,
        New {
            title: String,
            #[arg(long, default_value = "chat")]
            mode: String,
        },
        Chat {
            thread: uuid::Uuid,
            text: String,
        },
        Cancel {
            thread: uuid::Uuid,
        },
        /// Export settings as TOML, or apply a reviewed TOML configuration.
        Settings {
            #[arg(long)]
            apply: Option<PathBuf>,
        },
        RefreshModels {
            provider: uuid::Uuid,
        },
        Project {
            name: String,
            root: PathBuf,
        },
        GitStatus {
            project: uuid::Uuid,
        },
        Document {
            project: uuid::Uuid,
            path: String,
        },
        Download {
            url: String,
            name: String,
            #[arg(long)]
            sha256: Option<String>,
            #[arg(long)]
            models_directory: bool,
        },
        Events {
            #[arg(long, default_value_t = 0)]
            after: u64,
        },
        Extension {
            id: String,
        },
        #[command(hide = true)]
        ExtensionWorker,
        Vault {
            #[command(subcommand)]
            command: VaultAction,
        },
        /// Serve the shared web client on an authorized loopback session.
        Web {
            #[arg(long)]
            static_dir: PathBuf,
            #[arg(long, default_value_t = 8206)]
            port: u16,
        },
        Render {
            output: PathBuf,
        },
    }
    #[derive(Subcommand)]
    enum VaultAction {
        Init,
        List,
        Put { name: String },
    }
    #[derive(Subcommand)]
    enum GatewayAction {
        Status,
        Start,
        Stop,
        Restart,
        Reconnect,
    }
    #[derive(Subcommand)]
    enum ProviderAction {
        Presets,
        Add {
            preset: String,
            #[arg(long)]
            endpoint: Option<String>,
            #[arg(long)]
            label: Option<String>,
            /// Saved vault credential name, never an API key.
            #[arg(long)]
            credential: Option<String>,
        },
    }
    #[derive(Subcommand)]
    enum BackendAction {
        Configure {
            kind: String,
            #[arg(long)]
            executable: String,
            #[arg(long)]
            runtime_root: String,
            #[arg(long)]
            adapter_script: Option<String>,
        },
        Select {
            thread: uuid::Uuid,
            kind: String,
            #[arg(long)]
            read: bool,
            #[arg(long)]
            write: bool,
            #[arg(long)]
            execute: bool,
        },
    }
    fn backend_kind(value: &str) -> anyhow::Result<CodingBackend> {
        match value {
            "native" => Ok(CodingBackend::Native),
            "codex" => Ok(CodingBackend::Codex),
            "claude-agent" => Ok(CodingBackend::ClaudeAgent),
            _ => anyhow::bail!("choose native, codex or claude-agent"),
        }
    }
    #[derive(Subcommand)]
    enum JoshuaAction {
        Start {
            label: String,
            model_file: String,
            #[arg(long)]
            port: u16,
        },
        Stop,
        Status,
    }
    fn default_root() -> anyhow::Result<PathBuf> {
        Ok(PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| anyhow::anyhow!("HOME is not set; pass --data-dir"))?,
        )
        .join(".agent206"))
    }
    pub fn run() -> anyhow::Result<()> {
        let args = Args::parse();
        let root = args.data_dir.map(Ok).unwrap_or_else(default_root)?;
        match args.command.unwrap_or(Action::Desktop) {
            Action::Desktop => {
                #[cfg(feature = "desktop")]
                {
                    a206_ui::configure(root, args.remote);
                    dioxus::launch(a206_ui::App);
                    Ok(())
                }
                #[cfg(not(feature = "desktop"))]
                {
                    anyhow::bail!("build with --features desktop")
                }
            }
            Action::Tui => {
                #[cfg(feature = "tui")]
                {
                    a206_ui::configure(root, args.remote);
                    dioxus_tui::launch(a206_ui::App)
                }
                #[cfg(not(feature = "tui"))]
                {
                    anyhow::bail!("build with --features tui")
                }
            }
            Action::Render { output } => {
                let mut dom = dioxus::prelude::VirtualDom::new(a206_ui::App);
                dom.rebuild_in_place();
                std::fs::write(output, dioxus_ssr::render(&dom))?;
                Ok(())
            }
            Action::Vault { command } => {
                let password =
                    a206_vault::Secret::new(rpassword::prompt_password("Vault password: ")?);
                let mut vault = a206_vault::Vault::locked(root.join("vault").join("secrets.json"));
                vault.unlock(&password)?;
                // Two-factor state comes from the encrypted vault, so an
                // enrolled (including imported RustyClaw) vault always challenges.
                if vault.totp_enrolled()? {
                    let code = rpassword::prompt_password("Authenticator code: ")?;
                    let outcome = vault.verify_totp(&code, a206_vault::unix_now())?;
                    if outcome != a206_vault::TotpOutcome::Valid {
                        vault.lock();
                        if let a206_vault::TotpOutcome::ClockDrift { steps } = outcome {
                            anyhow::bail!(
                                "authenticator code rejected: clocks differ by about {} seconds",
                                steps.unsigned_abs() * 30
                            );
                        }
                        anyhow::bail!("authenticator code rejected");
                    }
                }
                match command {
                    VaultAction::Init => println!("Vault ready"),
                    VaultAction::List => {
                        for name in vault.names()? {
                            println!("{name}");
                        }
                        for (name, meta) in vault.credentials()? {
                            let state = if meta.disabled { " disabled" } else { "" };
                            println!("{name}\t{:?}\t{:?}{state}", meta.kind, meta.policy);
                        }
                    }
                    VaultAction::Put { name } => {
                        let value =
                            a206_vault::Secret::new(rpassword::prompt_password("Secret value: ")?);
                        vault.put(&name, value)?;
                        println!("Credential stored");
                    }
                }
                Ok(())
            }
            Action::ExtensionWorker => {
                let (_, wasm) = a206_extensions::builtin()?;
                let output = a206_extensions::execute(wasm, 1_000_000)?;
                use std::io::Write;
                std::io::stdout().write_all(&rmp_serde::to_vec_named(&output)?)?;
                Ok(())
            }
            action => tokio::runtime::Runtime::new()?.block_on(async move {
                if matches!(action, Action::Status | Action::StopGateway) {
                    use a206_core::{GatewayAction as Control, GatewayPhase};
                    let controller =
                        a206_runtime::control::GatewayController::attach(root, args.remote);
                    let status = matches!(action, Action::Status);
                    let gateway = controller
                        .control(if status {
                            Control::Status
                        } else {
                            Control::Stop
                        })
                        .await?;
                    #[derive(serde::Serialize)]
                    struct SystemStatus {
                        gateway: a206_core::GatewayReport,
                        #[serde(skip_serializing_if = "Option::is_none")]
                        workspace: Option<WorkspaceStatus>,
                    }
                    #[derive(serde::Serialize)]
                    struct WorkspaceStatus {
                        conversations: usize,
                        projects: usize,
                        providers: usize,
                        api_catalogs: usize,
                    }
                    let workspace = if status && gateway.phase == GatewayPhase::Running {
                        match controller.call(a206_core::Command::Snapshot).await? {
                            Reply::Snapshot(snapshot) => Some(WorkspaceStatus {
                                conversations: snapshot.threads.len(),
                                projects: snapshot.projects.len(),
                                providers: snapshot.settings.providers.len(),
                                api_catalogs: snapshot.catalogs.len(),
                            }),
                            Reply::Error { message } => anyhow::bail!("{message}"),
                            _ => anyhow::bail!("gateway did not return workspace status"),
                        }
                    } else {
                        None
                    };
                    if status {
                        print!(
                            "{}",
                            toml::to_string_pretty(&SystemStatus {
                                gateway: gateway.clone(),
                                workspace
                            })?
                        );
                    } else {
                        print!("{}", toml::to_string_pretty(&gateway)?);
                    }
                    if let Some(error) = gateway.error {
                        anyhow::bail!("{error}");
                    }
                    return Ok(());
                }
                if let Action::Gateway {
                    command,
                    stdio,
                    daemon,
                    no_start,
                    start,
                } = action
                {
                    if let Some(action) = command {
                        use a206_core::{GatewayAction as Control, GatewayPhase};
                        anyhow::ensure!(
                            !stdio && !daemon && !no_start && !start,
                            "choose a gateway command or a transport mode"
                        );
                        let controller =
                            a206_runtime::control::GatewayController::attach(root, args.remote);
                        // Status/reconnect/stop must not launch a missing gateway.
                        let action = match action {
                            GatewayAction::Status => Control::Status,
                            GatewayAction::Start => Control::Start,
                            GatewayAction::Stop => Control::Stop,
                            GatewayAction::Restart => Control::Restart,
                            GatewayAction::Reconnect => Control::Reconnect,
                        };
                        let report = controller.control(action).await?;
                        print!("{}", toml::to_string_pretty(&report)?);
                        if let Some(error) = report.error {
                            anyhow::bail!("{error}");
                        }
                        if report.phase == GatewayPhase::Offline {
                            anyhow::bail!("gateway is offline");
                        }
                        return Ok(());
                    }
                    return if daemon {
                        a206_runtime::service::daemon(&root, start).await
                    } else if stdio {
                        a206_runtime::service::stdio(&root, !no_start, start).await
                    } else {
                        anyhow::bail!("choose gateway start, stop, restart, status or reconnect")
                    };
                }
                if let Action::Web { static_dir, port } = action {
                    return a206_runtime::web::serve(root, static_dir, port).await;
                }
                let client = a206_runtime::client::Client::start(root, args.remote);
                if let Action::Acp { thread } = action {
                    return a206_runtime::acp::serve(client, thread).await;
                }
                if let Action::Mcp {
                    thread,
                    allow_title_edit,
                } = action
                {
                    return a206_runtime::mcp::serve(client, thread, allow_title_edit).await;
                }
                let command = match action {
                    Action::Providers {
                        command: ProviderAction::Presets,
                    } => {
                        let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await?
                        else {
                            anyhow::bail!("could not read settings");
                        };
                        #[derive(serde::Serialize)]
                        struct Presets {
                            providers: Vec<ProviderPreset>,
                        }
                        print!(
                            "{}",
                            toml::to_string_pretty(&Presets {
                                providers: snapshot.settings.provider_presets.effective()
                            })?
                        );
                        return Ok(());
                    }
                    Action::Providers {
                        command:
                            ProviderAction::Add {
                                preset,
                                endpoint,
                                label,
                                credential,
                            },
                    } => {
                        let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await?
                        else {
                            anyhow::bail!("could not read settings");
                        };
                        let preset = snapshot
                            .settings
                            .provider_presets
                            .effective()
                            .into_iter()
                            .find(|p| p.id == preset)
                            .ok_or_else(|| {
                                anyhow::anyhow!("unknown provider preset; run providers presets")
                            })?;
                        let mut settings = snapshot.settings;
                        let mut provider =
                            preset.configure(endpoint.unwrap_or_else(|| preset.base_url.clone()));
                        if let Some(label) = label {
                            provider.label = label;
                        }
                        provider.credential = credential;
                        settings.providers.push(provider);
                        Command::SaveSettings(settings)
                    }
                    Action::Backend {
                        command:
                            BackendAction::Configure {
                                kind,
                                executable,
                                runtime_root,
                                adapter_script,
                            },
                    } => {
                        let kind = backend_kind(&kind)?;
                        let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await?
                        else {
                            anyhow::bail!("could not read settings");
                        };
                        let mut settings = snapshot.settings;
                        settings.backends.retain(|b| b.kind != kind);
                        settings.backends.push(BackendConfig {
                            kind,
                            executable,
                            runtime_root,
                            adapter_script,
                        });
                        Command::SaveSettings(settings)
                    }
                    Action::Backend {
                        command:
                            BackendAction::Select {
                                thread,
                                kind,
                                read,
                                write,
                                execute,
                            },
                    } => {
                        let capabilities = [
                            (read || write, Capability::ReadWorkspace),
                            (write, Capability::WriteWorkspace),
                            (execute, Capability::Execute),
                        ]
                        .into_iter()
                        .filter_map(|(allowed, capability)| allowed.then_some(capability))
                        .collect();
                        Command::SetBackend {
                            thread,
                            backend: backend_kind(&kind)?,
                            capabilities,
                        }
                    }
                    Action::Joshua { command } => match command {
                        JoshuaAction::Start {
                            label,
                            model_file,
                            port,
                        } => Command::StartJoshua {
                            label,
                            model_file,
                            port,
                            remote: None,
                        },
                        JoshuaAction::Stop => Command::StopJoshua,
                        JoshuaAction::Status => Command::JoshuaStatus,
                    },
                    Action::Snapshot => Command::Snapshot,
                    Action::New { title, mode } => Command::CreateThread {
                        title,
                        mode: Mode::ALL
                            .into_iter()
                            .find(|m| m.label().eq_ignore_ascii_case(&mode))
                            .ok_or_else(|| anyhow::anyhow!("unknown mode"))?,
                    },
                    Action::Chat { thread, text } => Command::Send {
                        thread,
                        text,
                        attachments: vec![],
                    },
                    Action::Cancel { thread } => Command::CancelRun { thread },
                    Action::Settings { apply: Some(path) } => {
                        Command::SaveSettings(toml::from_str(&std::fs::read_to_string(path)?)?)
                    }
                    Action::Settings { apply: None } => {
                        if let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await? {
                            print!("{}", toml::to_string_pretty(&snapshot.settings)?);
                            return Ok(());
                        } else {
                            anyhow::bail!("failed to read settings");
                        }
                    }
                    Action::RefreshModels { provider } => Command::RefreshCatalog { provider },
                    Action::Project { name, root } => Command::CreateProject {
                        name,
                        root: root.to_string_lossy().into_owned(),
                    },
                    Action::GitStatus { project } => Command::GitStatus { project },
                    Action::Document { project, path } => Command::ReadDocument { project, path },
                    Action::Download {
                        url,
                        name,
                        sha256,
                        models_directory,
                    } => Command::Download {
                        url,
                        name,
                        sha256,
                        models_directory,
                    },
                    Action::Events { after } => Command::Events { after },
                    Action::Extension { id } => Command::RunExtension {
                        id,
                        input: String::new(),
                    },
                    _ => anyhow::bail!("unsupported command"),
                };
                match client.call(command).await? {
                    Reply::Error { message } => anyhow::bail!(message),
                    Reply::Text(text) => println!("{text}"),
                    Reply::Ok => println!("OK"),
                    reply => println!("{}", toml::to_string_pretty(&reply)?),
                }
                Ok(())
            }),
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
fn main() {
    if let Err(error) = native::run() {
        eprintln!("Agent 206: {error:#}");
        std::process::exit(1);
    }
}
