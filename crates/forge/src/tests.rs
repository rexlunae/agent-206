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
/// One page listed as the only answer.
fn list(repository: &Repository, listed: Listed, page: &Value) -> PullRequests {
    let mut listing = repository.listing(listed, None);
    listing.next(repository).unwrap();
    listing.add(repository, &bytes(page)).unwrap();
    listing.finish()
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
    // A GitLab served under a path prefix: the prefix is not the project's.
    let prefixed = api(ForgeKind::GitLab, "https://forge.example/gitlab/api/v4");
    let repository =
        Repository::new(&prefixed, &remote(&format!("/gitlab/{group}/{name}.git"))).unwrap();
    assert_eq!(repository.name(), format!("{group}/{name}"));
    assert_eq!(
        repository.pull_request_url(3).path(),
        format!("/gitlab/api/v4/projects/{group}%2F{name}/merge_requests/3")
    );
    // An escaped prefix matches the same prefix in the remote.
    let escaped = api(
        ForgeKind::GitLab,
        "https://forge.example/my%20gitlab/api/v4",
    );
    let repository = Repository::new(
        &escaped,
        &remote(&format!("/my%20gitlab/{group}/{name}.git")),
    )
    .unwrap();
    assert_eq!(repository.name(), format!("{group}/{name}"));
    // Only on the API's own host, and never the whole path.
    let elsewhere =
        Url::parse(&format!("https://{}.example/gitlab/{group}/{name}", word())).unwrap();
    let repository = Repository::new(&prefixed, &elsewhere).unwrap();
    assert_eq!(repository.name(), format!("gitlab/{group}/{name}"));
    let root = Repository::new(&gitlab, &remote(&format!("/{group}/{name}"))).unwrap();
    assert_eq!(root.name(), format!("{group}/{name}"));
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
    let url = github
        .listing(Listed::Merged, Some(&branch))
        .next(&github)
        .unwrap();
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
    let url = gitlab
        .listing(Listed::Open, Some(&branch))
        .next(&gitlab)
        .unwrap();
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
    let merged = list(&repository, Listed::Merged, &page);
    assert_eq!(
        merged
            .pull_requests
            .iter()
            .map(|p| p.number)
            .collect::<Vec<_>>(),
        [1]
    );
    assert!(merged.complete);
    let closed = list(&repository, Listed::Closed, &page);
    assert_eq!(closed.pull_requests[0].number, 2);
    let all = list(&repository, Listed::All, &page);
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
    let listed = list(&repository, Listed::Open, &Value::Array(full));
    assert!(!listed.complete);
}

/// Merged pull requests can be scarce among closed ones: further pages are
/// read until a page of them is found, the forge runs out, or the page
/// limit is reached, and only then is the listing complete.
#[test]
fn github_reads_more_closed_pages_to_find_merged_ones() {
    let repository = Repository::new(
        &api(ForgeKind::GitHub, "https://api.forge.example"),
        &remote(&format!("/{}/{}", word(), word())),
    )
    .unwrap();
    let page = |first: u64, merged: usize, len: usize| {
        Value::Array(
            (0..len)
                .map(|n| github_pull(first + n as u64, "closed", n < merged))
                .collect(),
        )
    };
    // Two merged per full page, then a short last page: all of them.
    let mut listing = repository.listing(Listed::Merged, None);
    let mut asked = vec![];
    let answers = [page(100, 2, PAGE), page(200, 2, PAGE), page(300, 1, 3)];
    for answer in &answers {
        let url = listing.next(&repository).unwrap();
        asked.push(query(&url));
        listing.add(&repository, &bytes(answer)).unwrap();
    }
    assert_eq!(listing.next(&repository), None);
    assert_eq!(asked[0].iter().find(|(k, _)| k == "page"), None);
    assert_eq!(asked[2].last().unwrap(), &("page".into(), "3".into()));
    let listed = listing.finish();
    let numbers: Vec<_> = listed.pull_requests.iter().map(|p| p.number).collect();
    assert_eq!(numbers, [100, 101, 200, 201, 300]);
    assert!(listed.complete);

    // Full pages up to the limit may hide older ones.
    let mut listing = repository.listing(Listed::Merged, None);
    let mut pages = 0;
    while listing.next(&repository).is_some() {
        listing
            .add(&repository, &bytes(&page(pages * 100, 1, PAGE)))
            .unwrap();
        pages += 1;
    }
    assert_eq!(pages, MAX_LIST_PAGES as u64);
    let listed = listing.finish();
    assert_eq!(listed.pull_requests.len(), MAX_LIST_PAGES);
    assert!(!listed.complete);

    // Once a page of them is found, no more is asked for.
    let mut listing = repository.listing(Listed::Closed, None);
    listing.next(&repository).unwrap();
    listing.add(&repository, &bytes(&page(0, 0, PAGE))).unwrap();
    assert_eq!(listing.next(&repository), None);
    assert!(!listing.finish().complete);
    // Open pull requests are one page.
    let mut listing = repository.listing(Listed::Open, None);
    listing.next(&repository).unwrap();
    listing.add(&repository, &bytes(&page(0, 0, PAGE))).unwrap();
    assert_eq!(listing.next(&repository), None);
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
        {"name": "stop", "status": "canceling"},
        {"name": "stopped", "status": "canceled"},
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
            (CheckStatus::Running, None),
            (CheckStatus::Completed, Some(Conclusion::Cancelled)),
            (CheckStatus::Unknown, None),
        ]
    );
    // Listed merge requests carry no pipeline, so no checks are read.
    let listed = list(&repository, Listed::All, &json!([request]));
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

#[test]
fn opening_and_commenting_send_each_forges_fields() {
    let (owner, name) = (word(), word());
    let github = Repository::new(
        &api(ForgeKind::GitHub, "https://forge.example/api/v3"),
        &remote(&format!("/{owner}/{name}")),
    )
    .unwrap();
    let gitlab = Repository::new(
        &api(ForgeKind::GitLab, "https://forge.example/api/v4"),
        &remote(&format!("/{owner}/{name}")),
    )
    .unwrap();
    let (title, text, source, target) = (word(), word(), word(), word());
    let new = NewPullRequest {
        title: &title,
        body: Some(&text),
        source: &source,
        target: &target,
        draft: true,
    };
    let sent = github.create_pull_request(&new).unwrap();
    assert_eq!(
        sent.url.path(),
        format!("/api/v3/repos/{owner}/{name}/pulls")
    );
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(
        body,
        json!({"title": title, "head": source, "base": target, "draft": true, "body": text})
    );
    let sent = gitlab.create_pull_request(&new).unwrap();
    assert_eq!(
        sent.url.path(),
        format!("/api/v4/projects/{owner}%2F{name}/merge_requests")
    );
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(
        body,
        json!({"title": format!("Draft: {title}"), "source_branch": source, "target_branch": target, "description": text})
    );
    // Without a description or draft flag, nothing extra is sent.
    let plain = NewPullRequest {
        body: None,
        draft: false,
        ..new
    };
    let body: Value =
        serde_json::from_slice(&gitlab.create_pull_request(&plain).unwrap().body).unwrap();
    assert_eq!(body["title"], title);
    assert!(body.get("description").is_none());

    // GitLab draft markers in the title never decide; `draft` does.
    for (marked, draft, posted) in [
        (format!("Draft: {title}"), false, title.clone()),
        (
            format!(" [DRAFT] (draft) {title}"),
            true,
            format!("Draft: {title}"),
        ),
        (
            format!("draftsman {title}"),
            false,
            format!("draftsman {title}"),
        ),
        // Unmarked titles go exactly as given.
        (format!("  {title}"), false, format!("  {title}")),
    ] {
        let new = NewPullRequest {
            title: &marked,
            draft,
            ..new
        };
        let body: Value =
            serde_json::from_slice(&gitlab.create_pull_request(&new).unwrap().body).unwrap();
        assert_eq!(body["title"], posted, "{marked}");
    }
    // A title that is only markers is refused rather than sent blank.
    for marked in ["Draft:", " [Draft] (draft) "] {
        let new = NewPullRequest {
            title: marked,
            draft: false,
            ..new
        };
        assert!(matches!(
            gitlab.create_pull_request(&new),
            Err(ForgeError::MarkerTitle)
        ));
    }
    // The longest title a draft may have reads back whole.
    let longest: String = std::iter::repeat_n('x', MAX_TITLE).collect();
    let new = NewPullRequest {
        title: &longest,
        draft: true,
        ..new
    };
    let mut request: Value =
        serde_json::from_slice(&gitlab.create_pull_request(&new).unwrap().body).unwrap();
    request["iid"] = json!(1);
    request["state"] = json!("opened");
    let read = gitlab.parse_pull_request(&bytes(&request)).unwrap();
    assert_eq!(read.title, format!("Draft: {longest}"));

    let sent = github.comment(5, &text);
    assert_eq!(
        sent.url.path(),
        format!("/api/v3/repos/{owner}/{name}/issues/5/comments")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&sent.body).unwrap(),
        json!({"body": text})
    );
    let sent = gitlab.comment(5, &text);
    assert!(sent.url.path().ends_with("/merge_requests/5/notes"));
    let url = format!("https://forge.example/{}", word());
    let posted = github
        .parse_comment(&bytes(&json!({"id": 77, "html_url": url})))
        .unwrap();
    assert_eq!(
        posted,
        Comment {
            id: 77,
            url: Some(url)
        }
    );
    let posted = gitlab
        .parse_comment(&bytes(&json!({"id": 78, "body": text})))
        .unwrap();
    assert_eq!(posted.url, None);
    assert!(matches!(
        github.parse_comment(br#"{"id": "x"}"#),
        Err(ForgeError::Answer(_))
    ));
}
