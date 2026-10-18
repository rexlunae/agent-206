//! `agent206 teams`: the agent teams a conversation can start, and the team
//! files the gateway could not use, as TOML; `agent206 teams start` starts
//! one on a conversation exactly as the conversation details do.
use a206_core::{AgentTeams, Command, Reply};
use a206_runtime::client::Client;
use anyhow::{Result, bail};
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum TeamAction {
    /// Start a team on a conversation: a subagent from the team's
    /// coordinator profile that hands tasks to its member profiles.
    Start {
        /// The team's ID: its file name without `.toml`.
        team: String,
        /// The conversation to start it from.
        #[arg(long)]
        parent: uuid::Uuid,
    },
}

pub async fn run(client: &Client, action: Option<TeamAction>) -> Result<()> {
    match action {
        None => {
            print!("{}", render(&teams(client).await?)?);
            Ok(())
        }
        Some(TeamAction::Start { team, parent }) => {
            let listed = teams(client).await?;
            let Some(team) = listed.get(&team) else {
                bail!("no usable agent team is named `{team}`; run `agent206 teams` to see why");
            };
            let profiles = match client.call(Command::AgentProfiles).await? {
                Reply::AgentProfiles(profiles) => profiles,
                Reply::Error(report) => return Err(report.into()),
                _ => bail!("gateway did not list agent profiles"),
            };
            let Reply::Snapshot(snapshot) = client.call(Command::Snapshot).await? else {
                bail!("gateway did not send a snapshot");
            };
            let Some(thread) = snapshot.threads.iter().find(|t| t.id == parent) else {
                bail!("no conversation has ID {parent}");
            };
            let providers = &snapshot.settings.providers;
            let Some(command) = a206_core::teams::start(team, thread, &profiles, providers) else {
                bail!(
                    "coordinator profile `{}` is unusable, or does not match exactly one of the conversation's subagent models",
                    team.coordinator
                );
            };
            match client.call(command).await? {
                Reply::Thread(thread) => {
                    println!("{}", thread.id);
                    Ok(())
                }
                Reply::Error(report) => Err(report.into()),
                _ => bail!("gateway did not start the team"),
            }
        }
    }
}

async fn teams(client: &Client) -> Result<AgentTeams> {
    match client.call(Command::AgentTeams).await? {
        Reply::AgentTeams(teams) => Ok(teams),
        Reply::Error(report) => Err(report.into()),
        _ => bail!("gateway did not list agent teams"),
    }
}

fn render(teams: &AgentTeams) -> Result<String> {
    Ok(toml::to_string_pretty(teams)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{AgentTeam, ErrorKind, ErrorReport, ProfileProblem};

    #[test]
    fn teams_and_unusable_files_are_listed_as_toml() {
        let id = format!("t{}", uuid::Uuid::new_v4().simple());
        let member = format!("m{}", uuid::Uuid::new_v4().simple());
        let message = uuid::Uuid::new_v4().to_string();
        let listing = AgentTeams {
            teams: vec![AgentTeam {
                id: id.clone(),
                name: id.clone(),
                description: None,
                coordinator: "lead".into(),
                members: vec![member.clone()],
                instructions: String::new(),
                digest: String::new(),
            }],
            problems: vec![ProfileProblem {
                file: "broken.toml".into(),
                error: ErrorReport::new(ErrorKind::InvalidRequest, message.clone()),
            }],
        };
        let parsed: toml::Table = toml::from_str(&render(&listing).unwrap()).unwrap();
        assert_eq!(parsed["teams"][0]["id"].as_str(), Some(id.as_str()));
        assert_eq!(
            parsed["teams"][0]["members"][0].as_str(),
            Some(member.as_str())
        );
        assert_eq!(
            parsed["problems"][0]["error"]["message"].as_str(),
            Some(message.as_str())
        );
    }
}
