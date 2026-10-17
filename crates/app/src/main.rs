#![forbid(unsafe_code)]
#[cfg(target_arch = "wasm32")]
fn main() {
    dioxus::launch(a206_ui::App);
}
#[cfg(not(target_arch = "wasm32"))]
mod doctor_cli;
#[cfg(not(target_arch = "wasm32"))]
mod extension_cli;
#[cfg(all(unix, not(target_arch = "wasm32")))]
mod setup_cli;
#[cfg(all(unix, not(target_arch = "wasm32")))]
mod vault_cli;
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
        /// Inspect and control configured MCP servers (add them with `settings --apply`).
        McpServers {
            #[command(subcommand)]
            command: McpServerAction,
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
        /// Walk through first-run setup: the gateway, then this computer's
        /// vault, two-factor enrollment and gateway pairing. Rerun to resume.
        #[cfg(unix)]
        Setup {
            /// Use a gateway on this computer.
            #[arg(long, conflicts_with = "ssh")]
            local: bool,
            /// Use an installed agent206 reached by OpenSSH: ssh://[user@]host[:port].
            #[arg(long)]
            ssh: Option<String>,
            /// Trust this SSH host key fingerprint after verifying it out of band.
            #[arg(long, requires = "ssh")]
            trust: Vec<String>,
            /// Do not offer authenticator enrollment.
            #[arg(long)]
            skip_totp: bool,
        },
        /// Check the profile, gateway, vault broker and provider accounts
        /// without starting or changing anything.
        Doctor {
            /// Print the report as TOML.
            #[arg(long)]
            toml: bool,
        },
        /// Preview, then apply with --apply, a reviewed import from another tool.
        #[cfg(unix)]
        Import {
            #[command(subcommand)]
            source: crate::setup_cli::ImportSource,
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
        /// Inspect, grant and run extensions.
        Extension {
            #[command(subcommand)]
            command: crate::extension_cli::ExtensionAction,
        },
        #[command(hide = true)]
        ExtensionWorker,
        /// Manage the encrypted vault through its isolated broker.
        #[cfg(unix)]
        Vault {
            #[command(subcommand)]
            command: crate::vault_cli::VaultAction,
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
    enum GatewayAction {
        /// Choose the gateway this profile connects to (required before first use).
        Setup {
            /// Use a gateway on this computer.
            #[arg(long, conflicts_with = "ssh", required_unless_present = "ssh")]
            local: bool,
            /// Use an installed agent206 reached by OpenSSH: ssh://[user@]host[:port].
            #[arg(long)]
            ssh: Option<String>,
            /// Trust this SSH host key fingerprint after verifying it out of band.
            #[arg(long, requires = "ssh")]
            trust: Vec<String>,
        },
        Status,
        Start,
        Stop,
        Restart,
        Reconnect,
    }
    #[derive(Subcommand)]
    enum McpServerAction {
        /// Show each server's state, reported tools and last failure.
        List,
        /// Allow a server to start; its tools stay subject to thread grants.
        Enable { name: String },
        /// Stop a server and keep it from starting.
        Disable { name: String },
        /// Start an enabled server now and list its tools.
        Connect { name: String },
        /// Stop a server until it is connected again.
        Disconnect { name: String },
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
    /// Settings that turn one MCP server on or off, leaving everything else as is.
    async fn mcp_server_enabled(
        client: &a206_runtime::client::Client,
        name: String,
        enabled: bool,
    ) -> anyhow::Result<Command> {
        let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await? else {
            anyhow::bail!("could not read settings");
        };
        let mut settings = snapshot.settings;
        settings
            .mcp_servers
            .iter_mut()
            .find(|server| server.name == name)
            .ok_or_else(|| {
                anyhow::anyhow!("no MCP server named {name}; add it with settings --apply")
            })?
            .enabled = enabled;
        Ok(Command::SaveSettings(settings))
    }
    fn default_root() -> anyhow::Result<PathBuf> {
        Ok(PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| anyhow::anyhow!("HOME is not set; pass --data-dir"))?,
        )
        .join(".agent206"))
    }
    /// Structured diagnostics go to stderr (the daemon's `gateway.log`), except
    /// for the terminal client, whose screen owns the terminal. `AGENT206_LOG`
    /// takes `tracing` filter directives; the default records warnings.
    fn init_tracing(root: &std::path::Path, terminal_ui: bool) -> anyhow::Result<()> {
        use tracing_subscriber::EnvFilter;
        let filter =
            EnvFilter::try_from_env("AGENT206_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
        let builder = tracing_subscriber::fmt().with_env_filter(filter);
        if terminal_ui {
            std::fs::create_dir_all(root)?;
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let log = options.open(root.join("tui.log"))?;
            builder
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(log))
                .try_init()
        } else {
            builder.with_writer(std::io::stderr).try_init()
        }
        .map_err(|error| anyhow::anyhow!(error))
    }
    async fn setup(
        root: &std::path::Path,
        local: bool,
        ssh: Option<String>,
        trust: &[String],
    ) -> anyhow::Result<()> {
        use a206_core::{GatewayConnection, GatewayTarget};
        use a206_runtime::connection::{self, ConnectionError};
        let target = match ssh {
            Some(endpoint) if !local => {
                let version = match connection::check_ssh(root, &endpoint).await {
                    Err(ConnectionError::UnknownHost { .. }) => {
                        let offered = connection::scan_host_keys(&endpoint).await?;
                        let accepted: Vec<_> = offered
                            .iter()
                            .filter(|key| trust.contains(&key.fingerprint))
                            .cloned()
                            .collect();
                        if accepted.is_empty() {
                            println!("The host offers these keys:");
                            for key in &offered {
                                println!("  {} {}", key.algorithm, key.fingerprint);
                            }
                            anyhow::bail!(
                                "verify a fingerprint with the server's administrator, then rerun with --trust <fingerprint>"
                            );
                        }
                        connection::trust_host_keys(root, &accepted)?;
                        connection::check_ssh(root, &endpoint).await?
                    }
                    other => other?,
                };
                println!("Remote gateway: {version}");
                GatewayTarget::Ssh { endpoint }
            }
            _ => GatewayTarget::Local,
        };
        connection::save(root, &GatewayConnection { target })?;
        println!("Gateway configured");
        Ok(())
    }
    pub fn run() -> anyhow::Result<()> {
        let args = Args::parse();
        let root = args.data_dir.map(Ok).unwrap_or_else(default_root)?;
        let action = args.command.unwrap_or(Action::Desktop);
        init_tracing(&root, matches!(action, Action::Tui))?;
        match action {
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
            #[cfg(unix)]
            Action::Vault { command } => crate::vault_cli::run(&root, command),
            Action::ExtensionWorker => {
                a206_extensions::worker::serve(std::io::stdin(), std::io::stdout())?;
                Ok(())
            }
            action => tokio::runtime::Runtime::new()?.block_on(async move {
                // Clients use --remote or the saved setup; never an implicit target.
                let remote = || -> anyhow::Result<Option<String>> {
                    let target = a206_runtime::connection::require(&root, args.remote.clone())?;
                    Ok(target.remote().map(str::to_owned))
                };
                if let Action::Gateway {
                    command: Some(GatewayAction::Setup { local, ssh, trust }),
                    ..
                } = &action
                {
                    return setup(&root, *local, ssh.clone(), trust).await;
                }
                if let Action::Doctor { toml } = action {
                    return crate::doctor_cli::run(&root, args.remote.clone(), toml).await;
                }
                #[cfg(unix)]
                if let Action::Setup {
                    local,
                    ssh,
                    trust,
                    skip_totp,
                } = action
                {
                    if local || ssh.is_some() {
                        setup(&root, local, ssh, &trust).await?;
                    }
                    crate::setup_cli::secure(&root, skip_totp).await?;
                    // Setup succeeded; the report shows what is still open
                    // (such as provider accounts) without failing setup.
                    println!();
                    println!("Health checks:");
                    crate::doctor_cli::print(&crate::doctor_cli::report(&root, None).await?);
                    return Ok(());
                }
                #[cfg(unix)]
                if let Action::Import { source } = action {
                    return crate::setup_cli::import(&root, args.remote.clone(), source).await;
                }
                if matches!(action, Action::Status | Action::StopGateway) {
                    use a206_core::{GatewayAction as Control, GatewayPhase};
                    let controller =
                        a206_runtime::control::GatewayController::attach(root.clone(), remote()?);
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
                            Reply::Error(report) => return Err(report.into()),
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
                    if let Some(error) = gateway.failure_report() {
                        return Err(error.into());
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
                        let controller = a206_runtime::control::GatewayController::attach(
                            root.clone(),
                            remote()?,
                        );
                        // Status/reconnect/stop must not launch a missing gateway.
                        let action = match action {
                            GatewayAction::Setup { .. } => anyhow::bail!("setup was not handled"),
                            GatewayAction::Status => Control::Status,
                            GatewayAction::Start => Control::Start,
                            GatewayAction::Stop => Control::Stop,
                            GatewayAction::Restart => Control::Restart,
                            GatewayAction::Reconnect => Control::Reconnect,
                        };
                        let report = controller.control(action).await?;
                        print!("{}", toml::to_string_pretty(&report)?);
                        if let Some(error) = report.failure_report() {
                            return Err(error.into());
                        }
                        if report.phase == GatewayPhase::Offline {
                            anyhow::bail!("gateway is offline");
                        }
                        return Ok(());
                    }
                    return if daemon {
                        Ok(a206_runtime::service::daemon(&root, start).await?)
                    } else if stdio {
                        Ok(a206_runtime::service::stdio(&root, !no_start, start).await?)
                    } else {
                        anyhow::bail!("choose gateway start, stop, restart, status or reconnect")
                    };
                }
                if let Action::Web { static_dir, port } = action {
                    return Ok(
                        a206_runtime::web::serve(root.clone(), remote()?, static_dir, port).await?,
                    );
                }
                let client = a206_runtime::client::Client::start(root.clone(), remote()?);
                if let Action::Acp { thread } = action {
                    return Ok(a206_runtime::acp::serve(client, thread).await?);
                }
                if let Action::Mcp {
                    thread,
                    allow_title_edit,
                } = action
                {
                    return Ok(a206_runtime::mcp::serve(client, thread, allow_title_edit).await?);
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
                    Action::McpServers { command } => match command {
                        McpServerAction::List => {
                            let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await?
                            else {
                                anyhow::bail!("could not read MCP server status");
                            };
                            #[derive(serde::Serialize)]
                            struct Servers {
                                servers: Vec<a206_core::McpServerStatus>,
                            }
                            print!(
                                "{}",
                                toml::to_string_pretty(&Servers {
                                    servers: snapshot.mcp_servers
                                })?
                            );
                            return Ok(());
                        }
                        McpServerAction::Enable { name } => {
                            mcp_server_enabled(&client, name, true).await?
                        }
                        McpServerAction::Disable { name } => {
                            mcp_server_enabled(&client, name, false).await?
                        }
                        McpServerAction::Connect { name } => Command::ConnectMcpServer { name },
                        McpServerAction::Disconnect { name } => {
                            Command::DisconnectMcpServer { name }
                        }
                    },
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
                    Action::Extension { command } => {
                        match crate::extension_cli::command(command)? {
                            Some(command) => command,
                            None => {
                                let Reply::Snapshot(snapshot) =
                                    client.call(Command::Snapshot).await?
                                else {
                                    anyhow::bail!("could not read extensions");
                                };
                                #[derive(serde::Serialize)]
                                struct Extensions {
                                    extensions: Vec<a206_core::PluginSummary>,
                                }
                                print!(
                                    "{}",
                                    toml::to_string_pretty(&Extensions {
                                        extensions: snapshot.plugins
                                    })?
                                );
                                return Ok(());
                            }
                        }
                    }
                    _ => anyhow::bail!("unsupported command"),
                };
                match client.call(command).await? {
                    Reply::Error(report) => return Err(report.into()),
                    Reply::Text(text) => println!("{text}"),
                    Reply::Panel(view) => println!("{}", crate::extension_cli::render(&view)),
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
