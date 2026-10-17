use super::*;
use uuid::Uuid;

fn word() -> String {
    format!("w{}", Uuid::new_v4().simple())
}
fn run<'a>(command: &'a str, output: &'a str) -> Input<'a> {
    Input {
        tool: "execute_command",
        command: Some(command),
        output,
    }
}
fn user(files: &[&str]) -> Result<Rules, RulesError> {
    Rules::with_user_files(
        files
            .iter()
            .enumerate()
            .map(|(index, text)| (PathBuf::from(format!("{index}.toml")), text.to_string()))
            .collect(),
    )
}
fn shipped(id: &str) -> bool {
    Rules::builtin().rules().any(|rule| rule.id == id)
}

#[test]
fn the_shipped_rules_compile_with_distinct_ids() {
    let rules = Rules::builtin();
    let ids: Vec<_> = rules.rules().map(|rule| rule.id).collect();
    assert!(!ids.is_empty());
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len());
    assert!(rules.rules().all(|rule| rule.origin == Origin::Builtin));
}

#[test]
fn git_status_drops_hints_and_keeps_the_branch_and_paths() {
    let (branch, path) = (word(), word());
    let output = format!(
        "On branch {branch}\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n  (use \"git restore <file>...\" to discard changes in working directory)\n\tmodified:   {path}\n\n\n?? {branch}.txt\n"
    );
    let reduced = Rules::builtin()
        .reduce(&run("git -C somewhere status", &output))
        .unwrap();
    assert_eq!(reduced.rule, "git.status");
    assert_eq!(reduced.origin, Origin::Builtin);
    assert!(reduced.text.contains(&format!("On branch {branch}")));
    assert!(reduced.text.contains(&format!("modified:   {path}")));
    assert!(reduced.text.contains(&format!("?? {branch}.txt")));
    assert!(!reduced.text.contains("(use \"git"));
    assert!(!reduced.text.contains("\n\n\n"));
    assert!(reduced.counts.is_empty());
    assert!(
        reduced
            .text
            .ends_with("[output rule `git.status` kept 5 of 8 lines]")
    );
}

#[test]
fn cargo_run_output_reaches_the_model_as_printed() {
    let output = format!("test {} ... ok\n   Compiling x\n", word());
    assert_eq!(Rules::builtin().reduce(&run("cargo run", &output)), None);
}

#[test]
fn split_string_commands_are_not_matched_by_their_trailing_words() {
    let output = format!("test {} ... ok\n   Compiling x\n", word());
    assert_eq!(
        Rules::builtin().reduce(&run("env -S 'printf x' cargo test", &output)),
        None
    );
}

#[test]
fn cargo_output_keeps_failures_and_drops_progress() {
    let (pass, fail) = (word(), word());
    let output = format!(
        "\u{1b}[1m   Compiling\u{1b}[0m a v0.1.0\n    Checking b v0.1.0\ntest {pass} ... ok\ntest {fail} ... FAILED\nwarning: unused\nerror[E0425]: cannot find\ntest result: FAILED. 1 passed; 1 failed\n"
    );
    let reduced = Rules::builtin()
        .reduce(&run(
            "cd crate && RUST_LOG=x cargo test 2>&1 | tail -n 50",
            &output,
        ))
        .unwrap();
    assert_eq!(reduced.rule, "cargo");
    assert!(!reduced.text.contains("Compiling"));
    assert!(!reduced.text.contains(&pass));
    assert!(!reduced.text.contains('\u{1b}'));
    for kept in [
        &fail,
        "warning: unused",
        "error[E0425]",
        "test result: FAILED",
    ] {
        assert!(reduced.text.contains(kept), "{kept} missing");
    }
    let count = |name: &str| {
        reduced
            .counts
            .iter()
            .find(|c| c.name == name)
            .unwrap()
            .count
    };
    assert_eq!(
        (count("warnings"), count("errors"), count("failed tests")),
        (1, 1, 1)
    );
}

#[test]
fn programs_are_found_in_lists_pipelines_and_wrappers() {
    let program = |line: &str| super::program(line);
    assert_eq!(
        program("cd x && FOO=1 env cargo test 2>&1 | tail -n 5"),
        ["cargo", "test", "2>&1"]
    );
    assert_eq!(program("/usr/bin/git status"), ["git", "status"]);
    assert_eq!(
        program("make || 'my tool' \"a b\" c\\ d"),
        ["my tool", "a b", "c d"]
    );
    assert_eq!(program("(cd x; ls -la) "), ["ls", "-la"]);
    assert_eq!(program("time ls &> out"), ["ls", "&>", "out"]);
    assert_eq!(program("echo '&& git status'"), ["echo", "&& git status"]);
    assert!(program("  ").is_empty());
    // Wrapper options, and the values some of them take, are not the program.
    assert_eq!(program("env -i -u HOME A=1 cargo test"), ["cargo", "test"]);
    assert_eq!(program("nice -n 5 time -p git status"), ["git", "status"]);
    assert_eq!(program("env -- ls"), ["ls"]);
    assert!(program("env -u").is_empty());
    // `env -S` runs its operand, so no later word is taken for the program.
    for line in [
        "env -S 'printf ok' cargo test",
        "env -iS'printf ok' cargo test",
        "env --split-string='printf ok' cargo test",
        "env --split-string 'printf ok' cargo test",
    ] {
        assert!(program(line).is_empty(), "{line}");
    }
    let git = |line: &str| command::git_subcommand(&super::program(line)).map(str::to_owned);
    assert_eq!(
        git("git -C dir -c a=b --no-pager diff --stat").as_deref(),
        Some("diff")
    );
    assert_eq!(git("git --version"), None);
    assert_eq!(git("cargo status"), None);
}

#[test]
fn nothing_changes_without_a_matching_rule_or_a_reduction() {
    let rules = Rules::builtin();
    let output = format!("modified: {}\n  (use \"git add\")\n", word());
    // The shipped rules apply only to commands the command tool runs.
    assert_eq!(
        rules.reduce(&Input {
            tool: &word(),
            command: Some("git status"),
            output: &output
        }),
        None
    );
    assert_eq!(
        rules.reduce(&run(&format!("{} status", word()), &output)),
        None
    );
    // A matching rule that drops nothing leaves the output alone.
    let clean = format!("On branch {}\n", word());
    assert_eq!(rules.reduce(&run("git status", &clean)), None);
}

#[test]
fn long_output_keeps_its_start_and_end() {
    let rules = user(&[r#"
[[rule]]
id = "long"
[rule.summarize]
head = 2
tail = 1
"#])
    .unwrap();
    let lines: Vec<String> = (0..6).map(|_| word()).collect();
    let output = lines.join("\n");
    let reduced = rules
        .reduce(&Input {
            tool: &word(),
            command: None,
            output: &output,
        })
        .unwrap();
    assert_eq!(
        reduced.text,
        format!(
            "{}\n{}\n… [3 lines left out]\n{}\n[output rule `long` kept 3 of 6 lines]",
            lines[0], lines[1], lines[5]
        )
    );
    // Counters count only what the model is shown.
    let counted = user(&[&format!(
        "[[rule]]\nid = \"c\"\n[rule.summarize]\nhead = 1\ntail = 1\n[[rule.counters]]\nname = \"hits\"\npattern = '^{}'\n",
        lines[2]
    )])
    .unwrap()
    .reduce(&Input {
        tool: "x",
        command: None,
        output: &output,
    })
    .unwrap();
    assert_eq!(counted.counts[0].count, 0);
    assert!(
        counted.text.ends_with("kept 2 of 6 lines; hits: 0]"),
        "{}",
        counted.text
    );
    // Short output is untouched.
    let short = lines[..3].join("\n");
    assert_eq!(
        rules.reduce(&Input {
            tool: "x",
            command: None,
            output: &short
        }),
        None
    );
}

#[test]
fn user_rules_replace_disable_and_outrank_shipped_ones() {
    let marker = word();
    // Same id: the user's rule replaces the shipped one.
    let replaced = user(&[&format!(
        "[[rule]]\nid = \"git.status\"\n[rule.match]\ngit_subcommands = [\"status\"]\n[rule.filters]\nskip = ['{marker}']\n"
    )])
    .unwrap();
    let output = format!("{marker}\n  (use \"git add\")\n");
    let reduced = replaced.reduce(&run("git status", &output)).unwrap();
    assert_eq!(reduced.origin, Origin::User);
    assert!(!reduced.text.contains(&marker));
    assert!(reduced.text.contains("(use \"git add\")"));
    assert_eq!(
        replaced
            .rules()
            .filter(|rule| rule.id == "git.status")
            .count(),
        1
    );
    // Disabled: no rule applies.
    let disabled = user(&["[[rule]]\nid = \"git.status\"\nenabled = false\n"]).unwrap();
    assert!(shipped("git.status"));
    assert!(disabled.rules().all(|rule| rule.id != "git.status"));
    assert_eq!(disabled.reduce(&run("git status", &output)), None);
    // A user rule at the shipped rule's priority wins the tie.
    let tie = user(&[&format!(
        "[[rule]]\nid = \"{}\"\npriority = 100\n[rule.filters]\nskip = ['{marker}']\n",
        word()
    )])
    .unwrap();
    assert_eq!(
        tie.reduce(&run("git status", &output)).unwrap().origin,
        Origin::User
    );
}

#[test]
fn invalid_rule_files_are_refused_with_their_cause() {
    let id = word();
    let parse = user(&["[[rule]]\nid = \"x\"\nunknown = 1\n"]).unwrap_err();
    assert!(matches!(&parse, RulesError::Parse { path, .. } if path == Path::new("0.toml")));
    assert!(std::error::Error::source(&parse).is_some());
    let pattern = user(&[&format!(
        "[[rule]]\nid = \"{id}\"\n[rule.filters]\nskip = ['(']\n"
    )])
    .unwrap_err();
    assert!(
        matches!(&pattern, RulesError::Pattern { id: found, source: PatternError::Regex { .. }, .. } if *found == id)
    );
    let flag = user(&[&format!(
        "[[rule]]\nid = \"{id}\"\n[[rule.counters]]\nname = \"n\"\npattern = \"a\"\nflags = \"g\"\n"
    )])
    .unwrap_err();
    assert!(matches!(
        flag,
        RulesError::Pattern {
            source: PatternError::Flag('g'),
            ..
        }
    ));
    for (head, tail) in [(MAX_SUMMARY + 1, 0), (0, usize::MAX)] {
        let summary = user(&[&format!(
            "[[rule]]\nid = \"{id}\"\n[rule.summarize]\nhead = {head}\ntail = {tail}\n"
        )])
        .unwrap_err();
        assert!(
            matches!(&summary, RulesError::Summary { id: found, .. } if *found == id),
            "{summary:?}"
        );
    }
    assert!(user(&[&format!("[[rule]]\nid = \"{id}\"\n[rule.summarize]\nhead = {MAX_SUMMARY}\ntail = {MAX_SUMMARY}\n")]).is_ok());
    let missing = user(&["[[rule]]\nid = \" \"\n"]).unwrap_err();
    assert!(matches!(missing, RulesError::MissingId { .. }));
    let long = |id: String, counters: usize, name: usize| {
        let counters: String = (0..counters)
            .map(|_| {
                format!(
                    "[[rule.counters]]\nname = \"{}\"\npattern = \"a\"\n",
                    "n".repeat(name)
                )
            })
            .collect();
        user(&[&format!("[[rule]]\nid = \"{id}\"\n{counters}")])
    };
    assert!(long("i".repeat(MAX_NAME), MAX_COUNTERS, MAX_NAME).is_ok());
    for refused in [
        long("i".repeat(MAX_NAME + 1), 0, 0),
        long(word(), MAX_COUNTERS + 1, 1),
        long(word(), 1, MAX_NAME + 1),
    ] {
        assert!(matches!(refused.unwrap_err(), RulesError::TooLong { .. }));
    }
    let rule = format!("[[rule]]\nid = \"{id}\"\n");
    let duplicate = user(&[&rule, &rule]).unwrap_err();
    assert!(
        matches!(&duplicate, RulesError::Duplicate { first, second, .. }
        if first == Path::new("0.toml") && second == Path::new("1.toml"))
    );
    let many: String = (0..=MAX_RULES)
        .map(|_| format!("[[rule]]\nid = \"{}\"\n", word()))
        .collect();
    assert!(matches!(user(&[&many]).unwrap_err(), RulesError::TooMany));
}

#[test]
fn rule_files_load_from_a_directory_in_name_order() {
    let directory = tempfile::tempdir().unwrap();
    // A missing directory adds nothing.
    let none = Rules::load(&directory.path().join(word())).unwrap();
    assert_eq!(none.rules().count(), Rules::builtin().rules().count());
    let (first, second) = (word(), word());
    std::fs::write(
        directory.path().join("b.toml"),
        format!("[[rule]]\nid = \"{second}\"\npriority = 5\n"),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("a.toml"),
        format!("[[rule]]\nid = \"{first}\"\npriority = 5\n"),
    )
    .unwrap();
    std::fs::write(directory.path().join("ignored.txt"), "not toml").unwrap();
    let rules = Rules::load(directory.path()).unwrap();
    let user: Vec<_> = rules
        .rules()
        .filter(|rule| rule.origin == Origin::User)
        .map(|rule| (rule.id.to_owned(), rule.source.map(Path::to_owned)))
        .collect();
    assert_eq!(
        user,
        [
            (first, Some(directory.path().join("a.toml"))),
            (second, Some(directory.path().join("b.toml")))
        ]
    );
    let large = directory.path().join("c.toml");
    std::fs::write(&large, "#".repeat(MAX_RULE_FILE as usize + 1)).unwrap();
    assert!(
        matches!(Rules::load(directory.path()).unwrap_err(), RulesError::TooLarge { path } if path == large)
    );
}

#[test]
fn the_documented_example_is_a_valid_rule_file() {
    let guide = include_str!("../../../docs/output-rules.md");
    let example = guide
        .split("```toml\n")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("docs/output-rules.md has a TOML example");
    let rules = user(&[example]).unwrap();
    let rule = rules.rules().find(|rule| rule.id == "make").unwrap();
    assert_eq!((rule.priority, rule.origin), (60, Origin::User));
}

#[test]
fn terminal_escapes_are_removed_without_touching_text() {
    let rules =
        user(&["[[rule]]\nid = \"plain\"\n[rule.transforms]\nstrip_ansi = true\n"]).unwrap();
    let text = word();
    let output = format!("\u{1b}[31m日本語 {text}\u{1b}[0m 🦀\u{1b}]0;title\u{7}\n");
    let reduced = rules
        .reduce(&Input {
            tool: "x",
            command: None,
            output: &output,
        })
        .unwrap();
    assert_eq!(
        reduced.text,
        format!("日本語 {text} 🦀\n[output rule `plain` kept 1 of 1 lines]")
    );
}
