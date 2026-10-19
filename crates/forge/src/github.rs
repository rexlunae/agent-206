//! GitHub's REST API (github.com, or GitHub Enterprise Server's `/api/v3`).
use super::{
    Change, Check, CheckStatus, Conclusion, DETAIL_PAGE, ForgeError, Listed, MAX_BODY,
    NewPullRequest, PAGE, PullRequest, Repository, Review, ReviewState, State, answer, bounded,
    label, label_opt,
};
use serde::Deserialize;
use serde_json::json;
use url::Url;

#[derive(Deserialize)]
struct Pull {
    number: u64,
    title: String,
    state: String,
    #[serde(default)]
    draft: bool,
    merged_at: Option<String>,
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
}

/// `rest` under the repository, as GitHub and Gitea both lay it out.
pub(super) fn repo<'a>(repository: &'a Repository, rest: &[&'a str]) -> Url {
    let segments = ["repos"]
        .into_iter()
        .chain(repository.path.iter().map(String::as_str))
        .chain(rest.iter().copied());
    repository.endpoint(segments)
}

pub(super) fn list_url(repository: &Repository, listed: Listed, source: Option<&str>) -> Url {
    let mut url = repo(repository, &["pulls"]);
    let state = match listed {
        Listed::Open => "open",
        Listed::Closed | Listed::Merged => "closed",
        Listed::All => "all",
    };
    let mut query = url.query_pairs_mut();
    query
        .append_pair("state", state)
        .append_pair("per_page", &PAGE.to_string());
    if let Some(branch) = source {
        // Branches in this repository: forks are named by their owner.
        query.append_pair("head", &format!("{}:{branch}", repository.path[0]));
    }
    drop(query);
    url
}

pub(super) fn parse_list(
    body: &[u8],
    listed: Listed,
) -> Result<(Vec<PullRequest>, usize), ForgeError> {
    let pulls: Vec<Pull> = answer(body)?;
    let read = pulls.len();
    let pulls = pulls
        .into_iter()
        .map(|pull| convert(pull, false))
        // GitHub lists merged and unmerged closed ones together.
        .filter(|pull| match listed {
            Listed::Closed => pull.state == State::Closed,
            Listed::Merged => pull.state == State::Merged,
            Listed::Open | Listed::All => true,
        })
        .collect();
    Ok((pulls, read))
}

pub(super) fn create(repository: &Repository, new: &NewPullRequest<'_>) -> Change {
    let mut body = json!({
        "title": new.title,
        "head": new.source,
        "base": new.target,
        "draft": new.draft,
    });
    if let Some(text) = new.body {
        body["body"] = json!(text);
    }
    Change {
        url: repo(repository, &["pulls"]),
        body: body.to_string().into_bytes(),
    }
}

/// Pull requests are issues for comments that are not on a line.
pub(super) fn comment(repository: &Repository, number: u64, text: &str) -> Change {
    Change {
        url: repo(repository, &["issues", &number.to_string(), "comments"]),
        body: json!({"body": text}).to_string().into_bytes(),
    }
}

pub(super) fn pull_url(repository: &Repository, number: u64) -> Url {
    repo(repository, &["pulls", &number.to_string()])
}

pub(super) fn parse_pull(body: &[u8]) -> Result<PullRequest, ForgeError> {
    Ok(convert(answer(body)?, true))
}

fn convert(pull: Pull, with_body: bool) -> PullRequest {
    let state = match (pull.state.as_str(), pull.merged_at.is_some()) {
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

pub(super) fn runs_url(repository: &Repository, commit: &str) -> Url {
    let mut url = repo(repository, &["commits", commit, "check-runs"]);
    url.query_pairs_mut()
        .append_pair("per_page", &DETAIL_PAGE.to_string());
    url
}

pub(super) fn statuses_url(repository: &Repository, commit: &str) -> Url {
    let mut url = repo(repository, &["commits", commit, "status"]);
    url.query_pairs_mut()
        .append_pair("per_page", &DETAIL_PAGE.to_string());
    url
}

#[derive(Deserialize)]
struct Runs {
    check_runs: Vec<Run>,
}
#[derive(Deserialize)]
struct Run {
    name: String,
    status: String,
    conclusion: Option<String>,
    html_url: Option<String>,
}

pub(super) fn parse_runs(body: &[u8]) -> Result<Vec<Check>, ForgeError> {
    let runs: Runs = answer(body)?;
    Ok(runs
        .check_runs
        .into_iter()
        .map(|run| {
            let status = match run.status.as_str() {
                "queued" | "requested" | "waiting" | "pending" => CheckStatus::Queued,
                "in_progress" => CheckStatus::Running,
                "completed" => CheckStatus::Completed,
                _ => CheckStatus::Unknown,
            };
            let conclusion = (status == CheckStatus::Completed).then_some({
                match run.conclusion.as_deref() {
                    Some("success") => Conclusion::Success,
                    Some("failure") => Conclusion::Failure,
                    Some("neutral") => Conclusion::Neutral,
                    Some("cancelled") => Conclusion::Cancelled,
                    Some("skipped") => Conclusion::Skipped,
                    Some("timed_out") => Conclusion::TimedOut,
                    Some("action_required") => Conclusion::ActionRequired,
                    Some("stale") => Conclusion::Stale,
                    _ => Conclusion::Unknown,
                }
            });
            Check {
                name: label(&run.name),
                status,
                conclusion,
                reported: label(run.conclusion.as_deref().unwrap_or(&run.status)),
                url: label_opt(run.html_url),
            }
        })
        .collect())
}

#[derive(Deserialize)]
struct Combined {
    statuses: Vec<Status>,
}
#[derive(Deserialize)]
struct Status {
    context: String,
    state: String,
    target_url: Option<String>,
}

/// Commit statuses, which older integrations report instead of check runs.
pub(super) fn parse_statuses(body: &[u8]) -> Result<Vec<Check>, ForgeError> {
    let combined: Combined = answer(body)?;
    Ok(combined
        .statuses
        .into_iter()
        .map(|status| {
            let (state, conclusion) = match status.state.as_str() {
                "pending" => (CheckStatus::Running, None),
                "success" => (CheckStatus::Completed, Some(Conclusion::Success)),
                "failure" | "error" => (CheckStatus::Completed, Some(Conclusion::Failure)),
                _ => (CheckStatus::Unknown, None),
            };
            Check {
                name: label(&status.context),
                status: state,
                conclusion,
                reported: label(&status.state),
                url: label_opt(status.target_url),
            }
        })
        .collect())
}

pub(super) fn reviews_url(repository: &Repository, number: u64) -> Url {
    let mut url = repo(repository, &["pulls", &number.to_string(), "reviews"]);
    url.query_pairs_mut()
        .append_pair("per_page", &DETAIL_PAGE.to_string());
    url
}

#[derive(Deserialize)]
struct GivenReview {
    user: Option<User>,
    state: String,
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
                "APPROVED" => ReviewState::Approved,
                "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
                "COMMENTED" => ReviewState::Commented,
                "DISMISSED" => ReviewState::Dismissed,
                "PENDING" => ReviewState::Pending,
                _ => ReviewState::Unknown,
            },
            reported: label(&review.state),
            submitted: label_opt(review.submitted_at),
            commit: label_opt(review.commit_id),
        })
        .collect())
}
