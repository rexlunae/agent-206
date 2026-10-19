//! Pull requests, their checks and their reviews, described the same way
//! whichever forge holds the repository. Each adapter turns a question into
//! the URL of its forge's REST API and the answer into these types; this
//! crate does no I/O, so the gateway sends every request through the vault
//! broker, which attaches the token.
//!
//! Forges word things differently (a GitLab merge request is a pull request
//! here, a pipeline job is a check). What a forge reports that has no
//! counterpart is `Unknown`, with the forge's own word kept in `reported`.
mod gitea;
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
/// Pages of checks or reviews asked for at most, for a forge whose pages
/// may hold fewer than asked.
pub const MAX_DETAIL_PAGES: usize = 4;
/// Longest title, name or label kept, in characters.
pub const MAX_LABEL: usize = 256;
/// Longest description kept, in characters.
pub const MAX_BODY: usize = 8_000;
/// Longest description or comment sent, in characters. A tool call's
/// arguments are bounded in bytes too (64 KiB as the model encodes them),
/// so this leaves room for escaping.
pub const MAX_TEXT: usize = 30_000;
/// Longest pull request title sent, in characters: what is kept of a title
/// read back, less the longest draft marker an adapter adds (GitLab's).
pub const MAX_TITLE: usize = MAX_LABEL - gitlab::DRAFT.len();
const _: () = assert!(gitea::DRAFT.len() <= gitlab::DRAFT.len());

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
    #[error("the title is only a draft marker; give it a title and set `draft` instead")]
    MarkerTitle,
    #[error(
        "GitLab merges as the project is set to (a merge commit or fast-forward), or squashed; it cannot rebase here"
    )]
    GitLabRebase,
    #[error("the pull request's head is no longer the commit `head`; read it again")]
    HeadMoved,
    #[error("the forge has not worked out the pull request's changes yet; read it again shortly")]
    NoDiff,
}

/// The bundled API location of a well-known forge, by the origin its git
/// remotes use.
pub fn preset(origin: &str) -> Option<ForgeApi> {
    let (kind, api) = match origin {
        "https://github.com" => (ForgeKind::GitHub, "https://api.github.com"),
        "https://gitlab.com" => (ForgeKind::GitLab, "https://gitlab.com/api/v4"),
        "https://codeberg.org" => (ForgeKind::Gitea, "https://codeberg.org/api/v1"),
        "https://gitea.com" => (ForgeKind::Gitea, "https://gitea.com/api/v1"),
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
    /// `None` when the forge's answer does not say (older Gitea).
    pub draft: Option<bool>,
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
            ForgeKind::Gitea => gitea::parse_list(body, self.listed, self.source.as_deref())?,
        };
        self.read += 1;
        self.more = match repository.kind {
            ForgeKind::GitHub | ForgeKind::GitLab => read >= PAGE,
            ForgeKind::Gitea => read > 0,
        };
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

/// Which version of a file a line comment is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Side {
    /// The file at the pull request's head: an added or unchanged line.
    #[default]
    New,
    /// The file at its base: a removed line.
    Old,
}

/// A comment on one line of a pull request's changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineComment<'a> {
    /// The file, relative to the repository's root.
    pub path: &'a str,
    /// The line's number in that version of the file, from 1.
    pub line: u32,
    pub side: Side,
    pub text: &'a str,
}

/// A request that changes something on the forge: how and where to send
/// it, and its JSON body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub method: ChangeMethod,
    pub url: Url,
    pub body: Vec<u8>,
}
impl Change {
    fn post(url: Url, body: serde_json::Value) -> Self {
        Self {
            method: ChangeMethod::Post,
            url,
            body: body.to_string().into_bytes(),
        }
    }
    fn put(url: Url, body: serde_json::Value) -> Self {
        Self {
            method: ChangeMethod::Put,
            ..Self::post(url, body)
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeMethod {
    Post,
    Put,
}

/// How a pull request's commits reach its target branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergeMethod {
    /// A merge commit (or as the GitLab project is set to merge).
    #[default]
    Merge,
    /// One new commit with all its changes.
    Squash,
    /// Its commits replayed onto the target branch.
    Rebase,
}

/// A pull request merge, as the forge reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Merged {
    /// `false` when the forge accepted the request but has not merged yet.
    pub merged: bool,
    /// The commit the merge made, where the forge says.
    pub commit: Option<String>,
}

/// Checks or reviews from one page of an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// The next page may hold more.
    pub more: bool,
}
impl<T> Page<T> {
    /// A page of at most [`DETAIL_PAGE`]: only a full one may not be the
    /// last.
    fn full(items: Vec<T>) -> Self {
        let more = items.len() >= DETAIL_PAGE;
        Self { items, more }
    }
    /// A page that may hold fewer than asked: only an empty one is the last.
    fn capped(items: Vec<T>) -> Self {
        let more = !items.is_empty();
        Self { items, more }
    }
}

/// What one request for checks returns, and how to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksCall {
    url: Url,
    source: ChecksSource,
}
impl ChecksCall {
    /// Page `page` (from 1) of these checks.
    pub fn url(&self, page: usize) -> Url {
        paged(self.url.clone(), page)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChecksSource {
    /// GitHub check runs.
    Runs,
    /// GitHub commit statuses.
    Statuses,
    /// GitLab pipeline jobs.
    Jobs,
    /// Gitea commit statuses.
    GiteaStatuses,
}

/// A repository on a forge, named by its git remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    kind: ForgeKind,
    api: Url,
    /// `owner/name` on GitHub and Gitea, `group/…/project` on GitLab.
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
            ForgeKind::GitHub | ForgeKind::Gitea => path.len() == 2,
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
        // one page may hold few of those asked for. Gitea does too, cannot
        // ask for one branch, and may send fewer than asked, so only a
        // further, empty page shows the list is complete.
        let pages = match (self.kind, listed) {
            (ForgeKind::GitHub, Listed::Closed | Listed::Merged) | (ForgeKind::Gitea, _) => {
                MAX_LIST_PAGES
            }
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
        let url = match self.kind {
            ForgeKind::GitHub => github::list_url(self, listed, source),
            ForgeKind::GitLab => gitlab::list_url(self, listed, source),
            ForgeKind::Gitea => gitea::list_url(self, listed),
        };
        paged(url, page)
    }

    pub fn pull_request_url(&self, number: u64) -> Url {
        match self.kind {
            ForgeKind::GitHub => github::pull_url(self, number),
            ForgeKind::GitLab => gitlab::pull_url(self, number),
            ForgeKind::Gitea => gitea::pull_url(self, number),
        }
    }
    pub fn parse_pull_request(&self, body: &[u8]) -> Result<PullRequest, ForgeError> {
        match self.kind {
            ForgeKind::GitHub => github::parse_pull(body),
            ForgeKind::GitLab => gitlab::parse_pull(body),
            ForgeKind::Gitea => gitea::parse_pull(body),
        }
    }

    /// The requests that read `pull`'s checks, each a page at a time with
    /// [`ChecksCall::url`]; none on GitLab when no pipeline ran for it.
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
            ForgeKind::Gitea => {
                let head = pull.head.as_deref().ok_or(ForgeError::NoCommit)?;
                Ok(vec![ChecksCall {
                    url: gitea::statuses_url(self, head),
                    source: ChecksSource::GiteaStatuses,
                }])
            }
        }
    }
    pub fn parse_checks(&self, call: &ChecksCall, body: &[u8]) -> Result<Page<Check>, ForgeError> {
        Ok(match call.source {
            ChecksSource::Runs => Page::full(github::parse_runs(body)?),
            ChecksSource::Statuses => Page::full(github::parse_statuses(body)?),
            ChecksSource::Jobs => Page::full(gitlab::parse_jobs(body)?),
            ChecksSource::GiteaStatuses => Page::capped(gitea::parse_statuses(body)?),
        })
    }

    /// Page `page` (from 1) of pull request `number`'s reviews.
    pub fn reviews_url(&self, number: u64, page: usize) -> Url {
        let url = match self.kind {
            ForgeKind::GitHub => github::reviews_url(self, number),
            ForgeKind::GitLab => gitlab::approvals_url(self, number),
            ForgeKind::Gitea => gitea::reviews_url(self, number),
        };
        paged(url, page)
    }
    pub fn parse_reviews(&self, body: &[u8]) -> Result<Page<Review>, ForgeError> {
        Ok(match self.kind {
            ForgeKind::GitHub => Page::full(github::parse_reviews(body)?),
            ForgeKind::GitLab => Page::full(gitlab::parse_approvals(body)?),
            ForgeKind::Gitea => Page::capped(gitea::parse_reviews(body)?),
        })
    }

    /// Opens `new`; the answer is read with
    /// [`Repository::parse_pull_request`].
    pub fn create_pull_request(&self, new: &NewPullRequest<'_>) -> Result<Change, ForgeError> {
        match self.kind {
            ForgeKind::GitHub => Ok(github::create(self, new)),
            ForgeKind::GitLab => gitlab::create(self, new),
            ForgeKind::Gitea => gitea::create(self, new),
        }
    }

    /// Merges pull request `number` by `method` if its head is still commit
    /// `head`; the answer is read with [`Repository::parse_merge`].
    pub fn merge(
        &self,
        number: u64,
        head: &str,
        method: MergeMethod,
    ) -> Result<Change, ForgeError> {
        match self.kind {
            ForgeKind::GitHub => Ok(github::merge(self, number, head, method)),
            ForgeKind::GitLab => gitlab::merge(self, number, head, method),
            ForgeKind::Gitea => Ok(gitea::merge(self, number, head, method)),
        }
    }
    pub fn parse_merge(&self, body: &[u8]) -> Result<Merged, ForgeError> {
        match self.kind {
            ForgeKind::GitHub => github::parse_merge(body),
            ForgeKind::GitLab => gitlab::parse_merge(body),
            // Gitea answers a merge with an empty success.
            ForgeKind::Gitea => Ok(Merged {
                merged: true,
                commit: None,
            }),
        }
    }

    /// Posts `text` as a comment on pull request `number`; the answer is
    /// read with [`Repository::parse_comment`].
    pub fn comment(&self, number: u64, text: &str) -> Change {
        match self.kind {
            // Gitea's issue comments are shaped like GitHub's.
            ForgeKind::GitHub | ForgeKind::Gitea => github::comment(self, number, text),
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

    /// Posts `comment` on a line of pull request `number`, whose current
    /// answer to [`Repository::pull_request_url`] is `current`, anchored at
    /// commit `head`; refused if that is no longer its head. The answer is
    /// read with [`Repository::parse_line_comment`].
    pub fn line_comment(
        &self,
        number: u64,
        current: &[u8],
        head: &str,
        comment: &LineComment<'_>,
    ) -> Result<Change, ForgeError> {
        let pull = self.parse_pull_request(current)?;
        let head = pull
            .head
            .filter(|now| now.eq_ignore_ascii_case(head))
            .ok_or(ForgeError::HeadMoved)?;
        match self.kind {
            ForgeKind::GitHub => Ok(github::line_comment(self, number, &head, comment)),
            ForgeKind::GitLab => gitlab::line_comment(self, number, current, &head, comment),
            ForgeKind::Gitea => Ok(gitea::line_comment(self, number, &head, comment)),
        }
    }
    pub fn parse_line_comment(&self, body: &[u8]) -> Result<Comment, ForgeError> {
        match self.kind {
            // Gitea answers with the review that holds the comment.
            ForgeKind::GitHub | ForgeKind::Gitea => self.parse_comment(body),
            ForgeKind::GitLab => gitlab::parse_discussion(body),
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

/// `url` asking for page `page` (from 1); the first is asked for without
/// naming it.
fn paged(mut url: Url, page: usize) -> Url {
    if page > 1 {
        url.query_pairs_mut().append_pair("page", &page.to_string());
    }
    url
}

/// `title` without the draft `markers` (lowercase) it starts with, so only
/// a pull request's `draft` flag decides whether it is one. A title without
/// one is kept exactly; with one, the spaces around the markers go too.
fn unmarked<'a>(title: &'a str, markers: &[&str]) -> &'a str {
    let mut rest = title;
    let mut marked = false;
    loop {
        let trimmed = rest.trim_start();
        let marker = markers.iter().find(|marker| {
            trimmed
                .get(..marker.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(marker))
        });
        match marker {
            Some(marker) => {
                rest = &trimmed[marker.len()..];
                marked = true;
            }
            None if marked => return trimmed,
            None => return title,
        }
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
