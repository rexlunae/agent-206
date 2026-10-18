//! Bundled skills extension. Only the gateway can authorize approved skill reads.
use a206_extension_sdk::{Args, Extension, GuestError, ToolOutput, host, string_arg};
struct Skills;
a206_extension_sdk::export!(Skills);
impl Extension for Skills {
    fn tool(name: &str, args: &Args) -> Result<ToolOutput, GuestError> {
        let text = match name {
            "skill_list" => host::skill_list()?,
            "skill_info" => {
                let name = string_arg(args, "name")?
                    .ok_or_else(|| GuestError::new("invalid_argument", "name is required"))?;
                host::skill_read(name, string_arg(args, "path")?)?
            }
            _ => return Err(GuestError::new("unknown_tool", "unknown skill tool")),
        };
        // Instructions are never excerpted: the host checks the complete result.
        Ok(ToolOutput { text })
    }
}
