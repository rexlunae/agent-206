//! A tiny MCP server used by tests: it runs under the real sandbox on
//! platforms that have one. Not part of the shipped application.
#![forbid(unsafe_code)]
mod server;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if !server::serve((tokio::io::stdin(), tokio::io::stdout())).await {
        std::process::exit(1);
    }
}
