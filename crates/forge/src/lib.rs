//! Pull requests, their checks and their reviews, described the same way
//! whichever forge holds the repository. Each adapter turns a question into
//! the URL of its forge's REST API and the answer into these types; this
//! crate does no I/O, so the gateway sends every request through the vault
//! broker, which attaches the token.
//!
//! Forges word things differently (a GitLab merge request is a pull request
//! here, a pipeline job is a check). What a forge reports that has no
//! counterpart is `Unknown`, with the forge's own word kept in `reported`.
mod github;
mod gitlab;

use a206_core::{ForgeApi, ForgeKind};
use serde::Serialize;
use url::Url;

/// Pull requests asked for at once; a full page means there may be more.
pub const PAGE: usize = 30;
/// Checks and reviews read for one pull request.
pub const DETAIL_PAGE: usize = 100;
/// Longest title, name or label kept, in characters.
pub const MAX_LABEL: usize = 256;
/// Longest description kept, in characters.
pub const MAX_BODY: usize = 8_000;

#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error(
        "the forge API address must be an https:// URL without credentials, a query or a fragment"
    )]
    ApiUrl,
    #[error("the remote URL does not name a repository on this {0} forge")]
    RepositoryPath(&'static str),
    #[error("the forge's answer is not what its API documents")]
    Answer(#[source] serde_json::Error),
    #[error("the pull request has no commit to read checks for")]
    NoCommit,
}

/// The bundled API location of a well-known forge, by the origin its git
/// remotes use.
pub fn preset(origin: &str) -> Option<ForgeApi> {
    let (kind, api) = match origin {
        "https://github.com" => (ForgeKind::GitHub, "https://api.github.com"),
        "https://gitlab.com" => (ForgeKind::GitLab, "https://gitlab.com/api/v4"),
        _ => return None,
    };
    Some(ForgeApi {
        kind,
        api: api.into(),
    })
}

/// An API base URL as a forge setting may give it: HTTPS, or HTTP on a
/// loopback host, with no credentials, query or fragment.
pub fn api_url(api: &str) -> Result<Url, ForgeError> {
    let url = Url::parse(api.trim()).map_err(|_| ForgeError::ApiUrl)?;
    let loopback = matches!(
        url.host(),
        Some(url::Host::Domain("localhost"))
            | Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
            | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
    );
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.cannot_be_a_base()
    {
        return Err(ForgeError::ApiUrl);
    }
    Ok(url)
}

/// Which pull requests to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Listed {
    #[default]
    Open,
    /// Closed without being merged.
    Closed,
    Merged,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Open,
    Closed,
    Merged,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullRequest {
    /// The number people use for it (a GitLab merge request's `iid`).
    pub number: u64,
    pub title: String,
    pub state: State,
    pub draft: bool,
    pub author: Option<String>,
    /// The branch it merges from, and the one it merges into.
    pub source: String,
    pub target: String,
    /// The source branch's commit, as the forge last saw it.
    pub head: Option<String>,
    pub url: Option<String>,
    /// The description, only when one pull request is read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub body_shortened: bool,
    pub created: Option<String>,
    pub updated: Option<String>,
    /// GitLab: the pipeline whose jobs are this merge request's checks.
    #[serde(skip)]
    pipeline: Option<u64>,
}

/// One page of pull requests, newest first as the forge orders them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullRequests {
    pub pull_requests: Vec<PullRequest>,
    /// `false` when the page was full, so older ones may be left out.
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Queued,
    Running,
    Completed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    Success,
    Failure,
    /// Finished without passing or failing, or failed where failure is
    /// allowed.
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    Stale,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    /// Only for a completed check.
    pub conclusion: Option<Conclusion>,
    /// The forge's own word for the check's state.
    pub reported: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Review {
    pub author: Option<String>,
    pub state: ReviewState,
    pub reported: String,
    pub submitted: Option<String>,
    /// The commit the review was given on, where the forge says.
    pub commit: Option<String>,
}

/// What one request for checks returns, and how to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksCall {
    pub url: Url,
    source: ChecksSource,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChecksSource {
    /// GitHub check runs.
    Runs,
    /// GitHub commit statuses.
    Statuses,
    /// GitLab pipeline jobs.
    Jobs,
}

/// A repository on a forge, named by its git remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    kind: ForgeKind,
    api: Url,
    /// `owner/name` on GitHub, `group/…/project` on GitLab.
    path: Vec<String>,
}
impl Repository {
    /// The repository `remote` (an HTTPS git URL) names, on the forge whose
    /// API is `api`.
    pub fn new(api: &ForgeApi, remote: &Url) -> Result<Self, ForgeError> {
        let base = api_url(&api.api)?;
        let mut path: Vec<String> = remote
            .path_segments()
            .into_iter()
            .flatten()
            .filter(|segment| !segment.is_empty())
            .map(|segment| {
                // Not UTF-8 leaves it empty, which is refused below.
                percent_encoding::percent_decode_str(segment)
                    .decode_utf8()
                    .map_or_else(|_| String::new(), |decoded| decoded.into_owned())
            })
            .collect();
        if let Some(last) = path.last_mut()
            && let Some(name) = last.strip_suffix(".git")
        {
            *last = name.to_owned();
        }
        let valid = path.iter().all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && !segment.chars().any(|c| c == '/' || c.is_control())
        });
        let fits = match api.kind {
            ForgeKind::GitHub => path.len() == 2,
            ForgeKind::GitLab => path.len() >= 2,
        };
        if !valid || !fits {
            return Err(ForgeError::RepositoryPath(api.kind.label()));
        }
        Ok(Self {
            kind: api.kind,
            api: base,
            path,
        })
    }

    pub fn kind(&self) -> ForgeKind {
        self.kind
    }
    /// The repository's path on the forge, such as `owner/name`.
    pub fn name(&self) -> String {
        self.path.join("/")
    }

    /// The pull requests to list: `listed`, from branch `source` if given.
    pub fn list_url(&self, listed: Listed, source: Option<&str>) -> Url {
        match self.kind {
            ForgeKind::GitHub => github::list_url(self, listed, source),
            ForgeKind::GitLab => gitlab::list_url(self, listed, source),
        }
    }
    pub fn parse_list(&self, body: &[u8], listed: Listed) -> Result<PullRequests, ForgeError> {
        let (pull_requests, read) = match self.kind {
            ForgeKind::GitHub => github::parse_list(body, listed)?,
            ForgeKind::GitLab => gitlab::parse_list(body)?,
        };
        Ok(PullRequests {
            pull_requests,
            complete: read < PAGE,
        })
    }

    pub fn pull_request_url(&self, number: u64) -> Url {
        match self.kind {
            ForgeKind::GitHub => github::pull_url(self, number),
            ForgeKind::GitLab => gitlab::pull_url(self, number),
        }
    }
    pub fn parse_pull_request(&self, body: &[u8]) -> Result<PullRequest, ForgeError> {
        match self.kind {
            ForgeKind::GitHub => github::parse_pull(body),
            ForgeKind::GitLab => gitlab::parse_pull(body),
        }
    }

    /// The requests that read `pull`'s checks; none on GitLab when no
    /// pipeline ran for it.
    pub fn checks_calls(&self, pull: &PullRequest) -> Result<Vec<ChecksCall>, ForgeError> {
        match self.kind {
            ForgeKind::GitHub => {
                let head = pull.head.as_deref().ok_or(ForgeError::NoCommit)?;
                Ok(vec![
                    ChecksCall {
                        url: github::runs_url(self, head),
                        source: ChecksSource::Runs,
                    },
                    ChecksCall {
                        url: github::statuses_url(self, head),
                        source: ChecksSource::Statuses,
                    },
                ])
            }
            ForgeKind::GitLab => Ok(pull
                .pipeline
                .map(|pipeline| ChecksCall {
                    url: gitlab::jobs_url(self, pipeline),
                    source: ChecksSource::Jobs,
                })
                .into_iter()
                .collect()),
        }
    }
    pub fn parse_checks(&self, call: &ChecksCall, body: &[u8]) -> Result<Vec<Check>, ForgeError> {
        match call.source {
            ChecksSource::Runs => github::parse_runs(body),
            ChecksSource::Statuses => github::parse_statuses(body),
            ChecksSource::Jobs => gitlab::parse_jobs(body),
        }
    }

    pub fn reviews_url(&self, number: u64) -> Url {
        match self.kind {
            ForgeKind::GitHub => github::reviews_url(self, number),
            ForgeKind::GitLab => gitlab::approvals_url(self, number),
        }
    }
    pub fn parse_reviews(&self, body: &[u8]) -> Result<Vec<Review>, ForgeError> {
        match self.kind {
            ForgeKind::GitHub => github::parse_reviews(body),
            ForgeKind::GitLab => gitlab::parse_approvals(body),
        }
    }

    /// `segments` under the API base.
    fn endpoint<'a>(&self, segments: impl IntoIterator<Item = &'a str>) -> Url {
        let mut url = self.api.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        // A base that cannot take a path was refused by `api_url`.
        url
    }
}

/// `text` without control characters other than line breaks and tabs, cut
/// to at most `max` characters; `true` when it was cut.
fn bounded(text: &str, max: usize) -> (String, bool) {
    let mut kept = String::new();
    let mut count = 0;
    for c in text.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        if count == max {
            return (kept, true);
        }
        kept.push(c);
        count += 1;
    }
    (kept, false)
}

fn label(text: &str) -> String {
    bounded(text, MAX_LABEL).0
}

fn label_opt(text: Option<String>) -> Option<String> {
    text.as_deref().map(label)
}

fn answer<'a, T: serde::Deserialize<'a>>(body: &'a [u8]) -> Result<T, ForgeError> {
    serde_json::from_slice(body).map_err(ForgeError::Answer)
}

#[cfg(test)]
mod tests;
