//! `agent206 running`: every conversation with a response in progress, and
//! what each is waiting on, as the desktop's Running work page shows it.
use a206_core::{Command, Reply, RunState, Snapshot};
use a206_runtime::client::Client;
use anyhow::{Result, bail};
use serde::Serialize;

#[derive(Debug, PartialEq, Serialize)]
struct Listing {
    running: Vec<Entry>,
}

#[derive(Debug, PartialEq, Serialize)]
struct Entry {
    id: uuid::Uuid,
    title: String,
    /// The conversation that delegated this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<uuid::Uuid>,
    /// Paused by the user; the run stops before its next step.
    paused: bool,
    /// Waiting for its own pause, or an ancestor's, to end.
    held: bool,
    approvals: usize,
    questions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    activity: Option<String>,
}

fn listing(snapshot: Snapshot) -> Listing {
    Listing {
        running: snapshot
            .threads
            .into_iter()
            .filter(|t| t.state == RunState::Running)
            .map(|t| Entry {
                id: t.id,
                title: t.title,
                parent: t.parent,
                paused: t.paused,
                held: t.held,
                approvals: t.approvals.len(),
                questions: t.prompts.len(),
                activity: t.backend_activity,
            })
            .collect(),
    }
}

pub async fn run(client: &Client) -> Result<()> {
    let snapshot = match client.call(Command::Snapshot).await? {
        Reply::Snapshot(snapshot) => snapshot,
        Reply::Error(report) => return Err(report.into()),
        _ => bail!("gateway did not return its state"),
    };
    print!("{}", toml::to_string_pretty(&listing(*snapshot))?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{Mode, Thread};

    #[test]
    fn only_running_conversations_are_listed_with_their_pause_state() {
        let mut parent = Thread::new(uuid::Uuid::new_v4().to_string(), Mode::Chat);
        parent.state = RunState::Running;
        parent.paused = true;
        let mut child = Thread::new(uuid::Uuid::new_v4().to_string(), Mode::Chat);
        child.state = RunState::Running;
        child.parent = Some(parent.id);
        child.held = true;
        let done = Thread::new(uuid::Uuid::new_v4().to_string(), Mode::Chat);
        let snapshot = a206_core::Snapshot {
            settings: Default::default(),
            threads: vec![parent.clone(), done, child.clone()],
            projects: vec![],
            catalogs: vec![],
            downloads: vec![],
            plugins: vec![],
            messengers: vec![],
            last_event: 0,
            managed_joshua_provider: None,
            mcp_servers: vec![],
            jobs: vec![],
            extension_proposals: vec![],
        };
        let shown = listing(snapshot);
        assert_eq!(
            shown.running.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![parent.id, child.id]
        );
        assert!(shown.running[0].paused && !shown.running[0].held);
        assert_eq!(shown.running[1].parent, Some(parent.id));
        assert!(shown.running[1].held);
        let printed = toml::to_string_pretty(&shown).unwrap();
        assert!(printed.contains(&child.title), "{printed}");
    }
}
