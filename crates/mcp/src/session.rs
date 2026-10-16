//! One initialized MCP client session and the server process behind it.
use crate::{Launch, Limits, McpError};
use a206_core::McpToolInfo;
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, ContentBlock, PaginatedRequestParams, ResourceContents},
    service::RunningService,
    transport::IntoTransport,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Pages of tool listings followed before a server is considered abusive.
const MAX_LISTING_PAGES: usize = 64;

/// A connected server. Dropping it ends the session and stops its process.
pub struct Session {
    service: RunningService<RoleClient, ()>,
    /// Owns the server's process group.
    process: Option<Launch>,
    server: Option<String>,
    tools: Vec<McpToolInfo>,
    limits: Limits,
}

/// What a tool returned. Every field is untrusted server output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// The server reported the call as failed; `content` describes why.
    pub is_error: bool,
    pub content: Vec<Content>,
    /// Structured JSON result, when the tool declares one.
    pub structured: Option<serde_json::Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Content {
    Text(String),
    Image {
        mime_type: String,
        /// Base64, as the server sent it.
        data: String,
    },
    Audio {
        mime_type: String,
        data: String,
    },
    Resource {
        uri: String,
        mime_type: Option<String>,
        /// Absent for binary resources.
        text: Option<String>,
    },
    Link {
        uri: String,
        name: String,
        description: Option<String>,
    },
    /// A content type this client does not understand yet.
    Unsupported,
}

impl Session {
    /// Initialize a session over `transport` and list the server's tools.
    /// `process` is the server process behind the transport, if any.
    pub async fn start<T, E, A>(
        transport: T,
        process: Option<Launch>,
        limits: Limits,
    ) -> Result<Self, McpError>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let operation = async {
            let service =
                ().serve(transport)
                    .await
                    .map_err(|error| McpError::Initialize(Box::new(error)))?;
            let tools = list_tools(&service, &limits).await?;
            Ok((service, tools))
        };
        let (service, tools) = tokio::time::timeout(limits.connect_timeout, operation)
            .await
            .map_err(McpError::Timeout)??;
        let server = service
            .peer_info()
            .and_then(|info| info.server_info.clone())
            .map(|server| format!("{} {}", server.name, server.version));
        Ok(Self {
            service,
            process,
            server,
            tools,
            limits,
        })
    }
    /// Name and version the server reported; untrusted display text.
    pub fn server(&self) -> Option<&str> {
        self.server.as_deref()
    }
    pub fn tools(&self) -> &[McpToolInfo] {
        &self.tools
    }
    /// Call a tool the server advertised. Authorization happens before this,
    /// in [`crate::authorize`]; this only bounds time and output.
    pub async fn call(
        &self,
        tool: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
        timeout: Option<Duration>,
    ) -> Result<ToolOutput, McpError> {
        if !self.tools.iter().any(|t| t.name == tool) {
            return Err(McpError::UnknownTool);
        }
        let params = CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments);
        let result = tokio::time::timeout(
            timeout.unwrap_or(self.limits.call_timeout),
            self.service.peer().call_tool(params),
        )
        .await
        .map_err(McpError::Timeout)?
        .map_err(McpError::Service)?;
        let size = serde_json::to_vec(&result.content)
            .map_err(McpError::Schema)?
            .len()
            + result
                .structured_content
                .as_ref()
                .map(|v| serde_json::to_vec(v).map(|b| b.len()))
                .transpose()
                .map_err(McpError::Schema)?
                .unwrap_or(0);
        if size > self.limits.max_output_bytes {
            return Err(McpError::OutputTooLarge {
                limit: self.limits.max_output_bytes,
            });
        }
        Ok(ToolOutput {
            is_error: result.is_error.unwrap_or(false),
            content: result.content.into_iter().map(content).collect(),
            structured: result.structured_content,
        })
    }
    /// End the session now, stopping the server and failing in-flight calls.
    pub fn stop(&self) {
        self.service.cancellation_token().cancel();
        if let Some(process) = &self.process {
            process.kill();
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn list_tools(
    service: &RunningService<RoleClient, ()>,
    limits: &Limits,
) -> Result<Vec<McpToolInfo>, McpError> {
    let mut tools = vec![];
    let mut bytes = 0;
    let mut cursor = None;
    for _ in 0..MAX_LISTING_PAGES {
        let mut params = PaginatedRequestParams::default();
        params.cursor = cursor;
        let page = service
            .peer()
            .list_tools(Some(params))
            .await
            .map_err(McpError::Service)?;
        for tool in page.tools {
            if tools.len() == limits.max_tools {
                return Err(McpError::TooManyTools {
                    limit: limits.max_tools,
                });
            }
            let input_schema =
                serde_json::to_string(&*tool.input_schema).map_err(McpError::Schema)?;
            let info = McpToolInfo {
                name: tool.name.into_owned(),
                title: tool.title,
                description: tool.description.map(|d| d.into_owned()),
                input_schema,
            };
            bytes += info.name.len()
                + info.input_schema.len()
                + info.title.as_ref().map_or(0, String::len)
                + info.description.as_ref().map_or(0, String::len);
            if bytes > limits.max_listing_bytes {
                return Err(McpError::ListingTooLarge {
                    limit: limits.max_listing_bytes,
                });
            }
            tools.push(info);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(tools);
        }
    }
    Err(McpError::TooManyTools {
        limit: limits.max_tools,
    })
}

fn content(block: ContentBlock) -> Content {
    match block {
        ContentBlock::Text(text) => Content::Text(text.text),
        ContentBlock::Image(image) => Content::Image {
            mime_type: image.mime_type,
            data: image.data,
        },
        ContentBlock::Audio(audio) => Content::Audio {
            mime_type: audio.mime_type,
            data: audio.data,
        },
        ContentBlock::Resource(resource) => match resource.resource {
            ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                ..
            } => Content::Resource {
                uri,
                mime_type,
                text: Some(text),
            },
            ResourceContents::BlobResourceContents { uri, mime_type, .. } => Content::Resource {
                uri,
                mime_type,
                text: None,
            },
            _ => Content::Unsupported,
        },
        ContentBlock::ResourceLink(link) => Content::Link {
            uri: link.uri,
            name: link.name,
            description: link.description,
        },
        _ => Content::Unsupported,
    }
}
