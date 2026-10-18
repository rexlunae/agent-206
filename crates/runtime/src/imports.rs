//! Read foreign installations in a short-lived, read-only OS sandbox.
//! Only the reviewed preview crosses back; credential migration still belongs
//! to the vault broker. This is not an agent-callable operation.
use crate::transport::{FrameError, read_frame, write_frame};
use a206_core::{ErrorKind, ErrorReport, Settings};
use a206_import::{ImportError, Preview};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const READER_COMMAND: &str = "__a206-import-reader";
const VERSION: u32 = 3;
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Serialize, Deserialize)]
struct ReadRequest {
    version: u32,
    tool: a206_import::Tool,
    source: PathBuf,
    settings: Settings,
    vault_exists: Option<bool>,
    exposure: Exposure,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum Exposure {
    Terminal,
    SettingsReview,
}

/// Runs inside the reader, before any ordinary reply. Shared reviews omit
/// optional free-form source metadata rather than guessing whether it is a
/// password. Names used to save accounts are generated or owned by target presets.
fn protect(
    mut preview: Preview,
    exposure: Exposure,
    settings: &Settings,
) -> Result<Preview, ReaderError> {
    if matches!(exposure, Exposure::SettingsReview) {
        let presets = settings.provider_presets.all();
        for (index, provider) in preview.providers.iter_mut().enumerate() {
            provider.label = provider
                .preset
                .as_ref()
                .filter(|id| id.as_str() != "custom")
                .and_then(|id| presets.iter().find(|preset| &preset.id == id))
                .map(|preset| preset.name.clone())
                .unwrap_or_else(|| format!("Imported provider {}", index + 1));
            provider.source_id = format!("provider-{}", index + 1);
            provider.model_reference = None;
            provider.credential = None;
        }
        for messenger in &mut preview.messengers {
            messenger.source_name = String::new();
            messenger.source_kind = messenger
                .kind
                .map(|kind| kind.label().to_owned())
                .unwrap_or_else(|| "Unsupported service".into());
        }
    }
    // MessagePack string bytes remain contiguous. Scan before crossing the
    // reader boundary; refuse the whole preview rather than corrupting a
    // destination, login or restriction by replacing text inside it.
    let bytes = rmp_serde::to_vec_named(&preview).map_err(ReaderError::PreviewEncoding)?;
    if a206_leak_scan::redact(&String::from_utf8_lossy(&bytes)).is_some() {
        return Err(ReaderError::SensitiveSource);
    }
    Ok(preview)
}

/// Only duplicate matching and preset defaults are needed by the reader.
/// Never forward prompts, backend/MCP commands, grants or credential handles.
fn target_settings(settings: &Settings) -> Settings {
    Settings {
        providers: settings
            .providers
            .iter()
            .map(|provider| a206_core::ProviderConfig {
                id: provider.id,
                api: provider.api,
                base_url: provider.base_url.clone(),
                label: String::new(),
                credential: None,
            })
            .collect(),
        messengers: settings
            .messengers
            .iter()
            .map(|messenger| {
                a206_core::MessengerConfig::new(messenger.name.clone(), messenger.kind)
            })
            .collect(),
        provider_presets: settings.provider_presets.clone(),
        ..Settings::default()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReaderError {
    #[error(
        "Source metadata contains credential-shaped text. Review the source locally; no preview was returned."
    )]
    SensitiveSource,
    #[error("could not encode the import preview for inspection")]
    PreviewEncoding(#[source] rmp_serde::encode::Error),
    #[error("could not inspect import source {}", path.display())]
    Inspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("import source must be a directory, not a symbolic link")]
    Source,
    #[error("could not confine the import reader")]
    Confine(#[from] crate::sandbox::ConfineError),
    #[error("import reader channel failed")]
    Channel(#[from] FrameError),
    #[error("import reader returned no preview")]
    Empty,
    #[error("import reader returned an unrelated reply or extra output")]
    Reply,
    #[error("import reader protocol version {found} is unsupported")]
    Version { found: u32 },
    #[error("import reader refused the source")]
    Failed(#[source] ErrorReport),
    #[error("could not wait for import reader")]
    Wait(#[source] std::io::Error),
    #[error("import reader exited with {status}")]
    Exited { status: std::process::ExitStatus },
    #[error("import preview exceeded its deadline")]
    Timeout(#[from] tokio::time::error::Elapsed),
    #[error("could not initialize import reader")]
    Runtime(#[source] std::io::Error),
    #[error("isolated imports are supported only on Linux and macOS")]
    Unsupported,
}
impl crate::error::Classified for ReaderError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Inspect { .. } | Self::Confine(_) | Self::Failed(_) => return None,
            Self::Source | Self::SensitiveSource => ErrorKind::InvalidRequest,
            Self::PreviewEncoding(_) => ErrorKind::Protocol,
            Self::Channel(_) | Self::Empty | Self::Reply | Self::Version { .. } => {
                ErrorKind::Protocol
            }
            Self::Wait(_) | Self::Runtime(_) => ErrorKind::Internal,
            Self::Exited { .. } | Self::Unsupported => ErrorKind::Unavailable,
            Self::Timeout(_) => ErrorKind::Timeout,
        })
    }
}
impl crate::error::Classified for ImportError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::NotDirectory { .. }
            | Self::NotRegular { .. }
            | Self::TooLarge { .. }
            | Self::Config { .. }
            | Self::JsonConfig { .. } => ErrorKind::InvalidRequest,
            Self::Read { .. } | Self::Receipt { .. } => return None,
            Self::EncodeReceipt(_) => ErrorKind::Storage,
        })
    }
}

/// Preview a local RustyClaw source. Cancelling this future kills the reader;
/// no source/target settings or vault contents are changed.
pub async fn preview(
    source: &Path,
    settings: &Settings,
    vault_exists: Option<bool>,
) -> Result<Preview, ReaderError> {
    preview_with(
        a206_import::Tool::RustyClaw,
        source,
        settings,
        vault_exists,
        Exposure::Terminal,
    )
    .await
}

/// Settings-only review: no source labels, model text or credential references.
pub async fn preview_settings(source: &Path, settings: &Settings) -> Result<Preview, ReaderError> {
    preview_settings_for(a206_import::Tool::RustyClaw, source, settings).await
}

pub async fn preview_settings_for(
    tool: a206_import::Tool,
    source: &Path,
    settings: &Settings,
) -> Result<Preview, ReaderError> {
    preview_with(tool, source, settings, None, Exposure::SettingsReview).await
}

async fn preview_with(
    tool: a206_import::Tool,
    source: &Path,
    settings: &Settings,
    vault_exists: Option<bool>,
    exposure: Exposure,
) -> Result<Preview, ReaderError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let inspect = |source_error| ReaderError::Inspect {
            path: source.to_owned(),
            source: source_error,
        };
        let metadata = std::fs::symlink_metadata(source).map_err(inspect)?;
        if !metadata.is_dir() {
            return Err(ReaderError::Source);
        }
        let source = std::fs::canonicalize(source).map_err(inspect)?;
        let request = a206_protocol::Request {
            id: uuid::Uuid::new_v4(),
            body: ReadRequest {
                version: VERSION,
                tool,
                source: source.clone(),
                settings: target_settings(settings),
                vault_exists,
                exposure,
            },
        };
        tokio::time::timeout(DEADLINE, async {
            let mut worker = crate::sandbox::Confined::spawn_self_reading(Some(&source), |_| {
                vec![READER_COMMAND.into()]
            })
            .await?;
            let (Some(mut input), Some(mut output)) =
                (worker.child.stdin.take(), worker.child.stdout.take())
            else {
                return Err(ReaderError::Empty);
            };
            let exchange = async {
                write_frame(&mut input, &request).await?;
                drop(input);
                let response: a206_protocol::Response<Result<Preview, ErrorReport>> =
                    read_frame(&mut output).await?.ok_or(ReaderError::Empty)?;
                if response.id != request.id {
                    return Err(ReaderError::Reply);
                }
                // A single result and EOF, never an unbounded stream.
                use tokio::io::AsyncReadExt;
                if output.read(&mut [0u8; 1]).await.map_err(FrameError::Io)? != 0 {
                    return Err(ReaderError::Reply);
                }
                response.body.map_err(ReaderError::Failed)
            }
            .await;
            if exchange.is_err()
                && let Some(failure) = worker.startup_failure().await
            {
                return Err(crate::sandbox::ConfineError::Start(failure).into());
            }
            let status = worker.child.wait().await.map_err(ReaderError::Wait)?;
            worker.stop().await;
            let result = exchange?;
            if !status.success() {
                return Err(ReaderError::Exited { status });
            }
            Ok(result)
        })
        .await?
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (tool, source, settings, vault_exists, exposure);
        Err(ReaderError::Unsupported)
    }
}

async fn serve() -> Result<(), ReaderError> {
    serve_io(&mut tokio::io::stdin(), &mut tokio::io::stdout()).await
}

async fn serve_io(
    input: &mut (impl tokio::io::AsyncRead + Unpin),
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), ReaderError> {
    let request: a206_protocol::Request<ReadRequest> =
        read_frame(input).await?.ok_or(ReaderError::Empty)?;
    let result = if request.body.version != VERSION {
        Err(crate::error::report(&ReaderError::Version {
            found: request.body.version,
        })
        .with_correlation(request.id))
    } else {
        match request.body.tool {
            a206_import::Tool::RustyClaw => a206_import::rustyclaw::preview(
                &request.body.source,
                &request.body.settings,
                request.body.vault_exists,
            ),
            a206_import::Tool::ClaudeCode => {
                a206_import::claude_code::preview(&request.body.source, &request.body.settings)
            }
            a206_import::Tool::Codex => {
                a206_import::codex::preview(&request.body.source, &request.body.settings)
            }
        }
        .map_err(|error| crate::error::correlated("import.preview", request.id, &error))
        .and_then(|preview| {
            protect(preview, request.body.exposure, &request.body.settings).map_err(|error| {
                crate::error::correlated("import.preview.protect", request.id, &error)
            })
        })
    };
    send_result(output, request.id, result).await?;
    Ok(())
}

async fn send_result(
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
    id: uuid::Uuid,
    body: Result<Preview, ErrorReport>,
) -> Result<(), FrameError> {
    match write_frame(output, &a206_protocol::Response { id, body }).await {
        // Encoding fails before any bytes are written. Send a bounded failure
        // instead of closing the pipe and hiding the actual cause from the UI.
        Err(FrameError::Protocol(error)) => {
            let body: Result<Preview, ErrorReport> = Err(crate::error::correlated(
                "import.preview.encode",
                id,
                &error,
            ));
            write_frame(output, &a206_protocol::Response { id, body }).await
        }
        result => result,
    }
}

/// Called after the OS sandbox helper dispatch, before ordinary app startup.
pub fn dispatch() -> Option<Result<std::process::ExitCode, ReaderError>> {
    if std::env::args_os().nth(1)? != READER_COMMAND {
        return None;
    }
    Some((|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(ReaderError::Runtime)?
            .block_on(serve())?;
        Ok(std::process::ExitCode::SUCCESS)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn unknown_versions_are_correlated_protocol_failures_before_reading() {
        let request = a206_protocol::Request {
            id: uuid::Uuid::new_v4(),
            body: ReadRequest {
                version: VERSION + 1,
                tool: a206_import::Tool::RustyClaw,
                source: PathBuf::from("/not-an-installation"),
                settings: Settings::default(),
                vault_exists: None,
                exposure: Exposure::SettingsReview,
            },
        };
        let bytes = a206_protocol::encode(&request).unwrap();
        let mut output = Vec::new();
        serve_io(&mut bytes.as_slice(), &mut output).await.unwrap();
        let response: a206_protocol::Response<Result<Preview, ErrorReport>> =
            read_frame(&mut output.as_slice()).await.unwrap().unwrap();
        assert_eq!(response.id, request.id);
        assert_eq!(response.body.unwrap_err().kind, ErrorKind::Protocol);
    }
    #[tokio::test]
    async fn malformed_and_oversized_frames_do_not_produce_previews() {
        let mut output = Vec::new();
        assert!(
            serve_io(&mut b"A206".as_slice(), &mut output)
                .await
                .is_err()
        );
        assert!(output.is_empty());
        let mut header = b"A206\0\x01".to_vec();
        header.extend_from_slice(&((a206_protocol::MAX_FRAME + 1) as u32).to_be_bytes());
        assert!(serve_io(&mut header.as_slice(), &mut output).await.is_err());
        assert!(output.is_empty());
    }
    #[tokio::test]
    async fn an_oversized_preview_returns_its_failure_instead_of_closing() {
        let id = uuid::Uuid::new_v4();
        let preview = Preview {
            version: a206_import::PREVIEW_VERSION,
            tool: a206_import::Tool::RustyClaw,
            source: PathBuf::from("x".repeat(a206_protocol::MAX_FRAME + 1)),
            config_sha256: None,
            vault: None,
            providers: vec![],
            messengers: vec![],
            inactive: vec![],
            unsupported: vec![],
        };
        let mut output = Vec::new();
        send_result(&mut output, id, Ok(preview)).await.unwrap();
        let response: a206_protocol::Response<Result<Preview, ErrorReport>> =
            read_frame(&mut output.as_slice()).await.unwrap().unwrap();
        let failure = response.body.unwrap_err();
        assert_eq!(failure.kind, ErrorKind::Protocol);
        assert_eq!(failure.correlation, Some(id));
        assert!(failure.message.contains("frame exceeds"));
    }
    #[test]
    fn target_snapshot_does_not_forward_private_configuration() {
        let private = uuid::Uuid::new_v4().to_string();
        let mut settings = Settings::default();
        settings.providers.push(a206_core::ProviderConfig {
            id: uuid::Uuid::new_v4(),
            label: private.clone(),
            base_url: "https://example.test".into(),
            credential: Some(private.clone()),
            api: Default::default(),
        });
        settings.prompts[0].prompt = private.clone();
        let mut messenger =
            a206_core::MessengerConfig::new("inbox".into(), a206_core::MessengerKind::Telegram);
        messenger.credential = Some(private.clone());
        messenger.allowed_chats.insert(private.clone());
        settings.messengers.push(messenger);
        let target = target_settings(&settings);
        assert!(!toml::to_string(&target).unwrap().contains(&private));
        assert_eq!(target.providers[0].id, settings.providers[0].id);
        assert_eq!(target.messengers[0].name, "inbox");
    }
}
