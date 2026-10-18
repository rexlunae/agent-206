//! GitLab's REST API, version 4. Merge requests are pull requests here, the
//! jobs of a merge request's head pipeline are its checks, and approvals
//! are its reviews.
use super::{
    Change, Check, CheckStatus, Conclusion, DETAIL_PAGE, ForgeError, Listed, MAX_BODY,
    NewPullRequest, PAGE, PullRequest, Repository, Review, ReviewState, State, answer, bounded,
    label, label_opt,
};
use serde::Deserialize;
use serde_json::json;
use url::Url;

#[derive(Deserialize)]
struct MergeRequest {
    iid: u64,
    title: String,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    work_in_progress: bool,
    author: Option<User>,
    source_branch: String,
    target_branch: String,
    sha: Option<String>,
    web_url: Option<String>,
    description: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    head_pipeline: Option<Pipeline>,
}
#[derive(Deserialize)]
struct User {
    username: String,
}
#[derive(Deserialize)]
struct Pipeline {
    id: u64,
}

/// `rest` under the project, whose whole path is one encoded segment.
fn project(repository: &Repository, rest: &[&str]) -> Url {
    let id = repository.path.join("/");
    let segments = ["projects", id.as_str()]
        .into_iter()
        .chain(rest.iter().copied());
    repository.endpoint(segments)
}

pub(super) fn list_url(repository: &Repository, listed: Listed, source: Option<&str>) -> Url {
    let mut url = project(repository, &["merge_requests"]);
    let state = match listed {
        Listed::Open => "opened",
        Listed::Closed => "closed",
        Listed::Merged => "merged",
        Listed::All => "all",
    };
    let mut query = url.query_pairs_mut();
    query
        .append_pair("state", state)
        .append_pair("per_page", &PAGE.to_string());
    if let Some(branch) = source {
        query.append_pair("source_branch", branch);
    }
    drop(query);
    url
}

pub(super) fn parse_list(body: &[u8]) -> Result<(Vec<PullRequest>, usize), ForgeError> {
    let requests: Vec<MergeRequest> = answer(body)?;
    let read = requests.len();
    Ok((
        requests
            .into_iter()
            .map(|request| convert(request, false))
            .collect(),
        read,
    ))
}

pub(super) fn create(repository: &Repository, new: &NewPullRequest<'_>) -> Change {
    // GitLab marks a merge request as a draft by its title.
    let title = if new.draft {
        format!("Draft: {}", new.title)
    } else {
        new.title.to_owned()
    };
    let mut body = json!({
        "title": title,
        "source_branch": new.source,
        "target_branch": new.target,
    });
    if let Some(text) = new.body {
        body["description"] = json!(text);
    }
    Change {
        url: project(repository, &["merge_requests"]),
        body: body.to_string().into_bytes(),
    }
}

pub(super) fn comment(repository: &Repository, number: u64, text: &str) -> Change {
    Change {
        url: project(
            repository,
            &["merge_requests", &number.to_string(), "notes"],
        ),
        body: json!({"body": text}).to_string().into_bytes(),
    }
}

pub(super) fn pull_url(repository: &Repository, number: u64) -> Url {
    project(repository, &["merge_requests", &number.to_string()])
}

pub(super) fn parse_pull(body: &[u8]) -> Result<PullRequest, ForgeError> {
    Ok(convert(answer(body)?, true))
}

fn convert(request: MergeRequest, with_body: bool) -> PullRequest {
    let state = match request.state.as_str() {
        "opened" | "locked" => State::Open,
        "closed" => State::Closed,
        "merged" => State::Merged,
        _ => State::Unknown,
    };
    let (body, body_shortened) = match request.description.filter(|_| with_body) {
        Some(text) => {
            let (text, cut) = bounded(&text, MAX_BODY);
            (Some(text), cut)
        }
        None => (None, false),
    };
    PullRequest {
        number: request.iid,
        title: label(&request.title),
        state,
        draft: request.draft || request.work_in_progress,
        author: request.author.map(|user| label(&user.username)),
        source: label(&request.source_branch),
        target: label(&request.target_branch),
        head: label_opt(request.sha),
        url: label_opt(request.web_url),
        body,
        body_shortened,
        created: label_opt(request.created_at),
        updated: label_opt(request.updated_at),
        pipeline: request.head_pipeline.map(|pipeline| pipeline.id),
    }
}

pub(super) fn jobs_url(repository: &Repository, pipeline: u64) -> Url {
    let mut url = project(repository, &["pipelines", &pipeline.to_string(), "jobs"]);
    url.query_pairs_mut()
        .append_pair("per_page", &DETAIL_PAGE.to_string());
    url
}

#[derive(Deserialize)]
struct Job {
    name: String,
    status: String,
    #[serde(default)]
    allow_failure: bool,
    web_url: Option<String>,
}

pub(super) fn parse_jobs(body: &[u8]) -> Result<Vec<Check>, ForgeError> {
    let jobs: Vec<Job> = answer(body)?;
    Ok(jobs
        .into_iter()
        .map(|job| {
            let (status, conclusion) = match job.status.as_str() {
                "created" | "pending" | "waiting_for_resource" | "preparing" | "scheduled" => {
                    (CheckStatus::Queued, None)
                }
                // Waits for someone to start it.
                "manual" => (CheckStatus::Queued, None),
                "running" => (CheckStatus::Running, None),
                "success" => (CheckStatus::Completed, Some(Conclusion::Success)),
                "failed" if job.allow_failure => {
                    (CheckStatus::Completed, Some(Conclusion::Neutral))
                }
                "failed" => (CheckStatus::Completed, Some(Conclusion::Failure)),
                // Still stopping, so not yet finished.
                "canceling" => (CheckStatus::Running, None),
                "canceled" => (CheckStatus::Completed, Some(Conclusion::Cancelled)),
                "skipped" => (CheckStatus::Completed, Some(Conclusion::Skipped)),
                _ => (CheckStatus::Unknown, None),
            };
            Check {
                name: label(&job.name),
                status,
                conclusion,
                reported: label(&job.status),
                url: label_opt(job.web_url),
            }
        })
        .collect())
}

pub(super) fn approvals_url(repository: &Repository, number: u64) -> Url {
    project(
        repository,
        &["merge_requests", &number.to_string(), "approvals"],
    )
}

#[derive(Deserialize)]
struct Approvals {
    #[serde(default)]
    approved_by: Vec<Approval>,
}
#[derive(Deserialize)]
struct Approval {
    user: Option<User>,
}

/// GitLab reports who approved, not comments or requested changes.
pub(super) fn parse_approvals(body: &[u8]) -> Result<Vec<Review>, ForgeError> {
    let approvals: Approvals = answer(body)?;
    Ok(approvals
        .approved_by
        .into_iter()
        .map(|approval| Review {
            author: approval.user.map(|user| label(&user.username)),
            state: ReviewState::Approved,
            reported: "approved".into(),
            submitted: None,
            commit: None,
        })
        .collect())
}
