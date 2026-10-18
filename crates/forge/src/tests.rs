use super::*;
use serde_json::{Value, json};

fn word() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
fn sha() -> String {
    format!("{}{}", word(), &word()[..8])
}
fn api(kind: ForgeKind, base: &str) -> ForgeApi {
    ForgeApi {
        kind,
        api: base.into(),
    }
}
fn remote(path: &str) -> Url {
    Url::parse(&format!("https://forge.example{path}")).unwrap()
}
fn bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}
fn query(url: &Url) -> Vec<(String, String)> {
    url.query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[test]
fn remotes_name_repositories_in_each_forges_shape() {
    let (owner, name) = (word(), word());
    let github = api(ForgeKind::GitHub, "https://api.forge.example");
    let repository = Repository::new(&github, &remote(&format!("/{owner}/{name}.git"))).unwrap();
    assert_eq!(repository.name(), format!("{owner}/{name}"));
    // GitHub repositories have exactly an owner and a name.
    for path in [
        format!("/{owner}"),
        format!("/{owner}/{name}/{}", word()),
        format!("/{owner}/.."),
    ] {
        assert!(matches!(
            Repository::new(&github, &remote(&path)),
            Err(ForgeError::RepositoryPath("GitHub"))
        ));
    }
    // GitLab groups nest.
    let gitlab = api(ForgeKind::GitLab, "https://forge.example/api/v4");
    let group = word();
    let nested = Repository::new(&gitlab, &remote(&format!("/{group}/{owner}/{name}"))).unwrap();
    assert_eq!(nested.name(), format!("{group}/{owner}/{name}"));
    let url = nested.pull_request_url(7);
    assert_eq!(
        url.path(),
        format!("/api/v4/projects/{group}%2F{owner}%2F{name}/merge_requests/7")
    );
    // An encoded slash cannot smuggle in another segment.
    assert!(Repository::new(&github, &remote(&format!("/{owner}/a%2F{name}"))).is_err());
}

#[test]
fn api_addresses_are_https_without_credentials() {
    for refused in [
        "http://forge.example",
        "https://user:pass@forge.example",
        "https://forge.example/?token=1",
        "https://forge.example/#x",
        "mailto:someone@forge.example",
        "not a url",
    ] {
        assert!(
            matches!(api_url(refused), Err(ForgeError::ApiUrl)),
            "{refused}"
        );
    }
    api_url("https://forge.example/api/v3").unwrap();
    api_url("http://127.0.0.1:8080/api").unwrap();
    assert_eq!(
        preset("https://github.com").unwrap().kind,
        ForgeKind::GitHub
    );
    assert_eq!(
        preset("https://gitlab.com").unwrap().kind,
        ForgeKind::GitLab
    );
    assert_eq!(preset(&format!("https://{}.example", word())), None);
}

#[test]
fn list_requests_name_the_state_and_source_branch() {
    let (owner, name, branch) = (word(), word(), format!("feature/{}", word()));
    let github = Repository::new(
        &api(ForgeKind::GitHub, "https://forge.example/api/v3/"),
        &remote(&format!("/{owner}/{name}")),
    )
    .unwrap();
    let url = github.list_url(Listed::Merged, Some(&branch));
    assert_eq!(url.path(), format!("/api/v3/repos/{owner}/{name}/pulls"));
    assert_eq!(
        query(&url),
        [
            ("state".into(), "closed".into()),
            ("per_page".into(), PAGE.to_string()),
            ("head".into(), format!("{owner}:{branch}")),
        ]
    );
    let gitlab = Repository::new(
        &api(ForgeKind::GitLab, "https://forge.example/api/v4"),
        &remote(&format!("/{owner}/{name}.git")),
    )
    .unwrap();
    let url = gitlab.list_url(Listed::Open, Some(&branch));
    assert_eq!(
        query(&url),
        [
            ("state".into(), "opened".into()),
            ("per_page".into(), PAGE.to_string()),
            ("source_branch".into(), branch),
        ]
    );
}

fn github_pull(number: u64, state: &str, merged: bool) -> Value {
    json!({
        "number": number,
        "title": format!("{}\u{1b}[31m", word()),
        "state": state,
        "draft": number.is_multiple_of(2),
        "merged_at": merged.then(word),
        "user": {"login": word()},
        "head": {"ref": word(), "sha": sha()},
        "base": {"ref": word(), "sha": sha()},
        "html_url": format!("https://forge.example/{}", word()),
        "body": word(),
        "created_at": word(),
        "updated_at": word(),
    })
}

#[test]
fn github_lists_tell_merged_from_closed() {
    let repository = Repository::new(
        &api(ForgeKind::GitHub, "https://api.forge.example"),
        &remote(&format!("/{}/{}", word(), word())),
    )
    .unwrap();
    let page = json!([
        github_pull(1, "closed", true),
        github_pull(2, "closed", false),
        github_pull(3, "open", false),
    ]);
    let merged = repository
        .parse_list(&bytes(&page), Listed::Merged)
        .unwrap();
    assert_eq!(
        merged
            .pull_requests
            .iter()
            .map(|p| p.number)
            .collect::<Vec<_>>(),
        [1]
    );
    assert!(merged.complete);
    let closed = repository
        .parse_list(&bytes(&page), Listed::Closed)
        .unwrap();
    assert_eq!(closed.pull_requests[0].number, 2);
    let all = repository.parse_list(&bytes(&page), Listed::All).unwrap();
    let states: Vec<_> = all.pull_requests.iter().map(|p| p.state).collect();
    assert_eq!(states, [State::Merged, State::Closed, State::Open]);
    // Escape characters are removed and lists carry no descriptions.
    let first = &all.pull_requests[0];
    assert!(!first.title.contains('\u{1b}'));
    assert_eq!(
        first.title,
        page[0]["title"].as_str().unwrap().replace('\u{1b}', "")
    );
    assert_eq!(first.source, page[0]["head"]["ref"]);
    assert_eq!(first.head.as_deref(), page[0]["head"]["sha"].as_str());
    assert_eq!(first.body, None);
    assert!(all.pull_requests[1].draft);

    // A full page may not be all of them.
    let full: Vec<Value> = (0..PAGE as u64)
        .map(|n| github_pull(n, "open", false))
        .collect();
    let listed = repository
        .parse_list(&bytes(&Value::Array(full)), Listed::Open)
        .unwrap();
    assert!(!listed.complete);
}

#[test]
fn one_pull_request_carries_its_shortened_description() {
    let repository = Repository::new(
        &api(ForgeKind::GitHub, "https://api.forge.example"),
        &remote(&format!("/{}/{}", word(), word())),
    )
    .unwrap();
    let mut pull = github_pull(4, "open", false);
    let long: String = std::iter::repeat_with(word).take(MAX_BODY / 16).collect();
    pull["body"] = json!(long);
    let read = repository.parse_pull_request(&bytes(&pull)).unwrap();
    let body = read.body.unwrap();
    assert!(read.body_shortened);
    assert_eq!(body.chars().count(), MAX_BODY);
    assert!(long.starts_with(&body));
    pull["body"] = json!(word());
    let read = repository.parse_pull_request(&bytes(&pull)).unwrap();
    assert!(!read.body_shortened);
    assert_eq!(read.body.as_deref(), pull["body"].as_str());
}

#[test]
fn github_checks_combine_runs_and_statuses() {
    let repository = Repository::new(
        &api(ForgeKind::GitHub, "https://api.forge.example"),
        &remote(&format!("/{}/{}", word(), word())),
    )
    .unwrap();
    let pull = repository
        .parse_pull_request(&bytes(&github_pull(5, "open", false)))
        .unwrap();
    let head = pull.head.clone().unwrap();
    let calls = repository.checks_calls(&pull).unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[0]
            .url
            .path()
            .ends_with(&format!("/commits/{head}/check-runs"))
    );
    assert!(
        calls[1]
            .url
            .path()
            .ends_with(&format!("/commits/{head}/status"))
    );
    let unheard = word();
    let runs = json!({"total_count": 4, "check_runs": [
        {"name": "a", "status": "completed", "conclusion": "success", "html_url": null},
        {"name": "b", "status": "completed", "conclusion": "timed_out"},
        {"name": "c", "status": "in_progress", "conclusion": null},
        {"name": "d", "status": "completed", "conclusion": unheard},
    ]});
    let checks = repository.parse_checks(&calls[0], &bytes(&runs)).unwrap();
    let seen: Vec<_> = checks.iter().map(|c| (c.status, c.conclusion)).collect();
    assert_eq!(
        seen,
        [
            (CheckStatus::Completed, Some(Conclusion::Success)),
            (CheckStatus::Completed, Some(Conclusion::TimedOut)),
            (CheckStatus::Running, None),
            (CheckStatus::Completed, Some(Conclusion::Unknown)),
        ]
    );
    assert_eq!(checks[3].reported, unheard);
    let statuses = json!({"state": "pending", "statuses": [
        {"context": "ci/x", "state": "error", "target_url": "https://ci.example/1"},
        {"context": "ci/y", "state": "pending"},
    ]});
    let checks = repository
        .parse_checks(&calls[1], &bytes(&statuses))
        .unwrap();
    assert_eq!(checks[0].conclusion, Some(Conclusion::Failure));
    assert_eq!(checks[0].reported, "error");
    assert_eq!(checks[1].status, CheckStatus::Running);
    // A malformed answer is an error, not an empty list.
    assert!(matches!(
        repository.parse_checks(&calls[0], br#"{"checks": []}"#),
        Err(ForgeError::Answer(_))
    ));

    let mut headless = pull.clone();
    headless.head = None;
    assert!(matches!(
        repository.checks_calls(&headless),
        Err(ForgeError::NoCommit)
    ));
}

#[test]
fn github_reviews_keep_their_states() {
    let repository = Repository::new(
        &api(ForgeKind::GitHub, "https://api.forge.example"),
        &remote(&format!("/{}/{}", word(), word())),
    )
    .unwrap();
    assert!(
        repository
            .reviews_url(9)
            .path()
            .ends_with("/pulls/9/reviews")
    );
    let reviewer = word();
    let reviews = json!([
        {"user": {"login": reviewer}, "state": "APPROVED", "submitted_at": word(), "commit_id": sha()},
        {"user": null, "state": "CHANGES_REQUESTED"},
        {"user": {"login": word()}, "state": "SOMETHING_NEW"},
    ]);
    let read = repository.parse_reviews(&bytes(&reviews)).unwrap();
    let states: Vec<_> = read.iter().map(|r| r.state).collect();
    assert_eq!(
        states,
        [
            ReviewState::Approved,
            ReviewState::ChangesRequested,
            ReviewState::Unknown
        ]
    );
    assert_eq!(read[0].author.as_deref(), Some(reviewer.as_str()));
    assert_eq!(read[1].author, None);
    assert_eq!(read[2].reported, "SOMETHING_NEW");
}

#[test]
fn gitlab_merge_requests_checks_and_approvals() {
    let repository = Repository::new(
        &api(ForgeKind::GitLab, "https://forge.example/api/v4"),
        &remote(&format!("/{}/{}", word(), word())),
    )
    .unwrap();
    let pipeline = 4_000 + u64::from(word().as_bytes()[0]);
    let request = json!({
        "iid": 12,
        "title": word(),
        "state": "merged",
        "work_in_progress": true,
        "author": {"username": word()},
        "source_branch": word(),
        "target_branch": word(),
        "sha": sha(),
        "web_url": word(),
        "description": word(),
        "head_pipeline": {"id": pipeline, "status": "failed"},
    });
    let read = repository.parse_pull_request(&bytes(&request)).unwrap();
    assert_eq!(read.number, 12);
    assert_eq!(read.state, State::Merged);
    assert!(read.draft);
    assert_eq!(read.body.as_deref(), request["description"].as_str());
    let calls = repository.checks_calls(&read).unwrap();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0]
            .url
            .path()
            .ends_with(&format!("/pipelines/{pipeline}/jobs"))
    );
    let jobs = json!([
        {"name": "build", "status": "success"},
        {"name": "lint", "status": "failed", "allow_failure": true},
        {"name": "test", "status": "failed"},
        {"name": "deploy", "status": "manual"},
        {"name": "other", "status": word()},
    ]);
    let checks = repository.parse_checks(&calls[0], &bytes(&jobs)).unwrap();
    let seen: Vec<_> = checks.iter().map(|c| (c.status, c.conclusion)).collect();
    assert_eq!(
        seen,
        [
            (CheckStatus::Completed, Some(Conclusion::Success)),
            (CheckStatus::Completed, Some(Conclusion::Neutral)),
            (CheckStatus::Completed, Some(Conclusion::Failure)),
            (CheckStatus::Queued, None),
            (CheckStatus::Unknown, None),
        ]
    );
    // Listed merge requests carry no pipeline, so no checks are read.
    let listed = repository
        .parse_list(&bytes(&json!([request])), Listed::All)
        .unwrap();
    assert_eq!(listed.pull_requests[0].body, None);
    let mut without = read.clone();
    without.pipeline = None;
    assert!(repository.checks_calls(&without).unwrap().is_empty());

    let approver = word();
    let approvals = json!({"approved": true, "approved_by": [{"user": {"username": approver}}]});
    let reviews = repository.parse_reviews(&bytes(&approvals)).unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].state, ReviewState::Approved);
    assert_eq!(reviews[0].author.as_deref(), Some(approver.as_str()));
    assert!(matches!(
        repository.parse_reviews(br#"{"approved_by": 3}"#),
        Err(ForgeError::Answer(_))
    ));
}
