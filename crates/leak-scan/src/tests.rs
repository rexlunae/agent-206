//! Credential shapes are built at runtime, so no credential-shaped literal
//! is committed.
use super::*;

/// `n` random characters from `[0-9a-f]`.
fn random(n: usize) -> String {
    let mut text = String::new();
    while text.len() < n {
        text.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    text.truncate(n);
    text
}

fn one(kind: &str, secret: &str) {
    let text = format!("before {secret} after\n");
    let redacted = redact(&text).unwrap_or_else(|| panic!("{kind} not found in {text}"));
    assert_eq!(
        redacted.text,
        format!("before [redacted {kind}] after\n"),
        "{kind}"
    );
    assert_eq!(
        redacted.found,
        [Found {
            kind: kinds_name(kind),
            count: 1
        }]
    );
}

fn kinds_name(kind: &str) -> &'static str {
    kinds()
        .into_iter()
        .find(|name| *name == kind)
        .unwrap_or_else(|| panic!("unknown kind {kind}"))
}

#[test]
fn every_shipped_pattern_compiles() {
    assert_eq!(KINDS.len(), PATTERNS.len());
    assert!(kinds().windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn each_kind_is_removed_and_reported() {
    let up = |n| random(n).to_uppercase();
    let cases = [
        ("anthropic_api_key", format!("sk-ant-api03-{}", random(80))),
        ("openai_api_key", format!("sk-proj-{}", random(48))),
        ("openai_api_key", format!("sk-{}", random(40))),
        ("aws_access_key", format!("AKIA{}", up(16))),
        ("github_token", format!("ghp_{}", random(36))),
        (
            "github_token",
            format!("github_pat_{}_{}", random(22), random(59)),
        ),
        ("gitlab_token", format!("glpat-{}", random(20))),
        ("stripe_key", format!("sk_live_{}", random(24))),
        ("google_api_key", format!("AIza{}", random(35))),
        ("slack_token", format!("xoxb-{}-{}", random(12), random(24))),
        ("slack_token", format!("xapp-1-{}-{}", up(11), random(40))),
        (
            "sendgrid_api_key",
            format!("SG.{}.{}", random(22), random(43)),
        ),
        ("telegram_bot_token", format!("1234567890:AA{}", random(33))),
        (
            "discord_bot_token",
            format!("M{}.{}.{}", random(25), random(6), random(38)),
        ),
        (
            "jwt",
            format!("eyJ{}.eyJ{}.{}", random(20), random(30), random(43)),
        ),
    ];
    for (kind, secret) in &cases {
        one(kind, secret);
    }
}

#[test]
fn only_the_secret_part_of_a_match_is_removed() {
    let token = random(40);
    let header = format!("Authorization: Bearer {token}\n");
    let redacted = redact(&header).expect("bearer token found");
    assert_eq!(
        redacted.text,
        "Authorization: Bearer [redacted bearer_token]\n"
    );

    let url = format!("postgres://admin:{}@db.internal:5432/app", random(16));
    let redacted = redact(&url).expect("password found");
    assert_eq!(
        redacted.text,
        "postgres://admin:[redacted url_password]@db.internal:5432/app"
    );
}

#[test]
fn a_private_key_is_removed_whole_even_when_cut_off() {
    let body = random(64);
    let label = "PRIVATE KEY";
    let block = format!(
        "key:\n-----BEGIN OPENSSH {label}-----\n{body}\n-----END OPENSSH {label}-----\nnext\n"
    );
    let redacted = redact(&block).expect("key found");
    assert_eq!(redacted.text, "key:\n[redacted private_key]\nnext\n");
    assert!(!redacted.text.contains(&body));

    let truncated = format!("-----BEGIN RSA {label}-----\n{body}");
    assert_eq!(
        redact(&truncated).expect("key found").text,
        "[redacted private_key]"
    );
}

#[test]
fn repeated_and_mixed_credentials_are_counted_by_kind() {
    let (a, b) = (format!("ghp_{}", random(36)), format!("ghp_{}", random(36)));
    let aws = format!("AKIA{}", random(16).to_uppercase());
    let text = format!("{a}\n{aws} {b}\n{a}");
    let redacted = redact(&text).expect("found");
    assert_eq!(
        redacted.text,
        "[redacted github_token]\n[redacted aws_access_key] [redacted github_token]\n[redacted github_token]"
    );
    assert_eq!(
        redacted.found,
        [
            Found {
                kind: "aws_access_key",
                count: 1
            },
            Found {
                kind: "github_token",
                count: 3
            },
        ]
    );
}

#[test]
fn overlapping_shapes_are_removed_once() {
    // An Anthropic key is also shaped like an OpenAI one.
    let key = format!("sk-ant-api03-{}", random(80));
    let redacted = redact(&format!("x={key}")).expect("found");
    assert_eq!(redacted.text, "x=[redacted anthropic_api_key]");
    assert_eq!(redacted.found.len(), 1);
    // A bearer JWT is removed as one span.
    let jwt = format!("eyJ{}.eyJ{}.{}", random(20), random(30), random(43));
    let redacted = redact(&format!("Bearer {jwt}")).expect("found");
    assert_eq!(redacted.found.len(), 1, "{}", redacted.text);
    assert!(!redacted.text.contains(&jwt[3..]));
}

#[test]
fn ordinary_output_is_left_alone() {
    let text = format!(
        "commit {}\nsha256 {}{}\nask-the-user sk-short\nssh git@github.com:org/repo.git\n\
         https://example.com/path?q=1 user@example.com Bearer of bad news\n\
         -----BEGIN PUBLIC KEY-----\nAKIAshort ghp_short\n",
        random(40),
        random(32),
        random(32)
    );
    assert_eq!(redact(&text), None);
}

#[test]
fn redaction_keeps_utf8_text_around_a_secret() {
    let secret = format!("ghp_{}", random(36));
    let text = format!("ключ «{secret}» 🔑");
    assert_eq!(
        redact(&text).expect("found").text,
        "ключ «[redacted github_token]» 🔑"
    );
}
