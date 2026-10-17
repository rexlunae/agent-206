//! The fixture's tools. Kept apart from `main` so other crates' tests can
//! serve the same fixture in process (`#[path]`), where no sandbox exists to
//! launch it under.
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    service::RequestContext,
    tool, tool_handler, tool_router,
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Text {
    text: String,
}
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct File {
    path: String,
}
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Address {
    address: String,
}
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Program {
    program: String,
}
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Size {
    bytes: usize,
}

#[derive(Clone)]
struct Fixture {
    tool_router: ToolRouter<Self>,
}
#[tool_router]
impl Fixture {
    #[tool(description = "Return the text unchanged.")]
    async fn echo(&self, params: Parameters<Text>) -> String {
        params.0.text
    }
    #[tool(description = "Read a UTF-8 file.")]
    async fn read_file(&self, params: Parameters<File>) -> Result<String, String> {
        std::fs::read_to_string(&params.0.path).map_err(|error| error.kind().to_string())
    }
    #[tool(description = "Open a TCP connection, then close it.")]
    async fn connect(&self, params: Parameters<Address>) -> Result<String, String> {
        std::net::TcpStream::connect(&params.0.address)
            .map(|_| "connected".to_owned())
            .map_err(|error| error.kind().to_string())
    }
    #[tool(description = "Open a Unix socket connection, then close it.")]
    async fn connect_socket(&self, params: Parameters<File>) -> Result<String, String> {
        #[cfg(unix)]
        let connected = std::os::unix::net::UnixStream::connect(&params.0.path).map(|_| ());
        #[cfg(not(unix))]
        let connected: std::io::Result<()> = Err(std::io::ErrorKind::Unsupported.into());
        connected
            .map(|()| "connected".to_owned())
            .map_err(|error| error.kind().to_string())
    }
    #[tool(description = "Run a program without arguments and report its exit status.")]
    async fn run(&self, params: Parameters<Program>) -> Result<String, String> {
        std::process::Command::new(&params.0.program)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.to_string())
            .map_err(|error| error.kind().to_string())
    }
    #[tool(description = "Return a string of the requested length.")]
    async fn flood(&self, params: Parameters<Size>) -> String {
        "x".repeat(params.0.bytes)
    }
    #[tool(description = "Never return.")]
    async fn wait(&self) -> String {
        std::future::pending().await
    }
    #[tool(
        description = "Wait until the client cancels this call, then create the file at `path`."
    )]
    async fn wait_cancelled(
        &self,
        params: Parameters<File>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        context.ct.cancelled().await;
        std::fs::write(&params.0.path, b"cancelled").map_err(|error| error.kind().to_string())?;
        Ok(String::new())
    }
    #[tool(description = "Exit the server process.")]
    async fn exit(&self) -> String {
        std::process::exit(0)
    }
}
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Fixture {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        let mut config = rmcp::model::ServerConfig::default();
        config.capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .build();
        config
    }
}

/// Serve the fixture over `transport` until the client disconnects; false
/// if the session could not start or ended abnormally.
pub async fn serve<T, E, A>(transport: T) -> bool
where
    T: rmcp::transport::IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let Ok(service) = (Fixture {
        tool_router: Fixture::tool_router(),
    })
    .serve(transport)
    .await
    else {
        return false;
    };
    service.waiting().await.is_ok()
}
