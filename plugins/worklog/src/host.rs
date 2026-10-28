//! The host calls the work queue tools use, behind a trait so the tool logic is
//! tested natively against an in-memory workspace.
use a206_extension_sdk::{HostFailure, host};

pub trait Host {
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, HostFailure>;
    fn create_directory(&mut self, path: &str) -> Result<(), HostFailure>;
    fn write_file(
        &mut self,
        path: &str,
        contents: &[u8],
        overwrite: bool,
    ) -> Result<(), HostFailure>;
    fn edit_file(&mut self, path: &str, old: &str, new: &str) -> Result<(), HostFailure>;
}

/// The gateway, through the ABI's host calls.
pub struct Gateway;
impl Host for Gateway {
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, HostFailure> {
        host::read_file(path)
    }
    fn create_directory(&mut self, path: &str) -> Result<(), HostFailure> {
        host::create_directory(path)
    }
    fn write_file(
        &mut self,
        path: &str,
        contents: &[u8],
        overwrite: bool,
    ) -> Result<(), HostFailure> {
        host::write_file(path, contents, overwrite)
    }
    fn edit_file(&mut self, path: &str, old: &str, new: &str) -> Result<(), HostFailure> {
        host::edit_file(path, old, new)
    }
}
