//! ACP v1 adapter using the official SDK; no filesystem, terminal or permission bypass.
use crate::client::Client;
use a206_core::{Command, Reply, Role, RunState};
use agent_client_protocol::schema::{ProtocolVersion, v1::*};
use agent_client_protocol::{Agent, Stdio};
use uuid::Uuid;

/// The ACP connection failed or closed with an error.
#[derive(Debug, thiserror::Error)]
#[error("ACP connection failed")]
pub struct AcpError(#[source] agent_client_protocol::Error);
pub async fn serve(client: Client, thread: Uuid) -> Result<(), AcpError> {
    // The launch command binds authority to a user-selected existing thread.
    let session = SessionId::new(thread.to_string());
    let for_new = session.clone();
    let for_prompt = session.clone();
    let for_cancel = session.clone();
    let cancel_client = client.clone();
    Agent
        .builder()
        .name("Agent 206")
        .on_receive_request(
            async move |_request: InitializeRequest, responder, _connection| {
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _connection| {
                if !request.mcp_servers.is_empty() {
                    return responder.respond_with_error(
                        Error::invalid_params()
                            .data("MCP server installation must be approved in the application"),
                    );
                }
                responder.respond(NewSessionResponse::new(for_new.clone()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                if request.session_id != for_prompt {
                    return responder.respond_with_error(Error::invalid_params());
                }
                if request
                    .prompt
                    .iter()
                    .any(|block| !matches!(block, ContentBlock::Text(_)))
                {
                    return responder.respond_with_error(
                        Error::invalid_params().data("Only text prompts are supported"),
                    );
                }
                let text = request
                    .prompt
                    .iter()
                    .filter_map(|block| {
                        if let ContentBlock::Text(text) = block {
                            Some(text.text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let response = client
                    .call(Command::Send {
                        thread,
                        text,
                        attachments: vec![],
                        images: vec![],
                    })
                    .await
                    .map_err(|_| Error::internal_error())?;
                match response {
                    Reply::Thread(thread) if thread.state == RunState::Complete => {
                        if let Some(message) = thread
                            .messages
                            .iter()
                            .rev()
                            .find(|m| m.role == Role::Assistant)
                        {
                            connection.send_notification(SessionNotification::new(
                                for_prompt.clone(),
                                SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                    message.text.clone().into(),
                                )),
                            ))?;
                        }
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    }
                    Reply::Thread(thread) if thread.state == RunState::Cancelled => {
                        responder.respond(PromptResponse::new(StopReason::Cancelled))
                    }
                    _ => {
                        responder.respond_with_error(Error::internal_error().data(
                            "Gateway could not complete this prompt; inspect the conversation",
                        ))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |request: CancelNotification, _connection| {
                if request.session_id == for_cancel {
                    cancel_client
                        .call(Command::CancelRun { thread })
                        .await
                        .map_err(|_| Error::internal_error())?;
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
        .map_err(AcpError)?;
    Ok(())
}
