//! Bundled work queue extension: keeps a project's Literate AI work queue
//! (`docs/roadmap/active-work.md`) and changelog the way `litai work record`
//! and `litai work close` do, so a model records, checks and closes work
//! items through validated tools instead of editing checklist Markdown by
//! hand. Every file access is a host call the gateway authorizes against the
//! tool's declared grant: `read_workspace` for listing, and
//! `read_workspace` + `write_workspace` for every change.
use a206_extension_sdk::{Args, Extension, GuestError, ToolOutput, excerpt};

mod changelog;
mod error;
mod host;
mod queue;
mod tools;

struct Worklog;
a206_extension_sdk::export!(Worklog);

impl Extension for Worklog {
    fn tool(name: &str, args: &Args) -> Result<ToolOutput, GuestError> {
        let host = &mut host::Gateway;
        let text = match name {
            "work_list" => tools::list(host, args),
            "work_record" => tools::record(host, args),
            "work_add" => tools::add(host, args),
            "work_check" => tools::check(host, args),
            "work_close" => tools::close(host, args),
            "changelog_add" => tools::changelog_add(host, args),
            _ => return Err(GuestError::new("unknown_tool", format!("no tool `{name}`"))),
        }?;
        Ok(ToolOutput {
            text: excerpt(text),
        })
    }
}

#[cfg(test)]
mod tests;
