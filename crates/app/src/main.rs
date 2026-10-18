#![forbid(unsafe_code)]
#[cfg(target_arch = "wasm32")]
fn main() {
    dioxus::launch(a206_ui::App);
}
#[cfg(not(target_arch = "wasm32"))]
mod doctor_cli;
#[cfg(not(target_arch = "wasm32"))]
mod extension_cli;
#[cfg(not(target_arch = "wasm32"))]
mod messenger_cli;
#[cfg(not(target_arch = "wasm32"))]
mod profiles_cli;
#[cfg(not(target_arch = "wasm32"))]
mod running_cli;
#[cfg(not(target_arch = "wasm32"))]
mod schedule_cli;
#[cfg(not(target_arch = "wasm32"))]
mod search_cli;
#[cfg(all(unix, not(target_arch = "wasm32")))]
mod setup_cli;
#[cfg(not(target_arch = "wasm32"))]
mod teams_cli;
#[cfg(all(unix, not(target_arch = "wasm32")))]
mod vault_cli;
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use a206_core::{
        ApprovalDecision, ApprovalRequest, BackendConfig, Capability, CodingBackend, Command, Mode,
        PromptAnswer, ProviderPreset, Reply, ToolBudget, ToolPermission,
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
        /// Review and approve skills installed in the gateway's profile.
        Skills {
            #[command(subcommand)]
            command: SkillAction,
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
        /// Configure chat service accounts that converse with threads.
        Messenger {
            #[command(subcommand)]
            command: crate::messenger_cli::MessengerAction,
        },
        /// Send messages to a conversation at set times.
        Schedule {
            #[command(subcommand)]
            command: crate::schedule_cli::ScheduleAction,
        },
        /// Show every conversation with a response in progress.
        Running,
        /// Show the agent profiles subagents can start from, and any profile
        /// file that could not be used.
        Profiles,
        /// Show the agent teams a conversation can start, and any team file
        /// that could not be used; or start one.
        Teams {
            #[command(subcommand)]
            command: Option<crate::teams_cli::TeamAction>,
        },
        /// List, add or remove the web search services the `web_search`
        /// tool may query.
        SearchServices {
            #[command(subcommand)]
            command: crate::search_cli::SearchAction,
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
            /// Attach a PNG, JPEG, GIF or WebP image of at most 2 MiB;
            /// repeat for up to four.
            #[arg(long = "image")]
            images: Vec<PathBuf>,
        },
        /// Add a message to a conversation's running native run; it is read
        /// before the run's next model request.
        Steer {
            thread: uuid::Uuid,
            text: String,
        },
        /// Hold a conversation's native run, and its subagents', before
        /// their next model round.
        Pause {
            thread: uuid::Uuid,
        },
        /// Let a paused conversation's runs continue.
        Resume {
            thread: uuid::Uuid,
        },
        Cancel {
            thread: uuid::Uuid,
        },
        /// Summarize a conversation's earlier messages now, keeping its
        /// latest turn; `cancel THREAD` stops it.
        Compact {
            thread: uuid::Uuid,
        },
        /// Inspect and set a conversation's tool permissions and budget.
        Tools {
            #[command(subcommand)]
            command: ToolAction,
        },
        /// Answer tool calls waiting for approval.
        Approvals {
            #[command(subcommand)]
            command: ApprovalAction,
        },
        /// Show the checklist a conversation's models keep with `plan_update`.
        Plan {
            thread: uuid::Uuid,
        },
        /// Show the provider-reported tokens and cost a conversation has
        /// used over all its runs, alone and with its subagents.
        Usage {
            thread: uuid::Uuid,
        },
        /// Answer questions agents asked with `ask_user`.
        Prompts {
            #[command(subcommand)]
            command: PromptAction,
        },
        /// Export settings as TOML, or apply a reviewed TOML configuration.
        Settings {
            #[arg(long)]
            apply: Option<PathBuf>,
        },
        RefreshModels {
            provider: uuid::Uuid,
        },
        /// Gateway activity newest first, without conversation and download
        /// progress notices; pass the last `sequence` printed as --before for
        /// the next page.
        Activity {
            #[arg(long)]
            before: Option<u64>,
            #[arg(long, default_value_t = 20)]
            limit: u32,
        },
        /// Find messages containing every word of QUERY, newest first.
        Search {
            query: String,
            #[arg(long, default_value_t = 20)]
            limit: u32,
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
        /// Serve the shared web client. By default only this computer can
        /// open it, through a one-time link; `--listen` opens it to other
        /// devices over HTTPS with sign-in by the vault's authenticator.
        Web {
            #[arg(long)]
            static_dir: PathBuf,
            /// Loopback port.
            #[arg(long, default_value_t = 8206, conflicts_with = "listen")]
            port: u16,
            /// Address and port to serve other devices on, such as 0.0.0.0:8443.
            #[arg(long, requires = "names")]
            listen: Option<std::net::SocketAddr>,
            /// A host name or IP address other devices use to reach this
            /// server; repeat for several. Requests for other names are refused.
            #[arg(long = "name", requires = "listen")]
            names: Vec<String>,
            /// PEM certificate chain to serve instead of a generated self-signed one.
            #[arg(long, requires_all = ["listen", "tls_key"])]
            tls_cert: Option<PathBuf>,
            /// PEM private key for `--tls-cert`.
            #[arg(long, requires_all = ["listen", "tls_cert"])]
            tls_key: Option<PathBuf>,
        },
        Render {
            output: PathBuf,
        },
    }
    #[derive(Subcommand)]
    enum ToolAction {
        /// List the tools a conversation could use and how each is permitted.
        List { thread: uuid::Uuid },
        /// Set one tool to allow, ask or deny; `unset` returns it to deny.
        Permit {
            thread: uuid::Uuid,
            tool: String,
            permission: String,
        },
        /// Limit model rounds, tool calls and, optionally, tokens per run.
        Budget {
            thread: uuid::Uuid,
            #[arg(long)]
            rounds: u32,
            #[arg(long)]
            tool_calls: u32,
            /// Input plus output tokens per run, as the provider reports them.
            #[arg(long)]
            tokens: Option<u64>,
        },
    }
    #[derive(Subcommand)]
    enum ApprovalAction {
        /// Show the tool calls waiting for this conversation's user.
        List { thread: uuid::Uuid },
        /// Run one pending call; DIGEST must be the one `approvals list` showed.
        Approve {
            thread: uuid::Uuid,
            approval: uuid::Uuid,
            digest: String,
        },
        /// Refuse one pending call.
        Deny {
            thread: uuid::Uuid,
            approval: uuid::Uuid,
            digest: String,
        },
    }
    #[derive(Subcommand)]
    enum PromptAction {
        /// Show the questions waiting for this conversation's user.
        List { thread: uuid::Uuid },
        /// Pick a suggested answer; CHOICE counts from 1 in the listed order.
        Choose {
            thread: uuid::Uuid,
            prompt: uuid::Uuid,
            choice: std::num::NonZeroU32,
        },
        /// Answer in your own words, if the question allows it.
        Answer {
            thread: uuid::Uuid,
            prompt: uuid::Uuid,
            text: String,
        },
        /// Tell the agent you will not answer.
        Decline {
            thread: uuid::Uuid,
            prompt: uuid::Uuid,
        },
    }
    fn permission(value: &str) -> anyhow::Result<Option<ToolPermission>> {
        if value.eq_ignore_ascii_case("unset") {
            return Ok(None);
        }
        ToolPermission::ALL
            .into_iter()
            .find(|p| p.label().eq_ignore_ascii_case(value))
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("permission must be allow, ask, deny or unset"))
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
    enum SkillAction {
        /// Show installed skills, whether each is approved, and problems.
        List,
        /// Print a skill's SKILL.md for review, followed by its digest.
        Show { name: String },
        /// Approve the skill exactly as reviewed: pass the digest `show` printed.
        Enable {
            name: String,
            #[arg(long)]
            digest: String,
        },
        /// Withdraw approval.
        Disable { name: String },
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
            /// Agent 206 backend only: read git history (git status, log, diff).
            #[arg(long)]
            git_read: bool,
            /// Agent 206 backend only: stage and commit (implies --git-read).
            #[arg(long)]
            git_write: bool,
            /// Agent 206 backend only: fetch from the network; commands may use it too.
            #[arg(long)]
            network: bool,
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
        Ok(Command::SaveSettings(Box::new(settings)))
    }
    /// Store each image with the gateway, which checks its format and size.
    async fn stage_images(
        client: &a206_runtime::client::Client,
        paths: &[PathBuf],
    ) -> anyhow::Result<Vec<a206_core::ImageAttachment>> {
        anyhow::ensure!(
            paths.len() <= a206_core::MAX_IMAGES,
            "attach at most {} images",
            a206_core::MAX_IMAGES
        );
        use anyhow::Context;
        let mut staged = vec![];
        for path in paths {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow::anyhow!("{} has no usable file name", path.display()))?
                .to_owned();
            let size = std::fs::metadata(path)
                .with_context(|| format!("could not read {}", path.display()))?
                .len();
            anyhow::ensure!(
                size <= a206_core::MAX_IMAGE_BYTES as u64,
                "{} is larger than 2 MiB",
                path.display()
            );
            let data = std::fs::read(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            match client
                .call(Command::StageImage { name, data })
                .await?
                .into_result()?
            {
                Reply::Image(image) => staged.push(image),
                _ => anyhow::bail!("unexpected reply while attaching {}", path.display()),
            }
        }
        Ok(staged)
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
        use a206_core::{GatewayConnection, GatewayProbe, GatewayTarget};
        use a206_runtime::connection;
        let target = match ssh {
            Some(endpoint) if !local => {
                let mut probe = connection::probe(root, &endpoint).await?;
                if let GatewayProbe::Unverified { keys } = &probe {
                    if !keys.iter().any(|key| trust.contains(&key.fingerprint)) {
                        println!("The host offers these keys:");
                        for key in keys {
                            println!("  {} {}", key.algorithm, key.fingerprint);
                        }
                        anyhow::bail!(
                            "verify a fingerprint with the server's administrator, then rerun with --trust <fingerprint>"
                        );
                    }
                    probe = connection::trust_fingerprints(root, &endpoint, trust).await?;
                }
                let GatewayProbe::Reachable { version } = probe else {
                    anyhow::bail!("the host key is still not trusted after recording it");
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
                    a206_ui::configure_terminal();
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
                if let Action::Web {
                    static_dir,
                    port,
                    listen,
                    names,
                    tls_cert,
                    tls_key,
                } = action
                {
                    use a206_runtime::web::{Certificate, Exposure, NetworkOptions};
                    let exposure = match listen {
                        None => Exposure::Loopback { port },
                        Some(listen) => Exposure::Network(NetworkOptions {
                            listen,
                            names,
                            certificate: match (tls_cert, tls_key) {
                                (Some(cert), Some(key)) => Certificate::Files { cert, key },
                                _ => Certificate::Generated,
                            },
                        }),
                    };
                    // The bridge offers first-time setup to the browser itself.
                    return Ok(a206_runtime::web::serve(
                        root.clone(),
                        args.remote.clone(),
                        static_dir,
                        exposure,
                    )
                    .await?);
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
                if let Action::Messenger { command } = action {
                    return crate::messenger_cli::run(&client, command).await;
                }
                if let Action::Schedule { command } = action {
                    return crate::schedule_cli::run(&client, command).await;
                }
                if let Action::Running = action {
                    return crate::running_cli::run(&client).await;
                }
                if let Action::Profiles = action {
                    return crate::profiles_cli::run(&client).await;
                }
                if let Action::Teams { command } = action {
                    return crate::teams_cli::run(&client, command).await;
                }
                if let Action::SearchServices { command } = action {
                    return crate::search_cli::run(&client, command).await;
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
                        Command::SaveSettings(Box::new(settings))
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
                        Command::SaveSettings(Box::new(settings))
                    }
                    Action::Backend {
                        command:
                            BackendAction::Select {
                                thread,
                                kind,
                                read,
                                write,
                                execute,
                                git_read,
                                git_write,
                                network,
                            },
                    } => {
                        let capabilities = [
                            (read || write, Capability::ReadWorkspace),
                            (write, Capability::WriteWorkspace),
                            (execute, Capability::Execute),
                            (git_read || git_write, Capability::GitRead),
                            (git_write, Capability::GitWrite),
                            (network, Capability::Network),
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
                    Action::Skills { command } => match command {
                        SkillAction::List => {
                            #[derive(serde::Serialize)]
                            struct Skills {
                                skills: Vec<a206_core::SkillSummary>,
                            }
                            match client.call(Command::Skills).await? {
                                Reply::Skills(skills) => {
                                    print!("{}", toml::to_string_pretty(&Skills { skills })?);
                                    return Ok(());
                                }
                                Reply::Error(report) => return Err(report.into()),
                                _ => anyhow::bail!("gateway did not list skills"),
                            }
                        }
                        SkillAction::Show { name } => {
                            match client.call(Command::ReadSkill { name }).await? {
                                Reply::Skill(skill) => {
                                    print!("{}", skill.text);
                                    for file in &skill.files {
                                        println!("\n--- {} ({} bytes) ---", file.path, file.bytes);
                                        match &file.text {
                                            Some(text) => println!("{text}"),
                                            None => println!("Binary content: not inspected by the text audit."),
                                        }
                                    }
                                    let audit = &skill.audit;
                                    if !audit.is_clean() {
                                        eprintln!("\nCheck before approving; the audit matched:");
                                        for finding in &audit.findings {
                                            if let a206_core::AuditLocation::Line(line) =
                                                finding.location
                                            {
                                                eprintln!("  line {line}: {}", finding.rule.label());
                                            } else if let a206_core::AuditLocation::File { path, line } = &finding.location {
                                                eprintln!("  {path}{}: {}", line.map(|line| format!(":{line}")).unwrap_or_default(), finding.rule.label());
                                            }
                                        }
                                        if audit.more > 0 {
                                            eprintln!("  and {} more", audit.more);
                                        }
                                    }
                                    eprintln!(
                                        "\nTo approve exactly this package: agent206 skills enable {} --digest {}",
                                        skill.name, skill.digest
                                    );
                                    return Ok(());
                                }
                                Reply::Error(report) => return Err(report.into()),
                                _ => anyhow::bail!("gateway did not return the skill"),
                            }
                        }
                        SkillAction::Enable { name, digest } => Command::SetSkillEnabled {
                            name,
                            enabled: true,
                            digest: Some(digest),
                        },
                        SkillAction::Disable { name } => Command::SetSkillEnabled {
                            name,
                            enabled: false,
                            digest: None,
                        },
                    },
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
                    Action::Chat {
                        thread,
                        text,
                        images,
                    } => Command::Send {
                        thread,
                        text,
                        attachments: vec![],
                        images: stage_images(&client, &images).await?,
                    },
                    Action::Steer { thread, text } => Command::Steer { thread, text },
                    Action::Pause { thread } => Command::PauseRun { thread },
                    Action::Resume { thread } => Command::ResumeRun { thread },
                    Action::Cancel { thread } => Command::CancelRun { thread },
                    Action::Compact { thread } => Command::CompactThread { thread },
                    Action::Tools { command } => match command {
                        ToolAction::List { thread } => Command::ListTools { thread },
                        ToolAction::Permit {
                            thread,
                            tool,
                            permission: value,
                        } => Command::SetToolPermission {
                            thread,
                            tool,
                            permission: permission(&value)?,
                        },
                        ToolAction::Budget {
                            thread,
                            rounds,
                            tool_calls,
                            tokens,
                        } => Command::SetToolBudget {
                            thread,
                            budget: ToolBudget {
                                rounds,
                                tool_calls,
                                tokens,
                            },
                        },
                    },
                    Action::Plan { thread } => {
                        let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await? else {
                            anyhow::bail!("failed to read the conversation");
                        };
                        let thread = snapshot
                            .threads
                            .into_iter()
                            .find(|t| t.id == thread)
                            .ok_or_else(|| anyhow::anyhow!("conversation not found"))?;
                        #[derive(serde::Serialize)]
                        struct Plan {
                            steps: Vec<a206_core::PlanStep>,
                        }
                        print!(
                            "{}",
                            toml::to_string_pretty(&Plan { steps: thread.plan })?
                        );
                        return Ok(());
                    }
                    Action::Usage { thread } => {
                        let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await? else {
                            anyhow::bail!("failed to read the conversation");
                        };
                        let own = snapshot
                            .threads
                            .iter()
                            .find(|t| t.id == thread)
                            .ok_or_else(|| anyhow::anyhow!("conversation not found"))?
                            .usage_total
                            .clone();
                        #[derive(serde::Serialize)]
                        struct Totals {
                            conversation: a206_core::UsageTotal,
                            with_subagents: a206_core::UsageTotal,
                        }
                        print!(
                            "{}",
                            toml::to_string_pretty(&Totals {
                                conversation: own,
                                with_subagents: a206_core::UsageTotal::with_subagents(
                                    &snapshot.threads,
                                    thread
                                ),
                            })?
                        );
                        return Ok(());
                    }
                    Action::Approvals { command } => match command {
                        ApprovalAction::List { thread } => {
                            let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await?
                            else {
                                anyhow::bail!("failed to read the conversation");
                            };
                            let thread = snapshot
                                .threads
                                .into_iter()
                                .find(|t| t.id == thread)
                                .ok_or_else(|| anyhow::anyhow!("conversation not found"))?;
                            #[derive(serde::Serialize)]
                            struct Pending {
                                approvals: Vec<ApprovalRequest>,
                            }
                            print!(
                                "{}",
                                toml::to_string_pretty(&Pending {
                                    approvals: thread.approvals
                                })?
                            );
                            return Ok(());
                        }
                        ApprovalAction::Approve {
                            thread,
                            approval,
                            digest,
                        } => Command::ResolveApproval {
                            thread,
                            approval,
                            digest,
                            decision: ApprovalDecision::Approve,
                        },
                        ApprovalAction::Deny {
                            thread,
                            approval,
                            digest,
                        } => Command::ResolveApproval {
                            thread,
                            approval,
                            digest,
                            decision: ApprovalDecision::Deny,
                        },
                    },
                    Action::Prompts { command } => match command {
                        PromptAction::List { thread } => {
                            let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await?
                            else {
                                anyhow::bail!("failed to read the conversation");
                            };
                            let thread = snapshot
                                .threads
                                .into_iter()
                                .find(|t| t.id == thread)
                                .ok_or_else(|| anyhow::anyhow!("conversation not found"))?;
                            #[derive(serde::Serialize)]
                            struct Pending {
                                prompts: Vec<a206_core::UserPrompt>,
                            }
                            print!(
                                "{}",
                                toml::to_string_pretty(&Pending {
                                    prompts: thread.prompts
                                })?
                            );
                            return Ok(());
                        }
                        PromptAction::Choose {
                            thread,
                            prompt,
                            choice,
                        } => Command::AnswerPrompt {
                            thread,
                            prompt,
                            answer: PromptAnswer::Choice {
                                index: choice.get() - 1,
                            },
                        },
                        PromptAction::Answer {
                            thread,
                            prompt,
                            text,
                        } => Command::AnswerPrompt {
                            thread,
                            prompt,
                            answer: PromptAnswer::Text { text },
                        },
                        PromptAction::Decline { thread, prompt } => Command::AnswerPrompt {
                            thread,
                            prompt,
                            answer: PromptAnswer::Decline,
                        },
                    },
                    Action::Settings { apply: Some(path) } => {
                        Command::SaveSettings(Box::new(toml::from_str(&std::fs::read_to_string(path)?)?))
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
                    Action::Search { query, limit } => Command::SearchMessages { query, limit },
                    Action::Activity { before, limit } => Command::Activity { before, limit },
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
                                /// A listed package with what the pre-install
                                /// audit found in its text; bundled ones have none.
                                #[derive(serde::Serialize)]
                                struct Audited<T> {
                                    #[serde(flatten)]
                                    item: T,
                                    #[serde(skip_serializing_if = "Option::is_none")]
                                    audit: Option<a206_core::AuditReport>,
                                }
                                #[derive(serde::Serialize)]
                                struct Extensions {
                                    extensions: Vec<Audited<a206_core::PluginSummary>>,
                                    #[serde(skip_serializing_if = "Vec::is_empty")]
                                    proposals: Vec<Audited<a206_core::ExtensionProposal>>,
                                }
                                let extensions = snapshot
                                    .plugins
                                    .into_iter()
                                    .map(|item| Audited {
                                        audit: (item.source != a206_core::PluginSource::Bundled)
                                            .then(|| a206_audit::plugin(&item)),
                                        item,
                                    })
                                    .collect();
                                let proposals = snapshot
                                    .extension_proposals
                                    .into_iter()
                                    .map(|item| Audited {
                                        audit: Some(a206_audit::plugin(&item.summary)),
                                        item,
                                    })
                                    .collect();
                                print!(
                                    "{}",
                                    toml::to_string_pretty(&Extensions {
                                        extensions,
                                        proposals,
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
                    Reply::Messages(hits) => {
                        #[derive(serde::Serialize)]
                        struct Hits {
                            hits: Vec<a206_core::MessageHit>,
                        }
                        print!("{}", toml::to_string_pretty(&Hits { hits })?);
                    }
                    Reply::Activity(page) => print!("{}", toml::to_string_pretty(&page)?),
                    reply => println!("{}", toml::to_string_pretty(&reply)?),
                }
                Ok(())
            }),
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    // The gateway re-runs this executable as its Linux sandbox helper; that
    // must happen before anything else starts threads.
    match a206_sandbox::dispatch() {
        None => {}
        Some(Ok(code)) => return code,
        Some(Err(error)) => {
            eprintln!("Agent 206 sandbox: {:#}", anyhow::Error::new(error));
            return std::process::ExitCode::FAILURE;
        }
    }
    if let Some(result) = a206_runtime::imports::dispatch() {
        return match result {
            Ok(code) => code,
            Err(error) => {
                eprintln!("Agent 206 import reader: {error:#}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    if let Err(error) = native::run() {
        eprintln!("Agent 206: {error:#}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
