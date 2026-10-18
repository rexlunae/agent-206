//! `agent206 profiles`: the agent profiles subagents can start from, and the
//! profile files the gateway could not use, as TOML.
use a206_core::{AgentProfiles, Command, Reply};
use a206_runtime::client::Client;
use anyhow::{Result, bail};

pub async fn run(client: &Client) -> Result<()> {
    match client.call(Command::AgentProfiles).await? {
        Reply::AgentProfiles(profiles) => {
            print!("{}", render(&profiles)?);
            Ok(())
        }
        Reply::Error(report) => Err(report.into()),
        _ => bail!("gateway did not list agent profiles"),
    }
}

fn render(profiles: &AgentProfiles) -> Result<String> {
    Ok(toml::to_string_pretty(profiles)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{AgentProfile, ErrorKind, ErrorReport, Mode, ProfileProblem};

    #[test]
    fn profiles_and_unusable_files_are_listed_as_toml() {
        let id = format!("p{}", uuid::Uuid::new_v4().simple());
        let message = uuid::Uuid::new_v4().to_string();
        let listing = AgentProfiles {
            profiles: vec![AgentProfile {
                id: id.clone(),
                name: id.clone(),
                description: None,
                mode: Some(Mode::Review),
                model: None,
                connection: None,
                capabilities: Some(Default::default()),
                instructions: String::new(),
                digest: String::new(),
            }],
            problems: vec![ProfileProblem {
                file: "broken.toml".into(),
                error: ErrorReport::new(ErrorKind::InvalidRequest, message.clone())
                    .caused_by(ErrorReport::new(ErrorKind::InvalidRequest, "cause")),
            }],
        };
        let text = render(&listing).unwrap();
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(parsed["profiles"][0]["id"].as_str(), Some(id.as_str()));
        assert_eq!(parsed["profiles"][0]["mode"].as_str(), Some("review"));
        assert_eq!(
            parsed["problems"][0]["error"]["message"].as_str(),
            Some(message.as_str())
        );
    }
}
