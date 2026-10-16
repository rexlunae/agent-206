//! External agent runtimes never inherit ambient configuration or credentials.
mod claude;
mod codex;
mod isolation;
mod proxy;
mod rpc;
use a206_core::*;
use anyhow::{Context, Result, ensure};
use std::path::Path;
use tokio::sync::mpsc;
pub enum Progress {
    Text(String),
    Activity(String),
}
pub fn validate_config(config: &BackendConfig) -> Result<()> {
    ensure!(
        config.kind != CodingBackend::Native,
        "Agent 206 does not need an external runtime"
    );
    for path in [&config.executable, &config.runtime_root]
        .into_iter()
        .chain(config.adapter_script.iter())
    {
        ensure!(
            Path::new(path).is_absolute() && !path.chars().any(char::is_control),
            "backend paths must be absolute and contain no control characters"
        );
    }
    ensure!(
        (config.kind == CodingBackend::ClaudeAgent) == config.adapter_script.is_some(),
        "Claude Agent needs a JavaScript ACP adapter entry point; Codex does not"
    );
    Ok(())
}
pub fn validate_selection(
    config: &ProviderConfig,
    thread: &Thread,
    backend: &BackendConfig,
) -> Result<()> {
    validate_config(backend)?;
    ensure!(
        backend.kind == thread.backend,
        "wrong coding backend configuration"
    );
    ensure!(
        config.credential.is_none(),
        "coding backends require the isolated credential broker for authenticated providers; it is not connected yet"
    );
    ensure!(
        match thread.backend {
            CodingBackend::Codex => config.api == ProviderApi::OpenAi,
            CodingBackend::ClaudeAgent => config.api == ProviderApi::Anthropic,
            CodingBackend::Native => false,
        },
        "selected provider protocol is incompatible with this coding backend"
    );
    ensure!(
        thread.parent.is_none(),
        "external backend delegation is unavailable until per-child backend policy is implemented"
    );
    Ok(())
}
pub struct Run<'a> {
    pub config: &'a ProviderConfig,
    pub thread: &'a Thread,
    pub prompt: &'a str,
    pub info: &'a ModelInfo,
    pub backend: &'a BackendConfig,
    pub workspace: Option<&'a Path>,
    pub state_root: &'a Path,
    pub progress: mpsc::Sender<Progress>,
    pub cancel: tokio_util::sync::CancellationToken,
}
pub async fn complete(run: Run<'_>) -> Result<(String, Usage)> {
    let Run {
        config,
        thread,
        prompt,
        info,
        backend,
        workspace,
        state_root,
        progress,
        cancel,
    } = run;
    validate_selection(config, thread, backend)?;
    ensure!(
        thread.model.as_ref() == Some(&info.key) && info.key.provider == config.id,
        "backend model metadata does not match its provider selection"
    );
    let mut input = transcript(thread)?;
    let proxy = proxy::Proxy::start(config.clone(), info.clone()).await?;
    let mut process = isolation::Process::launch(
        backend,
        thread,
        workspace,
        state_root,
        proxy.port(),
        proxy.token(),
    )?;
    if let Some(workspace) = &process.workspace {
        input.insert_str(
            0,
            &format!(
                "Assigned workspace: {}\nUse this directory for project work.\n",
                workspace.display()
            ),
        );
    }
    let result = {
        let operation = async {
            match thread.backend {
                CodingBackend::Codex => {
                    codex::run(&mut process, thread, prompt, &input, progress).await
                }
                CodingBackend::ClaudeAgent => {
                    claude::run(&mut process, thread, prompt, &input, progress).await
                }
                CodingBackend::Native => anyhow::bail!("native agent is not an external backend"),
            }
        };
        tokio::select! {
            result = tokio::time::timeout(std::time::Duration::from_secs(600), operation) => result.context("coding backend exceeded its ten-minute run limit").and_then(|result| result),
            _ = cancel.cancelled() => Err(anyhow::anyhow!("coding backend cancelled")),
        }
    };
    drop(proxy);
    process.stop().await;
    result
}
fn transcript(thread: &Thread) -> Result<String> {
    let mut text = String::new();
    for message in &thread.messages {
        if matches!(message.role, Role::User | Role::Assistant) {
            text.push_str(if message.role == Role::User {
                "\nUSER:\n"
            } else {
                "\nASSISTANT:\n"
            });
            text.push_str(&crate::provider::message_content(message));
            ensure!(
                text.len() <= 2 * 1024 * 1024,
                "backend transcript exceeds its input limit"
            );
        }
    }
    Ok(text)
}
