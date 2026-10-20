//! Official ACP client SDK, with no client filesystem or terminal capabilities.
use super::{BackendError, Progress, Result, check, isolation::Process, rpc};
use crate::provider::Encoded;
use a206_core::*;
use agent_client_protocol::{
    Client, Lines,
    schema::{ProtocolVersion, v1::*},
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
#[derive(Default)]
struct Output {
    session: Option<SessionId>,
    text: String,
    cost: Option<f64>,
}
pub async fn run(
    process: &mut Process,
    thread: &Thread,
    prompt: &str,
    input: &str,
    images: &[Encoded],
    progress: mpsc::Sender<Progress>,
) -> Result<(String, Usage)> {
    let (Some(writer), Some(reader)) = (process.child.stdin.take(), process.child.stdout.take())
    else {
        return Err(BackendError::Closed);
    };
    let reader = tokio::io::BufReader::new(reader);
    let incoming =
        futures_util::stream::try_unfold((reader, 0usize), async |(mut reader, mut count)| {
            let line = rpc::read(&mut reader).await?;
            let Some(line) = line else {
                return Ok(None);
            };
            count += line.len();
            if count > 32 * 1024 * 1024 {
                return Err(std::io::Error::other(
                    "Claude Agent exceeded its output limit",
                ));
            }
            Ok(Some((line, (reader, count))))
        });
    let outgoing = futures_util::sink::unfold(writer, async |mut writer, line: String| {
        rpc::write(&mut writer, line).await?;
        Ok::<_, std::io::Error>(writer)
    });
    let output = Arc::new(Mutex::new(Output::default()));
    let notifications = output.clone();
    let session_output = output.clone();
    let mut tools = Vec::new();
    if thread
        .grant
        .capabilities
        .contains(&Capability::ReadWorkspace)
    {
        tools.extend(["Read", "Glob", "Grep"]);
    }
    if thread
        .grant
        .capabilities
        .contains(&Capability::WriteWorkspace)
    {
        tools.extend(["Edit", "Write"]);
    }
    if thread.grant.capabilities.contains(&Capability::Execute) {
        tools.push("Bash");
    }
    let mut options = json!({"model": thread.model.as_ref().ok_or(BackendError::NoModel)?.model, "systemPrompt": prompt,
        "settingSources": [], "plugins": [], "strictMcpConfig": true, "mcpServers": {}, "persistSession": false,
        "maxTurns": 32, "tools": tools, "allowedTools": tools, "disallowedTools": ["Agent", "Task", "WebFetch", "WebSearch"],
        "extraArgs": {"bare": null} });
    if let Some(effort) = &thread.effort {
        options["effort"] = json!(effort);
    }
    let mut meta = serde_json::Map::new();
    meta.insert("claudeCode".into(), json!({"options": options}));
    let cwd = process.cwd.clone();
    let input = input.to_owned();
    let images = images.to_vec();
    Client
        .builder()
        .name("Agent 206")
        .on_receive_request(
            async move |_request: RequestPermissionRequest, responder, _connection| {
                // Native tools explicitly granted above remain confined by the OS sandbox.
                // Any request to expand those grants is denied, never silently approved.
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                let mut output = notifications.lock().map_err(|_| Error::internal_error())?;
                if output.session.as_ref() != Some(&notification.session_id) {
                    return Ok(());
                }
                match notification.update {
                    SessionUpdate::AgentMessageChunk(chunk) => {
                        if let ContentBlock::Text(text) = chunk.content {
                            if output.text.len() + text.text.len() > 128 * 1024 {
                                return Err(Error::internal_error()
                                    .data("Backend message exceeds the conversation limit"));
                            }
                            output.text.push_str(&text.text);
                            // Progress is advisory: a full or closed channel only drops an update.
                            progress
                                .try_send(Progress::Text(output.text.clone()))
                                .ignore();
                        }
                    }
                    SessionUpdate::ToolCall(tool) => {
                        // Progress is advisory: a full or closed channel only drops an update.
                        progress
                            .try_send(Progress::Activity(format!("Claude Agent: {:?}", tool.kind)))
                            .ignore();
                    }
                    SessionUpdate::UsageUpdate(usage) => {
                        // ACP context-used is not request input usage. Preserve that distinction.
                        output.cost = usage
                            .cost
                            .filter(|cost| cost.currency == "USD")
                            .map(|cost| cost.amount)
                            .filter(|cost| cost.is_finite() && *cost >= 0.0);
                    }
                    _ => {}
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            Lines::new(Box::pin(outgoing), Box::pin(incoming)),
            async move |connection| {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                if initialized.protocol_version != ProtocolVersion::V1 {
                    return Err(Error::internal_error().data("Unsupported ACP protocol"));
                }
                let session = connection
                    .send_request(NewSessionRequest::new(cwd).meta(meta))
                    .block_task()
                    .await?;
                session_output
                    .lock()
                    .map_err(|_| Error::internal_error())?
                    .session = Some(session.session_id.clone());
                let accepts = initialized.agent_capabilities.prompt_capabilities.image;
                let response = connection
                    .send_request(PromptRequest::new(
                        session.session_id,
                        prompt_blocks(input, &images, accepts),
                    ))
                    .block_task()
                    .await?;
                if response.stop_reason != StopReason::EndTurn
                    && response.stop_reason != StopReason::MaxTokens
                {
                    return Err(
                        Error::internal_error().data("Claude Agent did not complete the turn")
                    );
                }
                Ok(())
            },
        )
        .await
        .map_err(BackendError::Acp)?;
    let output = output.lock().map_err(|_| BackendError::Poisoned)?;
    check(
        !output.text.is_empty(),
        BackendError::Empty {
            backend: "Claude Agent",
        },
    )?;
    Ok((
        output.text.clone(),
        Usage {
            input_tokens: None,
            output_tokens: None,
            cost: output.cost,
        },
    ))
}
/// The blocks of an ACP prompt: the transcript, then each image when the
/// agent advertises image prompts; otherwise the transcript names them as
/// not sent.
fn prompt_blocks(mut text: String, images: &[Encoded], accepts: bool) -> Vec<ContentBlock> {
    if !accepts {
        for image in images {
            text.push_str(&format!(
                "\n[Image {:?} was attached but is not sent: the Claude Agent adapter does not accept images.]",
                image.name
            ));
        }
        return vec![text.into()];
    }
    let mut blocks = vec![text.into()];
    blocks.extend(
        images
            .iter()
            .map(|image| ContentBlock::Image(ImageContent::new(image.base64.clone(), image.mime))),
    );
    blocks
}
#[cfg(test)]
mod tests {
    use super::*;
    fn images() -> Vec<Encoded> {
        ["image/jpeg", "image/gif"]
            .into_iter()
            .map(|mime| Encoded {
                name: uuid::Uuid::new_v4().to_string(),
                mime,
                base64: uuid::Uuid::new_v4().simple().to_string(),
            })
            .collect()
    }
    #[test]
    fn images_follow_the_transcript_when_the_agent_accepts_them() {
        let text = uuid::Uuid::new_v4().to_string();
        let images = images();
        let blocks = prompt_blocks(text.clone(), &images, true);
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], ContentBlock::Text(block) if block.text == text));
        for (block, image) in blocks[1..].iter().zip(&images) {
            let ContentBlock::Image(block) = block else {
                panic!("expected an image block");
            };
            assert_eq!(block.data, image.base64);
            assert_eq!(block.mime_type, image.mime);
        }
    }
    #[test]
    fn an_agent_without_image_prompts_learns_the_images_were_not_sent() {
        let text = uuid::Uuid::new_v4().to_string();
        let images = images();
        let blocks = prompt_blocks(text.clone(), &images, false);
        let [ContentBlock::Text(block)] = blocks.as_slice() else {
            panic!("expected one text block");
        };
        assert!(block.text.starts_with(&text));
        for image in &images {
            assert!(block.text.contains(&format!("{:?}", image.name)));
            assert!(!block.text.contains(&image.base64));
        }
    }
}
