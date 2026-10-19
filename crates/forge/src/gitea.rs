//! Gitea's REST API, version 1, which Forgejo (and so Codeberg) also
//! speaks. Its repository paths, pull request numbers and issue comments
//! are shaped like GitHub's; checks are commit statuses, which Gitea and
//! Forgejo Actions report too.
//!
//! A Gitea caps each page at its own configured limit, which may be fewer
//! than asked, so only an empty page shows that nothing more is left.
use super::{
    Change, Check, CheckStatus, Conclusion, DETAIL_PAGE, ForgeError, Listed, MAX_BODY,
    NewPullRequest, PAGE, PullRequest, Repository, Review, ReviewState, State, answer, bounded,
    github::repo, label, label_opt, unmarked,
};
use serde::Deserialize;
use serde_json::json;
use url::Url;

#[derive(Deserialize)]
struct Pull {
    number: u64,
    title: String,
    state: String,
    /// Absent from older versions, which then report none.
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    merged: bool,
    user: Option<User>,
    head: Branch,
    base: Branch,
    html_url: Option<String>,
    body: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}
#[derive(Deserialize)]
struct User {
    login: String,
}
#[derive(Deserialize)]
struct Branch {
    #[serde(rename = "ref")]
    name: String,
    sha: Option<String>,
    /// The repository the branch is in; a deleted fork's is gone.
    repo_id: Option<i64>,
}

pub(super) fn list_url(repository: &Repository, listed: Listed) -> Url {
    let mut url = repo(repository, &["pulls"]);
    let state = match listed {
        Listed::Open => "open",
        Listed::Closed | Listed::Merged => "closed",
        Listed::All => "all",
    };
    url.query_pairs_mut()
        .append_pair("state", state)
        .append_pair("limit", &PAGE.to_string());
    url
}

/// The pull requests of a page that are `listed` and, if `source` is
/// given, from that branch of the repository itself (Gitea cannot ask for
/// one branch), and how many the page held.
pub(super) fn parse_list(
    body: &[u8],
    listed: Listed,
    source: Option<&str>,
) -> Result<(Vec<PullRequest>, usize), ForgeError> {
    let pulls: Vec<Pull> = answer(body)?;
    let read = pulls.len();
    let pulls = pulls
        .into_iter()
        .filter(|pull| {
            source.is_none_or(|branch| {
                pull.head.name == branch
                    && pull.head.repo_id.is_some()
                    && pull.head.repo_id == pull.base.repo_id
            })
        })
        .map(|pull| convert(pull, false))
        // Gitea lists merged and unmerged closed ones together.
        .filter(|pull| match listed {
            Listed::Closed => pull.state == State::Closed,
            Listed::Merged => pull.state == State::Merged,
            Listed::Open | Listed::All => true,
        })
        .collect();
    Ok((pulls, read))
}

/// The title prefix that makes a pull request a draft (work in progress)
/// with Gitea's default settings.
pub(super) const DRAFT: &str = "WIP: ";
/// Gitea's default work-in-progress prefixes, compared ignoring case. An
/// instance may configure others, which are then sent as written.
const MARKERS: [&str; 2] = ["wip:", "[wip]"];

pub(super) fn create(
    repository: &Repository,
    new: &NewPullRequest<'_>,
) -> Result<Change, ForgeError> {
    // Gitea marks a pull request as a draft by its title.
    let title = unmarked(new.title, &MARKERS);
    if title.trim().is_empty() {
        return Err(ForgeError::MarkerTitle);
    }
    let title = if new.draft {
        format!("{DRAFT}{title}")
    } else {
        title.to_owned()
    };
    let mut body = json!({
        "title": title,
        "head": new.source,
        "base": new.target,
    });
    if let Some(text) = new.body {
        body["body"] = json!(text);
    }
    Ok(Change {
        url: repo(repository, &["pulls"]),
        body: body.to_string().into_bytes(),
    })
}

pub(super) fn pull_url(repository: &Repository, number: u64) -> Url {
    repo(repository, &["pulls", &number.to_string()])
}

pub(super) fn parse_pull(body: &[u8]) -> Result<PullRequest, ForgeError> {
    Ok(convert(answer(body)?, true))
}

fn convert(pull: Pull, with_body: bool) -> PullRequest {
    let state = match (pull.state.as_str(), pull.merged) {
        ("open", _) => State::Open,
        ("closed", true) => State::Merged,
        ("closed", false) => State::Closed,
        _ => State::Unknown,
    };
    let (body, body_shortened) = match pull.body.filter(|_| with_body) {
        Some(text) => {
            let (text, cut) = bounded(&text, MAX_BODY);
            (Some(text), cut)
        }
        None => (None, false),
    };
    PullRequest {
        number: pull.number,
        title: label(&pull.title),
        state,
        draft: pull.draft,
        author: pull.user.map(|user| label(&user.login)),
        source: label(&pull.head.name),
        target: label(&pull.base.name),
        head: label_opt(pull.head.sha),
        url: label_opt(pull.html_url),
        body,
        body_shortened,
        created: label_opt(pull.created_at),
        updated: label_opt(pull.updated_at),
        pipeline: None,
    }
}

/// The latest status of each context on `commit`.
pub(super) fn statuses_url(repository: &Repository, commit: &str) -> Url {
    let mut url = repo(repository, &["commits", commit, "status"]);
    url.query_pairs_mut()
        .append_pair("limit", &DETAIL_PAGE.to_string());
    url
}

#[derive(Deserialize)]
struct Combined {
    statuses: Vec<Status>,
}
#[derive(Deserialize)]
struct Status {
    context: String,
    /// Named `status` here, where GitHub has `state`.
    status: String,
    target_url: Option<String>,
}

pub(super) fn parse_statuses(body: &[u8]) -> Result<Vec<Check>, ForgeError> {
    let combined: Combined = answer(body)?;
    Ok(combined
        .statuses
        .into_iter()
        .map(|status| {
            let (state, conclusion) = match status.status.as_str() {
                "pending" => (CheckStatus::Running, None),
                "success" => (CheckStatus::Completed, Some(Conclusion::Success)),
                "failure" | "error" => (CheckStatus::Completed, Some(Conclusion::Failure)),
                "warning" => (CheckStatus::Completed, Some(Conclusion::Neutral)),
                _ => (CheckStatus::Unknown, None),
            };
            Check {
                name: label(&status.context),
                status: state,
                conclusion,
                reported: label(&status.status),
                url: label_opt(status.target_url),
            }
        })
        .collect())
}

pub(super) fn reviews_url(repository: &Repository, number: u64) -> Url {
    let mut url = repo(repository, &["pulls", &number.to_string(), "reviews"]);
    url.query_pairs_mut()
        .append_pair("limit", &DETAIL_PAGE.to_string());
    url
}

#[derive(Deserialize)]
struct GivenReview {
    user: Option<User>,
    state: String,
    #[serde(default)]
    dismissed: bool,
    submitted_at: Option<String>,
    commit_id: Option<String>,
}

pub(super) fn parse_reviews(body: &[u8]) -> Result<Vec<Review>, ForgeError> {
    let reviews: Vec<GivenReview> = answer(body)?;
    Ok(reviews
        .into_iter()
        .map(|review| Review {
            author: review.user.map(|user| label(&user.login)),
            state: match review.state.as_str() {
                _ if review.dismissed => ReviewState::Dismissed,
                "APPROVED" => ReviewState::Approved,
                "REQUEST_CHANGES" => ReviewState::ChangesRequested,
                "COMMENT" => ReviewState::Commented,
                // Not yet given: unsubmitted, or asked of the reviewer.
                "PENDING" | "REQUEST_REVIEW" => ReviewState::Pending,
                _ => ReviewState::Unknown,
            },
            reported: label(&review.state),
            submitted: label_opt(review.submitted_at),
            commit: label_opt(review.commit_id),
        })
        .collect())
}
