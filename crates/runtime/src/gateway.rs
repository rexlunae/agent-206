//! Gateway owns state. Trusted UI commands and untrusted agent tools are separate APIs.
use crate::{
    provider::{self, now},
    store::Store,
    transport,
};
use a206_core::*;
use a206_protocol::{Request, Response};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::io::AsyncWriteExt;
#[derive(Default)]
struct Admission {
    draining: bool,
    active: usize,
}
struct ActiveRequest(Arc<Mutex<Admission>>);
impl Drop for ActiveRequest {
    fn drop(&mut self) {
        if let Ok(mut admission) = self.0.lock() {
            admission.active -= 1;
        }
    }
}
struct DrainAttempt {
    admission: Arc<Mutex<Admission>>,
    committed: bool,
}
/// Dropping a freshly committed acknowledgement also completes shutdown when
/// its client disconnects or the writer cannot deliver earlier queued replies.
struct StopAfterReply(Option<tokio_util::sync::CancellationToken>);
impl Drop for StopAfterReply {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.cancel();
        }
    }
}
impl DrainAttempt {
    fn begin(admission: &Arc<Mutex<Admission>>) -> Result<Self> {
        let mut state = admission
            .lock()
            .map_err(|_| crate::error::poisoned("gateway admission"))?;
        ensure!(!state.draining, "gateway shutdown is already complete");
        state.draining = true;
        Ok(Self {
            admission: admission.clone(),
            committed: false,
        })
    }
}
impl Drop for DrainAttempt {
    fn drop(&mut self) {
        if !self.committed
            && let Ok(mut admission) = self.admission.lock()
        {
            admission.draining = false;
        }
    }
}
#[derive(Clone)]
pub struct Gateway {
    admission: Arc<Mutex<Admission>>,
    shutdown_committed: Arc<tokio::sync::Mutex<bool>>,
    started: std::time::Instant,
    stop: tokio_util::sync::CancellationToken,
    joshua: Arc<tokio::sync::Mutex<crate::joshua::Manager>>,
    store: Arc<Mutex<Store>>,
    downloads: Arc<Mutex<BTreeMap<Id, tokio_util::sync::CancellationToken>>>,
    running: Arc<Mutex<BTreeMap<Id, tokio_util::sync::CancellationToken>>>,
}
impl Gateway {
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            admission: Arc::new(Mutex::new(Admission::default())),
            shutdown_committed: Arc::new(tokio::sync::Mutex::new(false)),
            started: std::time::Instant::now(),
            stop: tokio_util::sync::CancellationToken::new(),
            joshua: Arc::new(tokio::sync::Mutex::new(crate::joshua::Manager::default())),
            store: Arc::new(Mutex::new(Store::open(root)?)),
            downloads: Arc::new(Mutex::new(BTreeMap::new())),
            running: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, Store>> {
        self.store
            .lock()
            .map_err(|_| crate::error::poisoned("state"))
    }
    pub fn stop_signal(&self) -> tokio_util::sync::CancellationToken {
        self.stop.clone()
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        Ok(self.lock()?.state.clone())
    }
    fn clear_managed_joshua(&self) -> Result<()> {
        let mut store = self.lock()?;
        let Some(provider) = store.state.managed_joshua_provider else {
            return Ok(());
        };
        let running = self
            .running
            .lock()
            .map_err(|_| crate::error::poisoned("run"))?;
        for thread in &store.state.threads {
            if thread
                .model
                .as_ref()
                .is_some_and(|key| key.provider == provider)
                && let Some(token) = running.get(&thread.id)
            {
                token.cancel();
            }
        }
        store.state.clear_managed_joshua();
        store.persist(Some(EventKind::SettingsChanged))?;
        Ok(())
    }
    pub async fn request(&self, request: Request<Command>) -> Response<Reply> {
        self.request_with_effect(request).await.0
    }
    /// The stop effect belongs only to fresh execution, never a durable replay.
    async fn request_with_effect(&self, request: Request<Command>) -> (Response<Reply>, bool) {
        let id = request.id;
        let operation = request.body.name();
        let span = tracing::info_span!("request", %id, operation);
        let (body, stop) = tracing::Instrument::instrument(self.execute(request), span).await;
        let body = match body {
            Ok(reply) => reply,
            Err(error) => Reply::Error(crate::error::handle(operation, &error)),
        };
        (Response { id, body }, stop)
    }
    async fn execute(&self, request: Request<Command>) -> (Result<Reply>, bool) {
        let id = request.id;
        let operation = request.body.name();
        let is_shutdown = matches!(request.body, Command::Shutdown);
        let _active = if !matches!(
            request.body,
            Command::Snapshot | Command::GatewayStatus | Command::Shutdown | Command::Events { .. }
        ) {
            let Ok(mut admission) = self.admission.lock() else {
                return (Err(crate::error::poisoned("gateway admission")), false);
            };
            if admission.draining {
                return (
                    Err(ErrorReport::new(
                        ErrorKind::Unavailable,
                        "gateway is stopping; start it again before submitting work",
                    )
                    .into()),
                    false,
                );
            }
            admission.active += 1;
            Some(ActiveRequest(self.admission.clone()))
        } else {
            None
        };
        let mutation = !matches!(
            request.body,
            Command::Snapshot
                | Command::GatewayStatus
                | Command::JoshuaStatus
                | Command::Events { .. }
                | Command::GitStatus { .. }
                | Command::ReadDocument { .. }
                | Command::RunExtension { .. }
        );
        let begin = if mutation {
            self.lock()
                .and_then(|mut store| store.begin(id, &request.body))
        } else {
            Ok(None)
        };
        match begin {
            Ok(Some(reply)) => (Ok(reply), false),
            Err(error) => (Err(error), false),
            Ok(None) => {
                // A failed command is a reply: persist its report as the receipt.
                let reply = match self.handle(request.body).await {
                    Ok(reply) => reply,
                    Err(error) => Reply::Error(crate::error::handle(operation, &error)),
                };
                let stop = is_shutdown && matches!(reply, Reply::Ok);
                if mutation && let Err(error) = self.lock().and_then(|s| s.finish(id, &reply)) {
                    return (
                        Err(error
                            .context("operation completed but its receipt could not be persisted")),
                        stop,
                    );
                }
                (Ok(reply), stop)
            }
        }
    }
    async fn handle(&self, command: Command) -> Result<Reply> {
        match command {
            Command::GatewayStatus => Ok(Reply::Gateway(GatewayInfo {
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").into(),
                protocol: a206_protocol::VERSION,
                uptime_seconds: self.started.elapsed().as_secs(),
                data_dir: self.lock()?.root.display().to_string(),
                active_runs: self
                    .running
                    .lock()
                    .map_err(|_| crate::error::poisoned("run"))?
                    .len(),
                active_downloads: self
                    .downloads
                    .lock()
                    .map_err(|_| crate::error::poisoned("download"))?
                    .len(),
                accepting_work: Some(
                    !self
                        .admission
                        .lock()
                        .map_err(|_| crate::error::poisoned("gateway admission"))?
                        .draining,
                ),
            })),
            Command::Snapshot => Ok(Reply::Snapshot(Box::new(self.snapshot()?))),
            Command::JoshuaStatus => {
                let mut manager = self.joshua.lock().await;
                let status = manager.status()?;
                if !manager.is_running() {
                    self.clear_managed_joshua()?;
                }
                Ok(Reply::Text(status))
            }
            Command::StartJoshua {
                label,
                model_file,
                port,
                remote,
            } => {
                let root = self.lock()?.root.clone();
                let mut manager = self.joshua.lock().await;
                manager.status()?;
                if !manager.is_running() {
                    self.clear_managed_joshua()?;
                }
                let provider = manager
                    .start(label, model_file, port, remote, &root)
                    .await?;
                let id = provider.id;
                let persisted = (|| -> Result<()> {
                    let mut store = self.lock()?;
                    store.state.settings.providers.push(provider);
                    store.state.managed_joshua_provider = Some(id);
                    store.state.settings.revision += 1;
                    store.persist(Some(EventKind::SettingsChanged))?;
                    Ok(())
                })();
                if let Err(error) = persisted {
                    manager.stop().await?;
                    return Err(error);
                }
                Ok(Reply::Text(format!(
                    "Joshua started. Refresh the new connection to discover its models. Provider: {id}"
                )))
            }
            Command::StopJoshua => {
                let mut manager = self.joshua.lock().await;
                manager.stop().await?;
                self.clear_managed_joshua()?;
                Ok(Reply::Ok)
            }
            Command::Events { after } => Ok(Reply::Events(self.lock()?.events(after)?)),
            Command::SaveSettings(mut settings) => {
                provider::validate_presets(&settings.provider_presets)?;
                let mut backend_kinds = BTreeSet::new();
                for backend in &settings.backends {
                    crate::backends::validate_config(backend)?;
                    ensure!(
                        backend_kinds.insert(backend.kind.label()),
                        "duplicate coding backend configuration"
                    );
                }
                let mut ids = BTreeSet::new();
                for config in &settings.providers {
                    ensure!(ids.insert(config.id), "duplicate provider id");
                    provider::endpoint(config, "models")?;
                    ensure!(
                        !config.label.trim().is_empty(),
                        "provider label is required"
                    );
                }
                for mode in Mode::ALL {
                    ensure!(
                        settings.prompts.iter().filter(|p| p.mode == mode).count() == 1,
                        "each mode needs exactly one prompt"
                    );
                }
                let mut store = self.lock()?;
                ensure!(
                    settings.revision == store.state.settings.revision,
                    "settings changed in another operation; refresh and retry your change"
                );
                if let Some(id) = store.state.managed_joshua_provider {
                    let owned = store.state.settings.providers.iter().find(|provider| provider.id == id)
                        .context("managed Joshua connection is missing; stop Joshua before applying settings")?;
                    ensure!(
                        settings
                            .providers
                            .iter()
                            .find(|provider| provider.id == id)
                            .is_some_and(|provider| provider.base_url == owned.base_url
                                && provider.api == owned.api
                                && provider.credential == owned.credential),
                        "stop managed Joshua before removing its connection or changing its endpoint, protocol or credentials"
                    );
                }
                // Revocation also stops already-running inference and coding backends.
                let running = self
                    .running
                    .lock()
                    .map_err(|_| crate::error::poisoned("run"))?;
                for thread in &store.state.threads {
                    let provider_changed = thread.model.as_ref().is_some_and(|key| {
                        let previous = store
                            .state
                            .settings
                            .providers
                            .iter()
                            .find(|p| p.id == key.provider);
                        settings.providers.iter().find(|p| p.id == key.provider) != previous
                    });
                    let backend_changed = thread.backend != CodingBackend::Native
                        && settings.backends.iter().find(|b| b.kind == thread.backend)
                            != store
                                .state
                                .settings
                                .backends
                                .iter()
                                .find(|b| b.kind == thread.backend);
                    if (provider_changed || backend_changed)
                        && let Some(token) = running.get(&thread.id)
                    {
                        token.cancel();
                    }
                }
                drop(running);
                settings.revision += 1;
                let text = toml::to_string_pretty(&settings)?;
                let temporary = store.root.join("settings.toml.new");
                std::fs::write(&temporary, text)?;
                std::fs::File::open(&temporary)?.sync_all()?;
                std::fs::rename(&temporary, store.root.join("settings.toml"))?;
                // Endpoint/credential changes invalidate any facts from the previous source.
                let old = store.state.settings.providers.clone();
                store.state.catalogs.retain(|c| {
                    settings
                        .providers
                        .iter()
                        .any(|p| p.id == c.provider && old.contains(p))
                });
                store.state.settings = settings;
                store.persist(Some(EventKind::SettingsChanged))?;
                Ok(Reply::Ok)
            }
            Command::RefreshCatalog { provider } => {
                let config = self
                    .lock()?
                    .state
                    .settings
                    .providers
                    .iter()
                    .find(|p| p.id == provider)
                    .cloned()
                    .context("provider not configured")?;
                let catalog = provider::catalog(&config).await?;
                let mut store = self.lock()?;
                ensure!(
                    store.state.settings.providers.contains(&config),
                    "provider configuration changed during refresh"
                );
                store.state.catalogs.retain(|c| c.provider != provider);
                store.state.catalogs.push(catalog.clone());
                store.persist(Some(EventKind::CatalogChanged { provider }))?;
                Ok(Reply::Catalog(catalog))
            }
            Command::CreateThread { title, mode } => {
                ensure!(
                    !title.trim().is_empty() && title.len() <= 512,
                    "thread title must be 1–512 bytes"
                );
                let thread = Thread::new(title, mode);
                let mut store = self.lock()?;
                store.state.threads.push(thread.clone());
                store.persist(Some(EventKind::ThreadChanged { id: thread.id }))?;
                Ok(Reply::Thread(thread))
            }
            Command::SetSubagentModel { id, model, allowed } => {
                let mut store = self.lock()?;
                if allowed {
                    validate_model(&store.state, &model)?;
                }
                let thread = store.thread_mut(id)?;
                if allowed {
                    thread.subagent_models.insert(model.clone());
                    thread.grant.models.insert(model.clone());
                } else {
                    thread.subagent_models.remove(&model);
                    if thread.model.as_ref() != Some(&model) {
                        thread.grant.models.remove(&model);
                    }
                }
                if !allowed {
                    let running = self
                        .running
                        .lock()
                        .map_err(|_| crate::error::poisoned("run"))?;
                    for child in store.state.threads.iter().filter(|child| {
                        child.parent == Some(id) && child.model.as_ref() == Some(&model)
                    }) {
                        if let Some(token) = running.get(&child.id) {
                            token.cancel();
                        }
                    }
                }
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                Ok(Reply::Ok)
            }
            Command::RenameThread { id, title } => {
                ensure!(
                    !title.trim().is_empty() && title.len() <= 512,
                    "thread title must be 1–512 bytes"
                );
                let mut store = self.lock()?;
                store.thread_mut(id)?.title = title;
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                Ok(Reply::Ok)
            }
            Command::ConfigureThread {
                id,
                mode,
                model,
                subagent_models,
            } => {
                let mut store = self.lock()?;
                for key in model.iter().chain(&subagent_models) {
                    validate_model(&store.state, key)?;
                }
                let thread = store.thread_mut(id)?;
                ensure!(
                    thread.state != RunState::Running,
                    "wait for the current response before changing its configuration"
                );
                thread.mode = mode;
                if thread.model != model {
                    thread.effort = None;
                    thread.usage = None;
                }
                thread.model = model;
                thread.subagent_models = subagent_models;
                thread.grant.models = thread.subagent_models.clone();
                if let Some(model) = &thread.model {
                    thread.grant.models.insert(model.clone());
                }
                thread.grant.capabilities = thread
                    .grant
                    .capabilities
                    .intersection(&mode.ceiling())
                    .copied()
                    .collect();
                let thread = thread.clone();
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                Ok(Reply::Thread(thread))
            }
            Command::SetEffort { thread: id, effort } => {
                let mut store = self.lock()?;
                let thread = store.thread(id)?;
                ensure!(
                    thread.state != RunState::Running,
                    "wait for the response before changing effort"
                );
                validate_effort(&store.state, thread.model.as_ref(), effort.as_deref())?;
                store.thread_mut(id)?.effort = effort;
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                Ok(Reply::Ok)
            }
            Command::SetBackend {
                thread: id,
                backend,
                capabilities,
            } => {
                let mut store = self.lock()?;
                let thread = store.thread(id)?;
                ensure!(
                    thread.state != RunState::Running,
                    "stop the run before changing backend permissions"
                );
                ensure!(
                    thread.parent.is_none(),
                    "delegated threads cannot change backend grants"
                );
                let supported: BTreeSet<_> = [
                    Capability::ReadWorkspace,
                    Capability::WriteWorkspace,
                    Capability::Execute,
                ]
                .into();
                ensure!(
                    capabilities.is_subset(&supported)
                        && capabilities.is_subset(&thread.mode.ceiling()),
                    "backend capability is unavailable in this mode"
                );
                ensure!(
                    !capabilities.contains(&Capability::WriteWorkspace)
                        || capabilities.contains(&Capability::ReadWorkspace),
                    "write access also requires read access"
                );
                ensure!(
                    capabilities.is_empty() || thread.project.is_some(),
                    "associate a project before granting workspace tools"
                );
                if backend != CodingBackend::Native {
                    ensure!(
                        store
                            .state
                            .settings
                            .backends
                            .iter()
                            .any(|b| b.kind == backend),
                        "configure the coding backend in Connections first"
                    );
                }
                let thread = store.thread_mut(id)?;
                thread.backend = backend;
                thread.backend_activity = None;
                thread.grant.capabilities = capabilities;
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                Ok(Reply::Ok)
            }
            Command::Send {
                thread: id,
                text,
                attachments,
            } => {
                ensure!(
                    attachments.len() <= 8,
                    "attach at most eight text documents"
                );
                let attachment_size: usize = attachments.iter().map(|a| a.text.len()).sum();
                ensure!(
                    text.len() + attachment_size <= 128 * 1024,
                    "message and attachments exceed 128 KiB"
                );
                for attachment in &attachments {
                    ensure!(
                        !attachment.name.is_empty()
                            && attachment.name.len() <= 255
                            && !attachment.name.chars().any(char::is_control),
                        "invalid attachment name"
                    );
                    ensure!(
                        !attachment.text.contains('\0'),
                        "only UTF-8 text attachments are supported"
                    );
                }
                ensure!(
                    (!text.trim().is_empty() || !attachments.is_empty())
                        && text.len() <= 128 * 1024,
                    "message must be 1–131072 bytes"
                );
                let token = tokio_util::sync::CancellationToken::new();
                let (config, thread, prompt, info, backend, workspace, root) = {
                    let mut store = self.lock()?;
                    anyhow::ensure!(
                        rmp_serde::to_vec_named(&store.state)?.len()
                            + text.len()
                            + attachment_size
                            + 262_144
                            < a206_protocol::MAX_FRAME,
                        "this preview reached its history capacity; export the state directory before continuing in a new profile"
                    );
                    let current = store.thread(id)?;
                    ensure!(
                        current.state != RunState::Running,
                        "thread is already running"
                    );
                    let key = current
                        .model
                        .as_ref()
                        .context("connect a provider and select a model first")?;
                    validate_model(&store.state, key)?;
                    validate_effort(&store.state, Some(key), current.effort.as_deref())?;
                    let info = store
                        .state
                        .catalogs
                        .iter()
                        .flat_map(|c| &c.models)
                        .find(|m| &m.key == key)
                        .cloned()
                        .context("model metadata missing")?;
                    if let Some(parent) = current.parent {
                        ensure!(
                            store.thread(parent)?.subagent_models.contains(key),
                            "parent revoked this subagent model"
                        );
                    }
                    let config = store
                        .state
                        .settings
                        .providers
                        .iter()
                        .find(|p| p.id == key.provider)
                        .cloned()
                        .context("provider not configured")?;
                    let prompt = store
                        .state
                        .settings
                        .prompts
                        .iter()
                        .find(|p| p.mode == current.mode)
                        .map(|p| p.prompt.clone())
                        .context("mode prompt missing")?;
                    let backend = if current.backend == CodingBackend::Native {
                        None
                    } else {
                        let backend = store
                            .state
                            .settings
                            .backends
                            .iter()
                            .find(|b| b.kind == current.backend)
                            .cloned()
                            .context("coding backend is not configured")?;
                        crate::backends::validate_selection(&config, current, &backend)?;
                        Some(backend)
                    };
                    let workspace = current
                        .project
                        .and_then(|id| store.state.projects.iter().find(|p| p.id == id))
                        .map(|p| PathBuf::from(&p.root));
                    let root = store.root.clone();
                    let thread = store.thread_mut(id)?;
                    thread.messages.push(Message {
                        id: Id::new_v4(),
                        role: Role::User,
                        text,
                        attachments,
                        at: now(),
                        backend: thread.backend,
                    });
                    thread.state = RunState::Running;
                    thread.backend_activity = None;
                    thread.error = None;
                    let copy = thread.clone();
                    self.running
                        .lock()
                        .map_err(|_| crate::error::poisoned("run"))?
                        .insert(id, token.clone());
                    store.persist(Some(EventKind::ThreadChanged { id }))?;
                    (config, copy, prompt, info, backend, workspace, root)
                };
                let (sender, mut progress) = tokio::sync::mpsc::channel(32);
                let operation = async {
                    if let Some(backend) = &backend {
                        crate::backends::complete(crate::backends::Run {
                            config: &config,
                            thread: &thread,
                            prompt: &prompt,
                            info: &info,
                            backend,
                            workspace: workspace.as_deref(),
                            state_root: &root,
                            progress: sender,
                            cancel: token.clone(),
                        })
                        .await
                    } else {
                        provider::complete(&config, &thread, &prompt, &info).await
                    }
                };
                let mut operation = Box::pin(operation);
                let mut progress_open = true;
                let mut partial_message = None;
                let mut activity_count = 0;
                let mut last_progress =
                    std::time::Instant::now() - std::time::Duration::from_secs(1);
                let result = loop {
                    tokio::select! {
                        result = &mut operation => break (!token.is_cancelled()).then_some(result),
                        _ = token.cancelled(), if backend.is_none() => break None,
                        update = progress.recv(), if progress_open => {
                            let Some(update) = update else { progress_open = false; continue; };
                            let mut store = self.lock()?;
                            match update {
                                crate::backends::Progress::Text(text) => {
                                    if last_progress.elapsed() < std::time::Duration::from_millis(100) { continue; }
                                    last_progress = std::time::Instant::now();
                                    let thread = store.thread_mut(id)?;
                                    let message = partial_message.get_or_insert_with(Id::new_v4);
                                    if let Some(existing) = thread.messages.iter_mut().find(|m| m.id == *message) { existing.text = text; }
                                    else { thread.messages.push(Message { id: *message, role: Role::Assistant, text, attachments: vec![], at: now(), backend: thread.backend }); }
                                    store.persist(Some(EventKind::ThreadChanged { id }))?;
                                }
                                crate::backends::Progress::Activity(title) if activity_count < 128 => {
                                    activity_count += 1;
                                    store.thread_mut(id)?.backend_activity = Some(title.clone());
                                    store.persist(Some(EventKind::BackendActivity { thread: id, title }))?;
                                }
                                _ => {}
                            }
                        }
                    }
                };
                // External cancellation returns only after the proxy closes and
                // the isolated process group is killed and its root reaped.
                drop(operation);
                self.running
                    .lock()
                    .map_err(|_| crate::error::poisoned("run"))?
                    .remove(&id);
                let mut store = self.lock()?;
                let thread = store.thread_mut(id)?;
                match result {
                    Some(Ok((text, usage))) => {
                        if let Some(message) = partial_message
                            .and_then(|id| thread.messages.iter_mut().find(|m| m.id == id))
                        {
                            message.text = text;
                        } else {
                            thread.messages.push(Message {
                                id: Id::new_v4(),
                                role: Role::Assistant,
                                text,
                                attachments: vec![],
                                at: now(),
                                backend: thread.backend,
                            });
                        }
                        thread.usage = Some(usage);
                        thread.state = RunState::Complete;
                    }
                    Some(Err(e)) => {
                        let report = crate::error::handle("run", &e);
                        thread.state = RunState::Failed;
                        thread.messages.push(Message {
                            id: Id::new_v4(),
                            role: Role::System,
                            attachments: vec![],
                            text: report.detail(),
                            at: now(),
                            backend: thread.backend,
                        });
                        thread.error = Some(report);
                    }
                    None => {
                        thread.state = RunState::Cancelled;
                    }
                }
                let thread = thread.clone();
                store.persist(Some(EventKind::ThreadChanged { id }))?;
                Ok(Reply::Thread(thread))
            }
            Command::CancelRun { thread } => {
                let running = self
                    .running
                    .lock()
                    .map_err(|_| crate::error::poisoned("run"))?;
                let token = running
                    .get(&thread)
                    .ok_or_else(|| crate::error::not_found("thread is not running"))?;
                token.cancel();
                Ok(Reply::Ok)
            }
            Command::CreateProject { name, root } => {
                ensure!(!name.trim().is_empty(), "project name is required");
                let root = std::fs::canonicalize(root)?;
                ensure!(root.is_dir(), "workspace must be a directory");
                let id = Id::new_v4();
                let mut manager = Thread::new(format!("{name} · coordinator"), Mode::Plan);
                manager.project = Some(id);
                let project = Project {
                    id,
                    name,
                    root: root.to_string_lossy().into(),
                    manager: manager.id,
                };
                let mut store = self.lock()?;
                store.state.projects.push(project.clone());
                store.state.threads.push(manager);
                store.persist(Some(EventKind::ProjectChanged { id }))?;
                Ok(Reply::Project(project))
            }
            Command::CreateSubagent {
                parent,
                title,
                mode,
                model,
                requested,
            } => {
                let mut store = self.lock()?;
                validate_model(&store.state, &model)?;
                let parent_thread = store.thread(parent)?.clone();
                ensure!(
                    parent_thread.subagent_models.contains(&model),
                    "model is not permitted for this thread's subagents"
                );
                let mut thread = Thread::new(title, mode);
                thread.project = parent_thread.project;
                thread.parent = Some(parent);
                thread.model = Some(model.clone());
                thread.grant = parent_thread.grant.attenuate(
                    mode,
                    &Grant {
                        capabilities: requested,
                        models: [model].into(),
                    },
                    &mode.ceiling(),
                );
                store.state.threads.push(thread.clone());
                store.persist(Some(EventKind::ThreadChanged { id: thread.id }))?;
                Ok(Reply::Thread(thread))
            }
            Command::Download {
                url,
                name,
                sha256,
                models_directory,
            } => {
                self.start_download(url, name, sha256, models_directory)
                    .await
            }
            Command::CancelDownload { id } => {
                let mut store = self.lock()?;
                let download = store
                    .state
                    .downloads
                    .iter_mut()
                    .find(|d| d.id == id)
                    .ok_or_else(|| crate::error::not_found("download not found"))?;
                ensure!(
                    matches!(
                        download.state,
                        DownloadState::Running | DownloadState::Queued
                    ),
                    "download is no longer running"
                );
                download.state = DownloadState::Cancelled;
                if let Some(token) = self
                    .downloads
                    .lock()
                    .map_err(|_| crate::error::poisoned("transfer"))?
                    .get(&id)
                {
                    token.cancel();
                }
                let event = download.clone();
                store.persist(Some(EventKind::DownloadChanged(event)))?;
                Ok(Reply::Ok)
            }
            Command::GitStatus { project } => {
                let root = self.project_root(project)?;
                let output = crate::process::output(
                    tokio::process::Command::new("git")
                        .args([
                            "--no-pager",
                            "--no-optional-locks",
                            "-c",
                            "core.fsmonitor=false",
                            "status",
                            "--short",
                            "--branch",
                        ])
                        .env_remove("GIT_DIR")
                        .env_remove("GIT_WORK_TREE")
                        .env_remove("GIT_INDEX_FILE")
                        .current_dir(root),
                    512 * 1024,
                    std::time::Duration::from_secs(15),
                )
                .await?;
                Ok(Reply::Text(String::from_utf8_lossy(&output).into_owned()))
            }
            Command::ReadDocument { project, path } => {
                let root = self.project_root(project)?;
                let text = tokio::task::spawn_blocking(move || -> Result<String> {
                    use std::io::Read;
                    let directory =
                        cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
                    let file = directory
                        .open(path)
                        .context("document is missing or outside the project")?;
                    let mut text = String::new();
                    file.take(512 * 1024 + 1)
                        .read_to_string(&mut text)
                        .context("this viewer supports UTF-8 text and Markdown")?;
                    ensure!(
                        text.len() <= 512 * 1024,
                        "text preview is limited to 512 KiB"
                    );
                    Ok(text)
                })
                .await??;
                Ok(Reply::Text(text))
            }
            Command::RunExtension { id, input } => {
                ensure!(input.is_empty(), "ABI v1 does not accept input yet");
                let (manifest, wasm) = a206_extensions::builtin()?;
                ensure!(
                    manifest.id == id && manifest.capabilities.is_empty(),
                    "extension is not enabled"
                );
                let _validated_wasm = wasm;
                let child = tokio::process::Command::new(std::env::current_exe()?)
                    .arg("--data-dir")
                    .arg(self.lock()?.root.clone())
                    .arg("extension-worker")
                    .env_clear()
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()?;
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    child.wait_with_output(),
                )
                .await??;
                ensure!(result.status.success(), "extension worker failed");
                let output: Vec<(String, String)> = a206_protocol::decode_payload(&result.stdout)?;
                Ok(Reply::Text(
                    output
                        .into_iter()
                        .map(|(_, value)| value)
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                ))
            }
            Command::Shutdown => {
                // Serialize independent shutdown attempts so a failed one's
                // rollback cannot reopen admission during another one's cleanup.
                let mut committed = self.shutdown_committed.lock().await;
                if *committed {
                    return Ok(Reply::Ok);
                }
                let mut drain = DrainAttempt::begin(&self.admission)?;
                tokio::time::timeout(std::time::Duration::from_secs(15), async {
                    loop {
                        // Admission is closed. Repeat cancellation for requests
                        // admitted before stop that only now acquired their task token.
                        for token in self
                            .running
                            .lock()
                            .map_err(|_| crate::error::poisoned("run"))?
                            .values()
                        {
                            token.cancel();
                        }
                        for token in self
                            .downloads
                            .lock()
                            .map_err(|_| crate::error::poisoned("download"))?
                            .values()
                        {
                            token.cancel();
                        }
                        let runs = self
                            .running
                            .lock()
                            .map_err(|_| crate::error::poisoned("run"))?
                            .len();
                        let downloads = self
                            .downloads
                            .lock()
                            .map_err(|_| crate::error::poisoned("download"))?
                            .len();
                        let active = self
                            .admission
                            .lock()
                            .map_err(|_| crate::error::poisoned("gateway admission"))?
                            .active;
                        if active == 0 && runs == 0 && downloads == 0 {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                    self.joshua.lock().await.stop().await?;
                    self.clear_managed_joshua()?;
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("gateway cleanup timed out; service remains running")??;
                // Persist user intent before acknowledging stop. Ordinary
                // clients cannot resurrect a deliberately stopped service.
                crate::service::mark_stopped(&self.lock()?.root)?;
                drain.committed = true;
                *committed = true;
                Ok(Reply::Ok)
            }
        }
    }
    fn project_root(&self, id: Id) -> Result<PathBuf> {
        Ok(self
            .lock()?
            .state
            .projects
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| crate::error::not_found("project not found"))?
            .root
            .clone()
            .into())
    }
    async fn start_download(
        &self,
        url: String,
        name: String,
        sha256: Option<String>,
        models_directory: bool,
    ) -> Result<Reply> {
        let parsed = url::Url::parse(&url)?;
        ensure!(
            parsed.scheme() == "https"
                && parsed.username().is_empty()
                && parsed.password().is_none(),
            "downloads require credential-free HTTPS URLs"
        );
        ensure!(
            !name.is_empty()
                && name.len() < 200
                && !name.starts_with('.')
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "use a simple filename without directories"
        );
        if let Some(hash) = &sha256 {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "SHA-256 must contain 64 hexadecimal characters"
            );
        }
        let download = Download {
            id: Id::new_v4(),
            name,
            source: parsed.origin().ascii_serialization(),
            received: 0,
            total: None,
            state: DownloadState::Queued,
            error: None,
            sha256,
        };
        let target = {
            let mut store = self.lock()?;
            let target = if models_directory {
                crate::joshua::models_directory()?
            } else {
                store.root.join("downloads")
            };
            std::fs::create_dir_all(&target)?;
            ensure!(
                !target.join(&download.name).exists(),
                "destination already exists"
            );
            ensure!(
                !store.state.downloads.iter().any(|d| d.name == download.name
                    && matches!(d.state, DownloadState::Running | DownloadState::Queued)),
                "filename already has an active transfer"
            );
            store.state.downloads.push(download.clone());
            store.persist(Some(EventKind::DownloadChanged(download.clone())))?;
            target
        };
        let token = tokio_util::sync::CancellationToken::new();
        self.downloads
            .lock()
            .map_err(|_| crate::error::poisoned("transfer"))?
            .insert(download.id, token.clone());
        let gateway = self.clone();
        let job = download.clone();
        tokio::spawn(async move {
            let result = tokio::select! { result = gateway.transfer(&job, &target, &url) => result, _ = token.cancelled() => Err(ErrorReport::new(ErrorKind::Cancelled, "download cancelled").into()) };
            if let Err(error) = result {
                let report = crate::error::report(&error);
                let update = gateway.update_download(job.id, |d| {
                    if d.state != DownloadState::Cancelled {
                        d.state = DownloadState::Failed;
                        crate::error::record("download", &report);
                        d.error = Some(report);
                    }
                });
                if let Err(error) = update {
                    crate::error::handle(
                        "download",
                        &error.context("could not persist failed download state"),
                    );
                }
            }
            if let Ok(mut tokens) = gateway.downloads.lock() {
                tokens.remove(&job.id);
            }
        });
        Ok(Reply::Download(download))
    }
    fn update_download(&self, id: Id, update: impl FnOnce(&mut Download)) -> Result<Download> {
        let mut store = self.lock()?;
        let download = store
            .state
            .downloads
            .iter_mut()
            .find(|d| d.id == id)
            .ok_or_else(|| crate::error::not_found("download disappeared"))?;
        update(download);
        let copy = download.clone();
        store.persist(Some(EventKind::DownloadChanged(copy.clone())))?;
        Ok(copy)
    }
    async fn transfer(&self, download: &Download, target: &Path, source: &str) -> Result<()> {
        let response = crate::network::public_get(source).await?;
        self.transfer_response(download, target, response).await
    }
    async fn transfer_response(
        &self,
        download: &Download,
        target: &Path,
        response: reqwest::Response,
    ) -> Result<()> {
        let total = response.content_length();
        let updated = self.update_download(download.id, |d| {
            if d.state != DownloadState::Cancelled {
                d.state = DownloadState::Running;
                d.total = total;
            }
        })?;
        ensure!(
            updated.state != DownloadState::Cancelled,
            "download cancelled"
        );
        // Create synchronously under an owning guard: cancellation must not race an
        // unfinished asynchronous open, leaving a file with no cleanup owner.
        let partial = tempfile::Builder::new()
            .prefix(&format!("{}.{}.", download.name, download.id))
            .suffix(".part")
            .tempfile_in(target)?;
        let mut file = tokio::fs::File::from_std(partial.reopen()?);
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut received = 0;
        let mut last_event = std::time::Instant::now();
        while let Some(bytes) = stream.next().await {
            let bytes = bytes.map_err(reqwest::Error::without_url)?;
            let cancelled = self
                .lock()?
                .state
                .downloads
                .iter()
                .any(|d| d.id == download.id && d.state == DownloadState::Cancelled);
            ensure!(!cancelled, "download cancelled");
            file.write_all(&bytes).await?;
            hasher.update(&bytes);
            received += bytes.len() as u64;
            if last_event.elapsed() >= std::time::Duration::from_millis(250) {
                self.update_download(download.id, |d| d.received = received)?;
                last_event = std::time::Instant::now();
            }
        }
        file.sync_all().await?;
        drop(file);
        let digest = format!("{:x}", hasher.finalize());
        ensure!(
            download
                .sha256
                .as_ref()
                .is_none_or(|expected| expected.eq_ignore_ascii_case(&digest)),
            "download integrity check failed"
        );
        ensure!(
            total.is_none_or(|total| total == received),
            "download length mismatch"
        );
        // Publication is serialized with cancellation. hard_link refuses to replace an existing target.
        let mut store = self.lock()?;
        let current = store
            .state
            .downloads
            .iter_mut()
            .find(|d| d.id == download.id)
            .ok_or_else(|| crate::error::not_found("download disappeared"))?;
        ensure!(
            current.state != DownloadState::Cancelled,
            "download cancelled"
        );
        partial
            .persist_noclobber(target.join(&download.name))
            .map_err(|error| error.error)?;
        current.received = received;
        current.sha256 = Some(digest);
        current.state = DownloadState::Complete;
        let event = current.clone();
        store.persist(Some(EventKind::DownloadChanged(event)))?;
        Ok(())
    }
}
pub fn scoped_path(root: &Path, path: &str) -> Result<PathBuf> {
    ensure!(
        !Path::new(path).is_absolute(),
        "document path must be relative to the project"
    );
    let root = root.canonicalize()?;
    let target = root.join(path).canonicalize()?;
    ensure!(target.starts_with(&root), "document escapes the project");
    Ok(target)
}
fn validate_model(snapshot: &Snapshot, key: &ModelKey) -> Result<()> {
    ensure!(
        snapshot
            .catalogs
            .iter()
            .any(|c| !c.stale(now()) && c.models.iter().any(|m| &m.key == key)),
        "model is unavailable or its API catalog has expired; refresh the provider"
    );
    Ok(())
}
pub async fn serve_connection<R, W>(gateway: Gateway, mut input: R, mut output: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, mut receiver) =
        tokio::sync::mpsc::channel::<(Response<Reply>, StopAfterReply)>(32);
    let writer = tokio::spawn(async move {
        while let Some((reply, stop)) = receiver.recv().await {
            let delivered = transport::write_frame(&mut output, &reply).await;
            // Cleanup and current stop intent are already committed. A lost
            // acknowledgement must not leave a live gateway stuck draining.
            drop(stop);
            delivered?;
        }
        Ok::<(), anyhow::Error>(())
    });
    let semaphore = Arc::new(tokio::sync::Semaphore::new(16));
    let mut tasks = tokio::task::JoinSet::new();
    let stop = gateway.stop_signal();
    loop {
        let request = tokio::select! {
            _ = stop.cancelled() => break,
            request = transport::read_frame::<Request<Command>>(&mut input) => request?,
        };
        let Some(request) = request else { break };
        let permit = semaphore.clone().acquire_owned().await?;
        let gateway = gateway.clone();
        let sender = sender.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let (reply, stop) = gateway.request_with_effect(request).await;
            let stop = StopAfterReply(stop.then(|| gateway.stop_signal()));
            sender
                .send((reply, stop))
                .await
                .context("reply channel closed")
        });
        while let Some(result) = tasks.try_join_next() {
            result??;
        }
    }
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    drop(sender);
    writer.await??;
    Ok(())
}

fn validate_effort(
    snapshot: &Snapshot,
    model: Option<&ModelKey>,
    effort: Option<&str>,
) -> Result<()> {
    if let Some(effort) = effort {
        let supported = model.and_then(|key| {
            snapshot
                .catalogs
                .iter()
                .flat_map(|c| &c.models)
                .find(|m| &m.key == key)
        });
        ensure!(
            supported.is_some_and(|m| m.reasoning_efforts.iter().any(|v| v == effort)),
            "effort is not advertised by the selected model API"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn committed_shutdown_survives_a_lost_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Gateway::open(directory.path()).unwrap();
        let (mut client, server) = tokio::io::duplex(1024);
        let (read, write) = tokio::io::split(server);
        let worker = tokio::spawn(serve_connection(gateway.clone(), read, write));
        transport::write_frame(
            &mut client,
            &Request {
                id: Id::new_v4(),
                body: Command::Shutdown,
            },
        )
        .await
        .unwrap();
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(3), gateway.stop.cancelled())
            .await
            .unwrap();
        assert!(crate::service::is_stopped(directory.path()));
        assert!(worker.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn managed_joshua_connection_cannot_be_repointed_or_removed() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Gateway::open(directory.path()).unwrap();
        let provider = ProviderPreset::builtins()[0].configure("http://127.0.0.1:9876/v1".into());
        {
            let mut store = gateway.lock().unwrap();
            store.state.settings.providers.push(provider.clone());
            store.state.managed_joshua_provider = Some(provider.id);
            store.persist(None).unwrap();
        }
        let mut rename = gateway.snapshot().unwrap().settings;
        rename.providers[0].label = Id::new_v4().to_string();
        assert_eq!(
            gateway
                .request(Request {
                    id: Id::new_v4(),
                    body: Command::SaveSettings(rename)
                })
                .await
                .body,
            Reply::Ok
        );
        let before = gateway.snapshot().unwrap();
        for edit in ["endpoint", "protocol", "credentials", "remove"] {
            let mut settings = before.settings.clone();
            match edit {
                "endpoint" => settings.providers[0].base_url = "http://127.0.0.1:9877/v1".into(),
                "protocol" => settings.providers[0].api = ProviderApi::Anthropic,
                "credentials" => settings.providers[0].credential = Some(Id::new_v4().to_string()),
                "remove" => settings.providers.clear(),
                _ => unreachable!(),
            }
            let reply = gateway
                .request(Request {
                    id: Id::new_v4(),
                    body: Command::SaveSettings(settings),
                })
                .await
                .body;
            assert!(
                matches!(reply, Reply::Error(ref report) if report.message.contains("stop managed Joshua")),
                "{reply:?}"
            );
            assert_eq!(gateway.snapshot().unwrap(), before);
        }
    }
    #[tokio::test]
    async fn partial_downloads_are_cleaned_on_errors_cancellation_and_publication() {
        use axum::{Router, body::Body, routing::get};
        let payload = Id::new_v4().to_string();
        let sent = payload.clone();
        let waiting = payload.clone();
        let broken = payload.clone();
        let fail = Arc::new(tokio::sync::Notify::new());
        let release = fail.clone();
        let router = Router::new()
            .route(
                "/data",
                get(move || {
                    let data = sent.clone();
                    async move { data }
                }),
            )
            .route(
                "/waiting",
                get(move || {
                    let data = waiting.clone();
                    async move {
                        Body::from_stream(
                            futures_util::stream::once(
                                async move { Ok::<_, std::io::Error>(data) },
                            )
                            .chain(futures_util::stream::pending()),
                        )
                    }
                }),
            )
            .route(
                "/broken",
                get(move || {
                    let data = broken.clone();
                    let release = release.clone();
                    async move {
                        Body::from_stream(
                            futures_util::stream::once(
                                async move { Ok::<_, std::io::Error>(data) },
                            )
                            .chain(futures_util::stream::once(
                                async move {
                                    release.notified().await;
                                    Err::<String, _>(std::io::Error::other("interrupted transfer"))
                                },
                            )),
                        )
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let gateway = Gateway::open(root.path()).unwrap();
        for scenario in ["integrity", "network", "cancel", "collision", "complete"] {
            let directory = tempfile::tempdir().unwrap();
            let job = Download {
                id: Id::new_v4(),
                name: Id::new_v4().to_string(),
                source: address.to_string(),
                received: 0,
                total: None,
                state: DownloadState::Queued,
                error: None,
                sha256: (scenario == "integrity").then(|| "0".repeat(64)),
            };
            gateway.lock().unwrap().state.downloads.push(job.clone());
            if scenario == "collision" {
                std::fs::write(directory.path().join(&job.name), "existing").unwrap();
            }
            let path = match scenario {
                "network" => "broken",
                "cancel" => "waiting",
                _ => "data",
            };
            let response = reqwest::get(format!("http://{address}/{path}"))
                .await
                .unwrap();
            let worker = gateway.clone();
            let download = job.clone();
            let target = directory.path().to_owned();
            let task = tokio::spawn(async move {
                worker.transfer_response(&download, &target, response).await
            });
            if matches!(scenario, "network" | "cancel") {
                tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    loop {
                        if std::fs::read_dir(directory.path())
                            .unwrap()
                            .any(|entry| entry.unwrap().metadata().unwrap().len() > 0)
                        {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
            }
            if scenario == "cancel" {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                if scenario == "network" {
                    fail.notify_one();
                }
                let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    result.is_ok(),
                    scenario == "complete",
                    "{scenario}: {result:?}"
                );
            }
            let files: Vec<_> = std::fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            if matches!(scenario, "collision" | "complete") {
                assert_eq!(files, vec![std::ffi::OsString::from(&job.name)]);
                assert_eq!(
                    std::fs::read_to_string(directory.path().join(&job.name)).unwrap(),
                    if scenario == "collision" {
                        "existing"
                    } else {
                        &payload
                    }
                );
            } else {
                assert!(files.is_empty(), "{scenario}: {files:?}");
            }
        }
        server.abort();
    }
    #[tokio::test]
    async fn command_replay_creates_only_one_thread() {
        let dir = tempfile::tempdir().unwrap();
        let g = Gateway::open(dir.path()).unwrap();
        let id = Id::new_v4();
        let body = Command::CreateThread {
            title: "Work".into(),
            mode: Mode::Code,
        };
        let a = g
            .request(Request {
                id,
                body: body.clone(),
            })
            .await;
        let b = g.request(Request { id, body }).await;
        assert_eq!(a.body, b.body);
        assert_eq!(g.snapshot().unwrap().threads.len(), 1);
    }
    #[test]
    fn document_scope_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("project")).unwrap();
        std::fs::write(dir.path().join("outside"), "private").unwrap();
        assert!(scoped_path(&dir.path().join("project"), "../outside").is_err());
    }
}
