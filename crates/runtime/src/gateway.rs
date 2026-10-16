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
#[derive(Clone)]
pub struct Gateway {
    stop: tokio_util::sync::CancellationToken,
    joshua: Arc<tokio::sync::Mutex<crate::joshua::Manager>>,
    store: Arc<Mutex<Store>>,
    downloads: Arc<Mutex<BTreeMap<Id, tokio_util::sync::CancellationToken>>>,
    running: Arc<Mutex<BTreeMap<Id, tokio_util::sync::CancellationToken>>>,
}
impl Gateway {
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self {
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
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))
    }
    pub fn stop_signal(&self) -> tokio_util::sync::CancellationToken {
        self.stop.clone()
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        Ok(self.lock()?.state.clone())
    }
    pub async fn request(&self, request: Request<Command>) -> Response<Reply> {
        let id = request.id;
        let mutation = !matches!(
            request.body,
            Command::Snapshot
                | Command::JoshuaStatus
                | Command::Events { .. }
                | Command::GitStatus { .. }
                | Command::ReadDocument { .. }
                | Command::RunExtension { .. }
        );
        let begin = (|| -> Result<Option<Reply>> {
            if mutation {
                self.lock()?.begin(id, &request.body)
            } else {
                Ok(None)
            }
        })();
        let body = match begin {
            Ok(Some(reply)) => reply,
            Err(error) => Reply::Error {
                message: error.to_string(),
            },
            Ok(None) => {
                let reply = self
                    .handle(request.body)
                    .await
                    .unwrap_or_else(|e| Reply::Error {
                        message: e.to_string(),
                    });
                if mutation && let Err(e) = self.lock().and_then(|s| s.finish(id, &reply)) {
                    return Response {
                        id,
                        body: Reply::Error {
                            message: format!(
                                "operation completed but its receipt could not be persisted: {e}"
                            ),
                        },
                    };
                }
                reply
            }
        };
        Response { id, body }
    }
    async fn handle(&self, command: Command) -> Result<Reply> {
        match command {
            Command::Snapshot => Ok(Reply::Snapshot(Box::new(self.snapshot()?))),
            Command::JoshuaStatus => Ok(Reply::Text(self.joshua.lock().await.status()?)),
            Command::StartJoshua {
                label,
                model_file,
                port,
                remote,
            } => {
                let root = self.lock()?.root.clone();
                let provider = self
                    .joshua
                    .lock()
                    .await
                    .start(label, model_file, port, remote, &root)
                    .await?;
                let id = provider.id;
                {
                    let mut store = self.lock()?;
                    store.state.settings.providers.push(provider);
                    store.state.settings.revision += 1;
                    store.persist(Some(EventKind::SettingsChanged))?;
                }
                Ok(Reply::Text(format!(
                    "Joshua started. Refresh the new connection to discover its models. Provider: {id}"
                )))
            }
            Command::StopJoshua => {
                self.joshua.lock().await.stop().await?;
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
                // Revocation also stops already-running inference and coding backends.
                let running = self
                    .running
                    .lock()
                    .map_err(|_| anyhow::anyhow!("run lock poisoned"))?;
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
                        .map_err(|_| anyhow::anyhow!("run lock poisoned"))?;
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
                    let copy = thread.clone();
                    self.running
                        .lock()
                        .map_err(|_| anyhow::anyhow!("run lock poisoned"))?
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
                    .map_err(|_| anyhow::anyhow!("run lock poisoned"))?
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
                        thread.state = RunState::Failed;
                        thread.messages.push(Message {
                            id: Id::new_v4(),
                            role: Role::System,
                            attachments: vec![],
                            text: e.to_string(),
                            at: now(),
                            backend: thread.backend,
                        });
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
                    .map_err(|_| anyhow::anyhow!("run lock poisoned"))?;
                let token = running.get(&thread).context("thread is not running")?;
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
                    .context("download not found")?;
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
                    .map_err(|_| anyhow::anyhow!("transfer lock poisoned"))?
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
            Command::Shutdown => Ok(Reply::Ok),
        }
    }
    fn project_root(&self, id: Id) -> Result<PathBuf> {
        Ok(self
            .lock()?
            .state
            .projects
            .iter()
            .find(|p| p.id == id)
            .context("project not found")?
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
            .map_err(|_| anyhow::anyhow!("transfer lock poisoned"))?
            .insert(download.id, token.clone());
        let gateway = self.clone();
        let job = download.clone();
        tokio::spawn(async move {
            let result = tokio::select! { result = gateway.transfer(&job, &target, &url) => result, _ = token.cancelled() => Err(anyhow::anyhow!("download cancelled")) };
            if let Err(error) = result {
                let update = gateway.update_download(job.id, |d| {
                    if d.state != DownloadState::Cancelled {
                        d.state = DownloadState::Failed;
                        d.error = Some(error.to_string());
                    }
                });
                if update.is_err() {
                    eprintln!("could not persist failed download state");
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
            .context("download disappeared")?;
        update(download);
        let copy = download.clone();
        store.persist(Some(EventKind::DownloadChanged(copy.clone())))?;
        Ok(copy)
    }
    async fn transfer(&self, download: &Download, target: &Path, source: &str) -> Result<()> {
        let response = crate::network::public_get(source).await?;
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
        let partial = target.join(format!("{}.{}.part", download.name, download.id));
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .await?;
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
            .context("download disappeared")?;
        ensure!(
            current.state != DownloadState::Cancelled,
            "download cancelled"
        );
        std::fs::hard_link(&partial, target.join(&download.name))?;
        std::fs::remove_file(partial)?;
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
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<(Response<Reply>, bool)>(32);
    let stop = gateway.stop_signal();
    let writer = tokio::spawn(async move {
        while let Some((reply, shutdown)) = receiver.recv().await {
            transport::write_frame(&mut output, &reply).await?;
            if shutdown {
                stop.cancel();
            }
        }
        Ok::<(), anyhow::Error>(())
    });
    let semaphore = Arc::new(tokio::sync::Semaphore::new(16));
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(request) = transport::read_frame::<Request<Command>>(&mut input).await? {
        let shutdown = matches!(request.body, Command::Shutdown);
        let permit = semaphore.clone().acquire_owned().await?;
        let gateway = gateway.clone();
        let sender = sender.clone();
        tasks.spawn(async move {
            let _permit = permit;
            sender
                .send((gateway.request(request).await, shutdown))
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
