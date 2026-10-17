//! Isolated vault broker. The broker is the only process that decrypts the
//! vault. Trusted user surfaces (desktop, TUI, CLI) talk to it directly over a
//! user-private socket; the gateway may only ask it to perform
//! destination-bound provider requests and never sees a credential value.
#![forbid(unsafe_code)]
pub mod channel;
#[cfg(unix)]
mod client;
mod proto;
#[cfg(unix)]
mod server;

#[cfg(unix)]
pub use client::{Client, ClientError, ProviderStream, StreamViolation};
pub use proto::*;
#[cfg(unix)]
pub use server::serve;

pub fn socket_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("vault").join("broker.sock")
}

/// The encrypted vault file the broker serves for a profile.
pub fn vault_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("vault").join("secrets.json")
}
