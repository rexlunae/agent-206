//! MCP harness introspection scoped to a single user-selected conversation.
use crate::client::Client;
use a206_core::{Command, Reply};
use rmcp::{
    ServerHandler, ServiceExt, handler::server::router::tool::ToolRouter, tool, tool_handler,
    tool_router,
};
use uuid::Uuid;
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Rename {
    title: String,
}
#[derive(Clone)]
struct Harness {
    client: Client,
    thread: Uuid,
    allow_title_edit: bool,
    tool_router: ToolRouter<Self>,
}
#[tool_router]
impl Harness {
    #[tool(
        description = "Rename this conversation to a concise title. Cannot change another conversation or alter permissions."
    )]
    async fn rename_thread(
        &self,
        params: rmcp::handler::server::wrapper::Parameters<Rename>,
    ) -> Result<String, String> {
        if !self.allow_title_edit {
            return Err("title editing was not authorized for this MCP session".into());
        }
        match self
            .client
            .call(Command::RenameThread {
                id: self.thread,
                title: params.0.title,
            })
            .await
            .map_err(|e| shown("rename_thread", &e))?
        {
            Reply::Ok => Ok("Conversation renamed".into()),
            // The gateway already recorded this failure; render its whole chain.
            Reply::Error(report) => Err(report.detail()),
            _ => Err("unexpected gateway response".into()),
        }
    }

    #[tool(
        description = "Read this conversation's mode, run state, granted tool surface and permitted subagent models. Does not expose prompts, credentials or other conversations."
    )]
    async fn thread_state(&self) -> Result<String, String> {
        let Reply::Snapshot(snapshot) = self
            .client
            .call(Command::Snapshot)
            .await
            .map_err(|e| shown("thread_state", &e))?
        else {
            return Err("gateway unavailable".into());
        };
        let thread = snapshot
            .threads
            .iter()
            .find(|t| t.id == self.thread)
            .ok_or("bound conversation no longer exists")?;
        #[derive(serde::Serialize)]
        struct View<'a> {
            id: Uuid,
            mode: a206_core::Mode,
            state: &'a a206_core::RunState,
            capabilities: &'a std::collections::BTreeSet<a206_core::Capability>,
            subagent_models: &'a std::collections::BTreeSet<a206_core::ModelKey>,
        }
        toml::to_string_pretty(&View {
            id: thread.id,
            mode: thread.mode,
            state: &thread.state,
            capabilities: &thread.grant.capabilities,
            subagent_models: &thread.subagent_models,
        })
        .map_err(|e| shown("thread_state", &e.into()))
    }
    #[tool(
        description = "Read the harness extension and permission contract. This tool cannot approve privileges, change settings, execute commands or read secret values."
    )]
    async fn harness_contract(&self) -> String {
        "Agent 206: only the user can approve permission changes. Extension ABI v1 accepts no imports and returns bounded MessagePack display fields. Mode and parent grants cap task authority. Provider/model choices are API-discovered; do not invent model IDs. Ask the user through the conversation when additional access is needed.".into()
    }
}
/// A tool result is the model-facing display edge: record the failure with its
/// chain, then render it as the text the tool returns.
fn shown(operation: &'static str, error: &anyhow::Error) -> String {
    crate::error::handle(operation, error).detail()
}
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Harness {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        {
            let mut config = rmcp::model::ServerConfig::default();
            config.capabilities = rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build();
            config.instructions = Some(if self.allow_title_edit {
                "Harness introspection and explicitly authorized title editing, bound to one conversation."
            } else {
                "Read-only harness introspection, bound to one conversation."
            }.into());
            config
        }
    }
}
pub async fn serve(client: Client, thread: Uuid, allow_title_edit: bool) -> anyhow::Result<()> {
    let mut tool_router = Harness::tool_router();
    if !allow_title_edit {
        tool_router.remove_route("rename_thread");
    }
    let service = Harness {
        client,
        thread,
        allow_title_edit,
        tool_router,
    }
    .serve(rmcp::transport::stdio())
    .await?;
    service.waiting().await?;
    Ok(())
}
