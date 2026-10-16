use super::*;

fn skill_text(name: &str, extra: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: >\n  Does one\n  thing.\n{extra}---\n\n# Steps\nDo it.\n"
    )
}
fn write(root: &Path, name: &str, text: &str) {
    std::fs::create_dir_all(root.join(name)).unwrap();
    std::fs::write(root.join(name).join("SKILL.md"), text).unwrap();
}

#[test]
fn parses_frontmatter_and_metadata_in_both_forms() {
    let bin = uuid::Uuid::new_v4().simple().to_string();
    let json = format!(
        "metadata: '{{\"openclaw\": {{\"emoji\": \"x\", \"requires\": {{\"bins\": [\"{bin}\"]}}}}}}'\n"
    );
    let skill = parse(&skill_text("one", &json), "one").unwrap();
    assert_eq!(skill.description.as_deref(), Some("Does one thing."));
    assert_eq!(skill.emoji.as_deref(), Some("x"));
    assert_eq!(skill.instructions, "# Steps\nDo it.\n");
    assert_eq!(skill.requires.unmet(), [format!("needs {bin}")]);

    let map = format!("metadata:\n  openclaw:\n    requires:\n      anyBins: [\"{bin}\"]\n");
    let skill = parse(&skill_text("one", &map), "one").unwrap();
    assert_eq!(skill.requires.any_bins, [bin]);
    assert_eq!(skill.digest.len(), 64);
}

#[test]
fn rejects_malformed_skills() {
    assert!(matches!(
        parse("# no frontmatter", "a"),
        Err(SkillError::NoFrontmatter)
    ));
    assert!(matches!(
        parse("---\nname: [\n---\n", "a"),
        Err(SkillError::Frontmatter(_))
    ));
    assert!(matches!(
        parse(&skill_text("Bad_Name", ""), "Bad_Name"),
        Err(SkillError::Name)
    ));
    assert!(matches!(
        parse(&skill_text("one", ""), "two"),
        Err(SkillError::NameMismatch)
    ));
    assert!(matches!(
        parse(&skill_text("one", "metadata: '{'\n"), "one"),
        Err(SkillError::Metadata(_))
    ));
}

#[test]
fn digest_changes_with_any_edit() {
    let a = parse(&skill_text("one", ""), "one").unwrap();
    let b = parse(
        &skill_text("one", "").replace("Do it.", "Do it now."),
        "one",
    )
    .unwrap();
    assert_ne!(a.digest, b.digest);
}

#[test]
fn scan_reports_broken_skills_and_ignores_other_entries() {
    let root = tempfile::tempdir().unwrap();
    assert!(scan(&root.path().join("missing")).unwrap().is_empty());
    write(root.path(), "good", &skill_text("good", ""));
    write(root.path(), "broken", "nothing here");
    std::fs::write(root.path().join("loose.md"), "x").unwrap();
    std::fs::create_dir(root.path().join(".hidden")).unwrap();
    let found = scan(root.path()).unwrap();
    let names: Vec<_> = found.iter().map(|e| e.directory.as_str()).collect();
    assert_eq!(names, ["broken", "good"]);
    assert!(matches!(found[0].skill, Err(SkillError::NoFrontmatter)));
    assert_eq!(found[1].skill.as_ref().unwrap().name, "good");
}

#[test]
fn bounds_skill_size() {
    let root = tempfile::tempdir().unwrap();
    let big = skill_text("big", "") + &"x".repeat(MAX_SKILL_BYTES as usize);
    write(root.path(), "big", &big);
    assert!(matches!(
        load(root.path(), "big"),
        Err(SkillError::TooLarge { .. })
    ));
    assert!(matches!(
        load(root.path(), "none"),
        Err(SkillError::NotFound)
    ));
    assert!(matches!(load(root.path(), "../x"), Err(SkillError::Name)));
}

#[test]
fn supporting_files_stay_inside_the_skill() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "one", &skill_text("one", ""));
    let text = uuid::Uuid::new_v4().to_string();
    std::fs::create_dir(root.path().join("one/ref")).unwrap();
    std::fs::write(root.path().join("one/ref/notes.md"), &text).unwrap();
    std::fs::write(root.path().join("secret"), "s").unwrap();
    assert_eq!(
        read_file(root.path(), "one", "ref/notes.md").unwrap(),
        text.as_bytes()
    );
    for path in [
        "",
        "/etc/passwd",
        "../secret",
        "ref/../../secret",
        "./SKILL.md",
    ] {
        assert!(
            matches!(read_file(root.path(), "one", path), Err(SkillError::Path)),
            "{path}"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.path().join("secret"), root.path().join("one/link"))
            .unwrap();
        assert!(matches!(
            read_file(root.path(), "one", "link"),
            Err(SkillError::Path)
        ));
        std::os::unix::fs::symlink(root.path().join("one"), root.path().join("alias")).unwrap();
        assert!(matches!(load(root.path(), "alias"), Err(SkillError::Path)));
    }
}

#[test]
fn index_escapes_author_text() {
    let text = index([("one", Some("a <b>\n\"c\" & d"))]);
    assert!(text.contains("<skill name=\"one\">a &lt;b&gt; &quot;c&quot; &amp; d</skill>"));
}
