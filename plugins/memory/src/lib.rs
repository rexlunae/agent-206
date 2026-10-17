//! Bundled memory extension: BM25 search and bounded reads over a project's
//! `MEMORY.md` and `memory/**/*.md`, and appending notes under `memory/`.
//! Every file access is a host call the gateway authorizes against the
//! tool's declared grant: `read_workspace` for searching and reading, and
//! `read_workspace` + `write_workspace` for saving.
use a206_extension_sdk::{Args, Extension, GuestError, ToolOutput, excerpt};

mod bm25;
mod corpus;
mod error;
mod host;
mod paths;
mod tools;

struct Memory;
a206_extension_sdk::export!(Memory);

impl Extension for Memory {
    fn tool(name: &str, args: &Args) -> Result<ToolOutput, GuestError> {
        let host = &mut host::Gateway;
        let text = match name {
            "memory_search" => tools::search(host, args),
            "memory_get" => tools::get(host, args),
            "memory_save" => tools::save(host, args),
            _ => return Err(GuestError::new("unknown_tool", format!("no tool `{name}`"))),
        }?;
        Ok(ToolOutput {
            text: excerpt(text),
        })
    }
}

#[cfg(test)]
mod tests;
