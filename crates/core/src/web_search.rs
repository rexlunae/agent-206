//! Web search services the native `web_search` tool sends queries to.
//!
//! A service is configured like a model provider: a name the user chose, the
//! service's API, its endpoint and the vault credential the broker attaches.
//! The bundled defaults name only each service's official endpoint and how
//! to get a key; nothing is enabled until the user adds a service.
use crate::Id;
use serde::{Deserialize, Serialize};

/// Most search services one settings file may hold.
pub const MAX_SERVICES: usize = 16;
/// Longest service name, in characters.
pub const MAX_LABEL: usize = 64;

/// The API a search service speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchApi {
    /// Brave Search's web search API: keyword search with snippets.
    Brave,
    /// Exa's search API: semantic search that returns page text.
    Exa,
}
impl SearchApi {
    pub const ALL: [Self; 2] = [Self::Brave, Self::Exa];
    pub fn label(self) -> &'static str {
        match self {
            Self::Brave => "Brave Search",
            Self::Exa => "Exa",
        }
    }
    /// The service's official API endpoint.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Brave => "https://api.search.brave.com/res/v1",
            Self::Exa => "https://api.exa.ai",
        }
    }
    /// How to get and store a key for the service.
    pub fn setup(self) -> &'static str {
        match self {
            Self::Brave => {
                "Create an API key in the Brave Search API dashboard, store it in the vault and bind it to https://api.search.brave.com."
            }
            Self::Exa => {
                "Create an API key in the Exa dashboard, store it in the vault and bind it to https://api.exa.ai."
            }
        }
    }
}

/// A search service the user added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchConfig {
    pub id: Id,
    /// The name the model and the user pick the service by.
    pub label: String,
    pub api: SearchApi,
    /// HTTPS (or loopback HTTP) endpoint; see [`SearchApi::default_base_url`].
    pub base_url: String,
    /// The vault credential holding the key, bound to the endpoint's origin.
    pub credential: String,
}
impl SearchConfig {
    /// A service of `api` at its official endpoint.
    pub fn new(label: String, api: SearchApi, credential: String) -> Self {
        Self {
            id: Id::new_v4(),
            label,
            api,
            base_url: api.default_base_url().to_owned(),
            credential,
        }
    }
}

/// Why a list of search services cannot be saved.
#[derive(Debug, thiserror::Error)]
pub enum SearchConfigError {
    #[error("at most {MAX_SERVICES} search services may be configured")]
    TooMany,
    #[error("a search service needs a name of at most {MAX_LABEL} characters")]
    Label,
    #[error("two search services are named `{0}`")]
    DuplicateLabel(String),
    #[error("two search services have the same id")]
    DuplicateId,
    #[error("search service `{0}` needs a vault credential name")]
    Credential(String),
    #[error("search service `{label}` has an endpoint the gateway refuses to call")]
    Endpoint {
        label: String,
        #[source]
        source: crate::providers::EndpointError,
    },
}

/// Check services before they are saved: names unique and short, a
/// credential for each, endpoints the gateway would call.
pub fn validate(services: &[SearchConfig]) -> Result<(), SearchConfigError> {
    if services.len() > MAX_SERVICES {
        return Err(SearchConfigError::TooMany);
    }
    let mut labels = std::collections::BTreeSet::new();
    let mut ids = std::collections::BTreeSet::new();
    for service in services {
        let label = service.label.trim();
        if label.is_empty() || label != service.label || label.chars().count() > MAX_LABEL {
            return Err(SearchConfigError::Label);
        }
        if !labels.insert(label.to_lowercase()) {
            return Err(SearchConfigError::DuplicateLabel(label.to_owned()));
        }
        if !ids.insert(service.id) {
            return Err(SearchConfigError::DuplicateId);
        }
        if service.credential.trim().is_empty() {
            return Err(SearchConfigError::Credential(label.to_owned()));
        }
        crate::providers::base_url(&service.base_url).map_err(|source| {
            SearchConfigError::Endpoint {
                label: label.to_owned(),
                source,
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(api: SearchApi) -> SearchConfig {
        SearchConfig::new(
            Id::new_v4().simple().to_string(),
            api,
            Id::new_v4().to_string(),
        )
    }

    #[test]
    fn services_are_checked_before_they_are_saved() {
        let (brave, exa) = (service(SearchApi::Brave), service(SearchApi::Exa));
        validate(&[brave.clone(), exa.clone()]).unwrap();

        let mut twin = exa.clone();
        twin.id = Id::new_v4();
        twin.label = brave.label.to_uppercase();
        assert!(matches!(
            validate(&[brave.clone(), twin]),
            Err(SearchConfigError::DuplicateLabel(_))
        ));
        let mut same = exa.clone();
        same.id = brave.id;
        assert!(matches!(
            validate(&[brave.clone(), same]),
            Err(SearchConfigError::DuplicateId)
        ));
        for label in [
            String::new(),
            format!(" {}", brave.label),
            "x".repeat(MAX_LABEL + 1),
        ] {
            let named = SearchConfig {
                label,
                ..brave.clone()
            };
            assert!(matches!(validate(&[named]), Err(SearchConfigError::Label)));
        }
        let keyless = SearchConfig {
            credential: " ".into(),
            ..brave.clone()
        };
        assert!(matches!(
            validate(&[keyless]),
            Err(SearchConfigError::Credential(_))
        ));
        let plain = SearchConfig {
            base_url: format!("http://{}.example", Id::new_v4().simple()),
            ..brave.clone()
        };
        assert!(matches!(
            validate(&[plain]),
            Err(SearchConfigError::Endpoint { .. })
        ));
        let many: Vec<_> = (0..=MAX_SERVICES)
            .map(|_| service(SearchApi::Exa))
            .collect();
        assert!(matches!(validate(&many), Err(SearchConfigError::TooMany)));
    }
}
