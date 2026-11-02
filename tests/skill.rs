use taskloop::validate_skill;
fn package() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("normal-skill");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("SKILL.md"), "---\nname: normal-skill\ndescription: A normal package without orchestration.\n---\nRead supporting resources when needed.\n").unwrap();
    (temp, path)
}
#[test]
fn ordinary_skill_does_not_require_playbook_or_text_only_assets() {
    let (_temp, path) = package();
    std::fs::create_dir(path.join("assets")).unwrap();
    std::fs::write(path.join("assets/icon.bin"), [0, 255, 128]).unwrap();
    let report = validate_skill(&path).unwrap();
    assert_eq!(report.name, "normal-skill");
    assert_eq!(report.files, 2);
    assert_eq!(report.declared_version, None);
}
#[test]
fn invalid_frontmatter_and_symlink_are_rejected_without_execution() {
    let (_temp, path) = package();
    std::fs::write(
        path.join("SKILL.md"),
        "---\nname: normal-skill\ndescription: Test.\nmetadata:\n  version: 1\n---\n",
    )
    .unwrap();
    assert_eq!(
        validate_skill(&path).unwrap_err().path,
        "/SKILL.md/metadata"
    );
    std::fs::write(
        path.join("SKILL.md"),
        "---\nname: normal-skill\ndescription: Test.\n---\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("/etc/passwd", path.join("escape")).unwrap();
    assert!(validate_skill(&path).is_err());
}

#[test]
fn optional_standard_fields_reject_non_string_values() {
    for field in ["license", "allowed-tools"] {
        let (_temp, path) = package();
        std::fs::write(
            path.join("SKILL.md"),
            format!(
                "---\nname: normal-skill\ndescription: Test.\n{field}: [unexpected, list]\n---\n"
            ),
        )
        .unwrap();
        assert_eq!(
            validate_skill(&path).unwrap_err().path,
            format!("/SKILL.md/{field}")
        );
    }
}
