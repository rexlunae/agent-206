//! The host calls the memory tools use, behind a trait so the tool logic is
//! tested natively against an in-memory workspace.
use a206_extension_sdk::{DirEntry, HostFailure, host};

pub trait Host {
    fn list_directory(&mut self, path: &str) -> Result<Vec<DirEntry>, HostFailure>;
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
    fn list_directory(&mut self, path: &str) -> Result<Vec<DirEntry>, HostFailure> {
        host::list_directory(path)
    }
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
