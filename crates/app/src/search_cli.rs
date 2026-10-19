//! `agent206 search-services`: the web search services the native
//! `web_search` tool may query. Every change is a `SaveSettings` through the
//! gateway, which validates it; a key is named by its vault credential,
//! never given here.
use a206_core::{
    Command, Reply, Settings,
    web_search::{SearchApi, SearchConfig},
};
use a206_runtime::client::Client;
use anyhow::{Result, bail};
use clap::{Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Api {
    Brave,
    Exa,
}
impl From<Api> for SearchApi {
    fn from(api: Api) -> Self {
        match api {
            Api::Brave => SearchApi::Brave,
            Api::Exa => SearchApi::Exa,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum SearchAction {
    /// List the configured search services as TOML.
    List,
    /// Add a search service at its official endpoint, or at `--endpoint`.
    Add {
        /// The name the model and you pick the service by.
        name: String,
        #[arg(long, value_enum)]
        api: Api,
        /// Vault credential holding the key, bound to the endpoint's origin.
        #[arg(long)]
        credential: String,
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Remove a search service.
    Remove { name: String },
}

/// The settings after `action`, or `None` when it changes nothing.
fn edit(mut settings: Settings, action: SearchAction) -> Result<Option<Settings>> {
    match action {
        SearchAction::List => return Ok(None),
        SearchAction::Add {
            name,
            api,
            credential,
            endpoint,
        } => {
            let mut service = SearchConfig::new(name, api.into(), credential);
            if let Some(endpoint) = endpoint {
                service.base_url = endpoint;
            }
            settings.search_services.push(service);
        }
        SearchAction::Remove { name } => {
            let before = settings.search_services.len();
            settings.search_services.retain(|s| s.label != name);
            if settings.search_services.len() == before {
                bail!("no search service is named `{name}`");
            }
        }
    }
    Ok(Some(settings))
}

pub async fn run(client: &Client, action: SearchAction) -> Result<()> {
    let snapshot = match client.call(Command::Snapshot).await? {
        Reply::Snapshot(snapshot) => snapshot,
        Reply::Error(report) => return Err(report.into()),
        _ => bail!("gateway did not return settings"),
    };
    let Some(settings) = edit(snapshot.settings.clone(), action)? else {
        #[derive(serde::Serialize)]
        struct Listing<'a> {
            services: &'a [SearchConfig],
        }
        print!(
            "{}",
            toml::to_string_pretty(&Listing {
                services: &snapshot.settings.search_services,
            })?
        );
        return Ok(());
    };
    match client
        .call(Command::SaveSettings(Box::new(settings)))
        .await?
    {
        Reply::Ok => println!("OK"),
        Reply::Error(report) => return Err(report.into()),
        _ => bail!("gateway sent an unexpected reply"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    #[test]
    fn services_are_added_at_their_endpoint_and_removed_by_name() {
        let (name, credential) = (random(), random());
        let settings = edit(
            Settings::default(),
            SearchAction::Add {
                name: name.clone(),
                api: Api::Exa,
                credential: credential.clone(),
                endpoint: None,
            },
        )
        .unwrap()
        .unwrap();
        let [service] = settings.search_services.as_slice() else {
            panic!("one service");
        };
        assert_eq!(
            (service.api, service.base_url.as_str(), &service.credential),
            (
                SearchApi::Exa,
                SearchApi::Exa.default_base_url(),
                &credential
            )
        );
        let endpoint = format!("https://{}.example/v1", random());
        let settings = edit(
            settings,
            SearchAction::Add {
                name: random(),
                api: Api::Brave,
                credential: random(),
                endpoint: Some(endpoint.clone()),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(settings.search_services[1].base_url, endpoint);
        let settings = edit(settings, SearchAction::Remove { name: name.clone() })
            .unwrap()
            .unwrap();
        assert_eq!(settings.search_services.len(), 1);
        assert!(edit(settings, SearchAction::Remove { name }).is_err());
    }
}
