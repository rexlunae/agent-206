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
/// Pages GitHub is asked for at most when looking for merged or unmerged
/// closed pull requests, which it lists together.
pub const MAX_LIST_PAGES: usize = 4;
/// Checks and reviews read for one pull request.
pub const DETAIL_PAGE: usize = 100;
/// Longest title, name or label kept, in characters.
pub const MAX_LABEL: usize = 256;
/// Longest description kept, in characters.
pub const MAX_BODY: usize = 8_000;
/// Longest description or comment sent, in characters; GitHub accepts
/// 65,536.
pub const MAX_TEXT: usize = 60_000;

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

/// Pull requests being listed, a page at a time: ask for [`Listing::next`]
/// until it is `None`, giving each answer to [`Listing::add`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    listed: Listed,
    source: Option<String>,
    /// Pages to ask for at most.
    pages: usize,
    /// Pages read so far.
    read: usize,
    /// The last page read was full, so the forge may have more.
    more: bool,
    found: Vec<PullRequest>,
}
impl Listing {
    /// The next page to ask for, if one is needed.
    pub fn next(&self, repository: &Repository) -> Option<Url> {
        (self.more && self.read < self.pages && self.found.len() < PAGE)
            .then(|| repository.list_url(self.listed, self.source.as_deref(), self.read + 1))
    }
    /// Reads the answer to the page [`Listing::next`] asked for.
    pub fn add(&mut self, repository: &Repository, body: &[u8]) -> Result<(), ForgeError> {
        let (found, read) = match repository.kind {
            ForgeKind::GitHub => github::parse_list(body, self.listed)?,
            ForgeKind::GitLab => gitlab::parse_list(body)?,
        };
        self.read += 1;
        self.more = read >= PAGE;
        self.found.extend(found);
        Ok(())
    }
    /// At most [`PAGE`] of them, and whether those are all there are.
    pub fn finish(mut self) -> PullRequests {
        let complete = !self.more && self.found.len() <= PAGE;
        self.found.truncate(PAGE);
        PullRequests {
            pull_requests: self.found,
            complete,
        }
    }
}

/// Pull requests newest first as the forge orders them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullRequests {
    pub pull_requests: Vec<PullRequest>,
    /// `false` when older ones may be left out.
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

/// A pull request to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewPullRequest<'a> {
    pub title: &'a str,
    pub body: Option<&'a str>,
    /// The branch of the repository whose commits it proposes.
    pub source: &'a str,
    /// The branch it would merge into.
    pub target: &'a str,
    pub draft: bool,
}

/// A comment as posted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Comment {
    pub id: u64,
    /// Its web address, where the forge gives one.
    pub url: Option<String>,
}

/// A request that changes something on the forge: where to send it and
/// its JSON body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub url: Url,
    pub body: Vec<u8>,
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
        let mut path = decoded_segments(remote);
        // A forge served under a path prefix has it in both addresses
        // (`https://host/gitlab/group/project.git` and
        // `https://host/gitlab/api/v4`); it is not part of the project.
        if remote.origin() == base.origin() {
            let api_path = decoded_segments(&base);
            if let Some(at) = api_path.iter().rposition(|segment| segment == "api") {
                let prefix = &api_path[..at];
                if path.len() > prefix.len() && path.iter().zip(prefix).all(|(a, b)| a == b) {
                    path.drain(..prefix.len());
                }
            }
        }
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

    /// Starts listing `listed` pull requests, from branch `source` if given.
    pub fn listing(&self, listed: Listed, source: Option<&str>) -> Listing {
        // GitHub lists merged and unmerged closed pull requests together, so
        // one page may hold few of those asked for.
        let pages = match (self.kind, listed) {
            (ForgeKind::GitHub, Listed::Closed | Listed::Merged) => MAX_LIST_PAGES,
            _ => 1,
        };
        Listing {
            listed,
            source: source.map(str::to_owned),
            pages,
            read: 0,
            more: true,
            found: vec![],
        }
    }

    /// Page `page` (from 1) of the pull requests to list.
    fn list_url(&self, listed: Listed, source: Option<&str>, page: usize) -> Url {
        let mut url = match self.kind {
            ForgeKind::GitHub => github::list_url(self, listed, source),
            ForgeKind::GitLab => gitlab::list_url(self, listed, source),
        };
        if page > 1 {
            url.query_pairs_mut().append_pair("page", &page.to_string());
        }
        url
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

    /// Opens `new`; the answer is read with
    /// [`Repository::parse_pull_request`].
    pub fn create_pull_request(&self, new: &NewPullRequest<'_>) -> Change {
        match self.kind {
            ForgeKind::GitHub => github::create(self, new),
            ForgeKind::GitLab => gitlab::create(self, new),
        }
    }

    /// Posts `text` as a comment on pull request `number`; the answer is
    /// read with [`Repository::parse_comment`].
    pub fn comment(&self, number: u64, text: &str) -> Change {
        match self.kind {
            ForgeKind::GitHub => github::comment(self, number, text),
            ForgeKind::GitLab => gitlab::comment(self, number, text),
        }
    }
    pub fn parse_comment(&self, body: &[u8]) -> Result<Comment, ForgeError> {
        #[derive(serde::Deserialize)]
        struct Posted {
            id: u64,
            html_url: Option<String>,
        }
        let posted: Posted = answer(body)?;
        Ok(Comment {
            id: posted.id,
            url: label_opt(posted.html_url),
        })
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

/// The non-empty segments of `url`'s path, percent-decoded; one that is not
/// UTF-8 is left empty, which [`Repository::new`] refuses.
fn decoded_segments(url: &Url) -> Vec<String> {
    url.path_segments()
        .into_iter()
        .flatten()
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            percent_encoding::percent_decode_str(segment)
                .decode_utf8()
                .map_or_else(|_| String::new(), |decoded| decoded.into_owned())
        })
        .collect()
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
