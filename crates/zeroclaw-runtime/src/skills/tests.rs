use super::*;

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn slash_option_kinds_registry_is_walked_from_the_enum() {
        // The published registry is exactly `SlashOptionKind::ALL` walked into
        // descriptors, in order. No hand-authored rows: adding a variant to the
        // enum extends this without touching the builder.
        let registry = slash_option_kinds();
        assert_eq!(registry.len(), SlashOptionKind::ALL.len());
        for (descriptor, kind) in registry.iter().zip(SlashOptionKind::ALL) {
            assert_eq!(descriptor.manifest_name, kind.manifest_name());
            assert_eq!(descriptor.supports_choices, kind.supports_choices());
            assert_eq!(
                descriptor.supports_numeric_bounds,
                kind.supports_numeric_bounds()
            );
            assert_eq!(
                descriptor.supports_length_bounds,
                kind.supports_length_bounds()
            );
        }
    }

    #[test]
    fn only_scalar_kinds_carry_bounds_and_choices() {
        // Capability invariants the surfaces depend on: numeric bounds imply a
        // scalar with choices; length bounds are string-only.
        for kind in SlashOptionKind::ALL {
            if kind.supports_numeric_bounds() || kind.supports_length_bounds() {
                assert!(
                    kind.supports_choices(),
                    "{:?} carries bounds but is not choiceable",
                    kind.manifest_name()
                );
            }
        }
        assert!(SlashOptionKind::String.supports_length_bounds());
        assert!(!SlashOptionKind::String.supports_numeric_bounds());
        assert!(SlashOptionKind::Integer.supports_numeric_bounds());
        assert!(!SlashOptionKind::Integer.supports_length_bounds());
    }

    #[test]
    fn parse_simple_frontmatter_keeps_blank_line_in_block_scalar() {
        // A blank line is a paragraph break *inside* a YAML block scalar, not a
        // terminator. The parser must not truncate the description at it.
        let frontmatter = "name: x\ndescription: >-\n  para one\n\n  para two\n";
        let meta = parse_simple_frontmatter(frontmatter);
        let desc = meta.description.expect("description should be parsed");
        assert!(
            desc.contains("para one"),
            "first paragraph missing: {desc:?}"
        );
        assert!(
            desc.contains("para two"),
            "second paragraph after blank line was truncated: {desc:?}"
        );
        assert_eq!(meta.name.as_deref(), Some("x"));
    }

    #[test]
    fn parse_simple_frontmatter_block_scalar_stops_at_next_key() {
        // A real, non-indented next key must still terminate the block scalar.
        let frontmatter = "description: >-\n  hello\n  world\nversion: 1.2.3\n";
        let meta = parse_simple_frontmatter(frontmatter);
        assert_eq!(meta.description.as_deref(), Some("hello world"));
        assert_eq!(meta.version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn test_is_registry_source_accepts_bare_names() {
        assert!(is_registry_source("auto-coder"));
        assert!(is_registry_source("web-researcher"));
        assert!(is_registry_source("telegram-assistant"));
        assert!(is_registry_source("data_analyst"));
        assert!(is_registry_source("ci-helper"));
        assert!(is_registry_source("selfimproving"));
    }

    #[test]
    fn test_is_registry_source_rejects_empty() {
        assert!(!is_registry_source(""));
    }

    #[test]
    fn test_is_registry_source_rejects_paths() {
        assert!(!is_registry_source("./my-skill"));
        assert!(!is_registry_source("../my-skill"));
        assert!(!is_registry_source("/abs/path"));
        assert!(!is_registry_source("skills/auto-coder"));
        assert!(!is_registry_source("some\\path"));
        assert!(!is_registry_source("~/.zeroclaw/skills/foo"));
    }

    #[test]
    fn test_is_registry_source_rejects_urls() {
        assert!(!is_registry_source("https://github.com/foo/bar"));
        assert!(!is_registry_source("http://example.com"));
        assert!(!is_registry_source("ssh://git@host/repo"));
        assert!(!is_registry_source("git://host/repo"));
        assert!(!is_registry_source("git@github.com:user/repo"));
    }

    #[test]
    fn test_is_registry_source_rejects_prefixed() {
        assert!(!is_registry_source("external:my-skill"));
    }

    #[test]
    fn test_is_registry_source_rejects_traversal() {
        assert!(!is_registry_source(".."));
        assert!(!is_registry_source("foo..bar"));
    }

    #[test]
    fn test_is_registry_source_rejects_special_chars() {
        assert!(!is_registry_source(".hidden"));
        assert!(!is_registry_source("~tilde"));
    }

    #[test]
    fn test_is_extra_registry_source_accepts_valid() {
        assert!(is_extra_registry_source("registry:myreg/auto-coder"));
        assert!(is_extra_registry_source("registry:co_op/data_analyst"));
        assert!(is_extra_registry_source("registry:r1/ci-helper"));
    }

    #[test]
    fn test_is_extra_registry_source_rejects_malformed() {
        assert!(!is_extra_registry_source(""));
        assert!(!is_extra_registry_source("registry:"));
        assert!(!is_extra_registry_source("registry:onlyname"));
        assert!(!is_extra_registry_source("registry:a/b/c"));
        assert!(!is_extra_registry_source("registry:../x"));
        assert!(!is_extra_registry_source("registry:a /b"));
        assert!(!is_extra_registry_source("registry:a/b:c"));
        assert!(!is_extra_registry_source("registry:/skill"));
        assert!(!is_extra_registry_source("registry:name/"));
        // A bare name has no prefix and stays a Tier-1 registry install.
        assert!(!is_extra_registry_source("auto-coder"));
    }

    #[test]
    fn test_is_extra_registry_source_rejects_competing_schemes() {
        assert!(!is_extra_registry_source("external:x"));
        assert!(!is_extra_registry_source("https://github.com/o/r"));
        assert!(!is_extra_registry_source("git@github.com:o/r"));
        assert!(!is_extra_registry_source("./local"));
    }

    #[test]
    fn test_parse_extra_registry_source_splits() {
        assert_eq!(
            parse_extra_registry_source("registry:myreg/auto-coder"),
            Some(("myreg".to_string(), "auto-coder".to_string()))
        );
        assert_eq!(parse_extra_registry_source("registry:onlyname"), None);
        assert_eq!(parse_extra_registry_source("registry:a/b/c"), None);
        assert_eq!(parse_extra_registry_source("auto-coder"), None);
    }

    #[test]
    fn test_install_extra_registry_unknown_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_path = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_path).unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let err = install_extra_registry_skill_source(
            "registry:nope/demo",
            &skills_path,
            false,
            &workspace,
            &[],
            true,
        )
        .expect_err("unknown registry must error before any git work");
        assert!(err.to_string().contains("nope"), "got: {err}");
    }

    #[test]
    fn test_install_git_catalog_rejects_non_bare_skill_name() {
        // The bare-name guard must reject anything with a path separator before
        // any network/git work happens (hermetic — no clone is attempted).
        assert!(!is_registry_source("a/b"));

        let tmp = tempfile::tempdir().unwrap();
        let skills_path = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_path).unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let err = install_git_catalog_skill_source(
            "https://github.com/example/skills",
            "a/b",
            &skills_path,
            false,
            &workspace,
        )
        .expect_err("a slashed --skill name must be rejected before any git work");
        assert!(err.to_string().contains("bare skill name"), "got: {err}");
    }

    /// Build a local git repository that acts as a skill catalog: a real commit
    /// containing `skills/<name>/SKILL.md` for each requested skill. Returns the
    /// repo path, which doubles as the clone URL for
    /// `install_git_catalog_skill_source` (git clones local paths directly, so
    /// the test stays hermetic — no network).
    fn init_git_skill_catalog(root: &Path, skills: &[&str]) -> std::path::PathBuf {
        let repo = root.join("catalog");
        for name in skills {
            let skill_dir = repo.join("skills").join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!(
                    "---\nname: {name}\ndescription: hermetic git-catalog fixture\n---\n\n# {name}\n"
                ),
            )
            .unwrap();
        }
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git must be available to build the catalog fixture");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["add", "-A"]);
        // Pass identity/signing inline so the commit does not depend on the
        // runner's global git config.
        run(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "init",
        ]);
        repo
    }

    #[test]
    fn install_git_catalog_skill_source_installs_selected_skill_through_audit() {
        // Happy path for the `--skill` replacement: clone a local git catalog,
        // resolve `skills/<name>/`, and install it through the shared
        // clone → local-copy → security-audit path. Skipped if git is absent.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let catalog = init_git_skill_catalog(tmp.path(), &["demo-skill", "other-skill"]);
        let skills_path = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_path).unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let (dest, files_scanned) = install_git_catalog_skill_source(
            catalog.to_str().unwrap(),
            "demo-skill",
            &skills_path,
            false,
            &workspace,
        )
        .expect("happy-path git-catalog install should succeed");

        // Installed at the expected destination, with the catalog's SKILL.md.
        assert_eq!(dest, skills_path.join("demo-skill"));
        assert!(
            dest.join("SKILL.md").is_file(),
            "the selected skill's SKILL.md must be installed"
        );
        // A non-zero scan count proves the security-audit path was entered.
        assert!(
            files_scanned >= 1,
            "install must run through the audit path; files_scanned = {files_scanned}"
        );
        // Only the requested skill is installed, not its sibling.
        assert!(!skills_path.join("other-skill").exists());
        // The transient clone scratch dir is cleaned up afterwards.
        let leftover = std::fs::read_dir(&workspace)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".skill-catalog-")
            });
        assert!(!leftover, "clone scratch dir must be removed after install");
    }

    #[test]
    fn install_git_catalog_skill_source_reports_missing_skill_after_clone() {
        // The main post-clone failure mode: the requested skill is not in the
        // catalog. The error must name it and list what *is* available, and must
        // not install anything.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let catalog = init_git_skill_catalog(tmp.path(), &["present-skill"]);
        let skills_path = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_path).unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let err = install_git_catalog_skill_source(
            catalog.to_str().unwrap(),
            "absent-skill",
            &skills_path,
            false,
            &workspace,
        )
        .expect_err("a skill missing from the catalog must error after clone");
        let msg = err.to_string();
        assert!(msg.contains("not found"), "got: {msg}");
        assert!(
            msg.contains("present-skill"),
            "error should list the available skills; got: {msg}"
        );
        // Nothing installed, and the clone scratch dir is cleaned up.
        assert!(!skills_path.join("absent-skill").exists());
        let leftover = std::fs::read_dir(&workspace)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".skill-catalog-")
            });
        assert!(!leftover, "clone scratch dir must be removed after failure");
    }

    /// Commit whatever is currently in `repo`'s worktree with a hermetic
    /// identity, so tests can add symlink entries the fixture builder can't.
    #[cfg(unix)]
    fn git_commit_all(repo: &Path, message: &str) {
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .expect("git must be available");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["add", "-A"]);
        run(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            message,
        ]);
    }

    #[cfg(unix)]
    #[test]
    fn install_git_catalog_does_not_follow_registry_sync_marker_symlink() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let external_marker = tmp.path().join("external-marker");
        std::fs::write(&external_marker, "must remain unchanged").unwrap();

        let catalog = init_git_skill_catalog(tmp.path(), &["demo-skill"]);
        std::os::unix::fs::symlink(&external_marker, catalog.join(SKILLS_REGISTRY_SYNC_MARKER))
            .unwrap();
        git_commit_all(&catalog, "add hostile registry sync marker symlink");

        let skills_path = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_path).unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let (dest, _) = install_git_catalog_skill_source(
            catalog.to_str().unwrap(),
            "demo-skill",
            &skills_path,
            false,
            &workspace,
        )
        .expect("a catalog marker must not participate in transient clone state");

        assert!(dest.join("SKILL.md").is_file());
        assert_eq!(
            std::fs::read_to_string(&external_marker).unwrap(),
            "must remain unchanged",
            "a catalog-controlled marker symlink must not redirect a host write"
        );
        let leftover = std::fs::read_dir(&workspace)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".skill-catalog-")
            });
        assert!(!leftover, "clone scratch dir must be removed after install");
    }

    #[cfg(unix)]
    #[test]
    fn install_git_catalog_skill_source_rejects_symlinked_selected_skill() {
        // A catalog that commits `skills/<name>` as a symlink pointing outside
        // the repo must be refused: `is_dir()` follows the link and
        // `install_local_skill_source` would canonicalize it to the external
        // target and audit/copy it. The out-of-clone directory here is itself a
        // *clean* skill, proving the audit passing does not rescue containment.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        // A valid skill living outside the catalog — the escape target.
        let outside = tmp.path().join("outside").join("secret-skill");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            outside.join("SKILL.md"),
            "---\nname: secret-skill\ndescription: outside the catalog\n---\n\n# secret\n",
        )
        .unwrap();

        let catalog = init_git_skill_catalog(tmp.path(), &["present-skill"]);
        // Commit an absolute symlink `skills/evil` -> the external skill dir.
        std::os::unix::fs::symlink(&outside, catalog.join("skills").join("evil")).unwrap();
        git_commit_all(&catalog, "add escaping symlink");

        let skills_path = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_path).unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let err = install_git_catalog_skill_source(
            catalog.to_str().unwrap(),
            "evil",
            &skills_path,
            false,
            &workspace,
        )
        .expect_err("a symlinked catalog entry must be rejected");
        assert!(
            err.to_string().contains("symlink"),
            "error should name the symlink; got: {err}"
        );
        // Nothing installed — neither the symlink name nor the escape target.
        assert!(!skills_path.join("evil").exists());
        assert!(!skills_path.join("secret-skill").exists());
        // The escape target on disk is untouched.
        assert!(outside.join("SKILL.md").is_file());
        let leftover = std::fs::read_dir(&workspace)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".skill-catalog-")
            });
        assert!(
            !leftover,
            "clone scratch dir must be removed after rejection"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_git_catalog_skill_source_rejects_selection_escaping_via_symlinked_skills_dir() {
        // Backstop for the case the symlink_metadata check alone misses: the
        // selected `skills/<name>` is a real directory, but its parent `skills`
        // is a symlink out of the clone. The final component is not a link, so
        // only the canonicalize-and-contain check catches the escape.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        // External directory that `skills` will point at, holding a clean skill.
        let external = tmp.path().join("external-skills");
        let victim = external.join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(
            victim.join("SKILL.md"),
            "---\nname: victim\ndescription: outside the catalog\n---\n\n# victim\n",
        )
        .unwrap();

        // A repo whose entire `skills/` tree is a symlink to `external`.
        let catalog = tmp.path().join("catalog");
        std::fs::create_dir_all(&catalog).unwrap();
        std::os::unix::fs::symlink(&external, catalog.join("skills")).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&catalog)
            .output()
            .expect("git init");
        git_commit_all(&catalog, "symlink skills dir out of the repo");

        let skills_path = tmp.path().join("dest-skills");
        std::fs::create_dir_all(&skills_path).unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let err = install_git_catalog_skill_source(
            catalog.to_str().unwrap(),
            "victim",
            &skills_path,
            false,
            &workspace,
        )
        .expect_err("a selection resolving outside the clone must be rejected");
        assert!(
            err.to_string().contains("outside") || err.to_string().contains("symlink"),
            "error should describe the containment/symlink failure; got: {err}"
        );
        assert!(!skills_path.join("victim").exists());
        assert!(victim.join("SKILL.md").is_file());
        let leftover = std::fs::read_dir(&workspace)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".skill-catalog-")
            });
        assert!(
            !leftover,
            "clone scratch dir must be removed after rejection"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_git_catalog_missing_skill_rejects_symlinked_skills_root_before_enumeration() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git not available");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let external = tmp.path().join("external-skills");
        let external_entry = external.join("external-private-name");
        std::fs::create_dir_all(&external_entry).unwrap();
        let external_manifest = external_entry.join("SKILL.md");
        let external_contents =
            "---\nname: external-private-name\ndescription: outside the catalog\n---\n";
        std::fs::write(&external_manifest, external_contents).unwrap();

        let catalog = tmp.path().join("catalog");
        std::fs::create_dir_all(&catalog).unwrap();
        std::os::unix::fs::symlink(&external, catalog.join("skills")).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&catalog)
            .output()
            .expect("git init");
        git_commit_all(&catalog, "symlink skills root out of the repo");

        let skills_path = tmp.path().join("dest-skills");
        std::fs::create_dir_all(&skills_path).unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let err = install_git_catalog_skill_source(
            catalog.to_str().unwrap(),
            "missing-skill",
            &skills_path,
            false,
            &workspace,
        )
        .expect_err("a symlinked catalog skills root must be rejected before enumeration");
        let message = err.to_string();
        assert!(message.contains("symlink"), "got: {message}");
        assert!(
            !message.contains("external-private-name"),
            "external entry names must not be enumerated; got: {message}"
        );
        assert_eq!(
            std::fs::read_dir(&skills_path).unwrap().count(),
            0,
            "nothing may be installed after rejecting the catalog root"
        );
        assert_eq!(
            std::fs::read_to_string(&external_manifest).unwrap(),
            external_contents,
            "the external target must remain untouched"
        );
        let leftover = std::fs::read_dir(&workspace)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".skill-catalog-")
            });
        assert!(
            !leftover,
            "clone scratch dir must be removed after rejection"
        );
    }

    #[test]
    fn tier_from_tags_recognizes_official() {
        assert_eq!(
            tier_from_tags(&["Official".into(), "Featured".into()]),
            SkillTier::Official
        );
        // Case-insensitive match.
        assert_eq!(tier_from_tags(&["official".into()]), SkillTier::Official);
    }

    #[test]
    fn tier_from_tags_recognizes_community() {
        assert_eq!(tier_from_tags(&["Community".into()]), SkillTier::Community);
    }

    #[test]
    fn tier_from_tags_recognizes_featured_only() {
        assert_eq!(tier_from_tags(&["Featured".into()]), SkillTier::Featured);
    }

    #[test]
    fn tier_from_tags_falls_back_to_unknown_when_no_tier_tag() {
        assert_eq!(tier_from_tags(&[]), SkillTier::Unknown);
        assert_eq!(
            tier_from_tags(&["productivity".into(), "automation".into()]),
            SkillTier::Unknown
        );
    }

    /// Resolve a tier banner against the English catalogue only — locale- and
    /// filesystem-independent, mirroring build_install_tier_banner's assembly.
    fn english_tier_banner(name: &str, version: Option<&str>, tier: SkillTier) -> String {
        let version_label = version.unwrap_or("?");
        let args = [("name", name), ("version", version_label)];
        let mut banner =
            crate::i18n::get_english_cli_string_with_args(install_tier_banner_key(tier), &args);
        if !banner.ends_with('\n') {
            banner.push('\n');
        }
        banner
    }

    #[test]
    fn build_install_tier_banner_official_is_single_line() {
        let banner = english_tier_banner("auto-coder", Some("0.3.0"), SkillTier::Official);
        assert!(banner.contains("Official (zeroclaw-labs maintained)"));
        assert!(banner.contains("Installing auto-coder v0.3.0"));
        assert!(!banner.contains("not audited"));
        // One trailing newline, no warn block.
        assert_eq!(banner.lines().count(), 1);
    }

    #[test]
    fn build_install_tier_banner_community_warns() {
        let banner = english_tier_banner("discord-moderator", Some("0.1.2"), SkillTier::Community);
        assert!(banner.contains("Community submission"));
        assert!(banner.contains("not audited by ZeroClaw"));
        assert!(banner.contains("zeroclaw skills audit discord-moderator"));
    }

    #[test]
    fn build_install_tier_banner_featured_uses_community_warning() {
        let banner = english_tier_banner("hand-picked", Some("1.0"), SkillTier::Featured);
        assert!(banner.contains("Community submission"));
        assert!(banner.contains("not audited by ZeroClaw"));
    }

    #[test]
    fn build_install_tier_banner_unknown_falls_back_to_community() {
        let banner = english_tier_banner("legacy", None, SkillTier::Unknown);
        assert!(banner.contains("Community submission"));
        assert!(banner.contains("not audited by ZeroClaw"));
        // Missing version is rendered as `v?` rather than panicking.
        assert!(banner.contains("v?"));
    }

    #[test]
    fn lookup_registry_skill_tier_resolves_from_registry_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let json = r#"{
            "version": 1,
            "skills": [
                { "name": "auto-coder", "version": "0.3.0", "tags": ["Official", "Featured"] },
                { "name": "discord-moderator", "version": "0.1.2", "tags": ["Community"] },
                { "name": "hand-picked", "version": "1.0.0", "tags": ["Featured"] },
                { "name": "untagged", "version": "0.0.1", "tags": ["productivity"] }
            ]
        }"#;
        std::fs::write(tmp.path().join("registry.json"), json).unwrap();

        assert_eq!(
            lookup_registry_skill_tier(tmp.path(), "auto-coder"),
            (SkillTier::Official, Some("0.3.0".to_string()))
        );
        assert_eq!(
            lookup_registry_skill_tier(tmp.path(), "discord-moderator"),
            (SkillTier::Community, Some("0.1.2".to_string()))
        );
        assert_eq!(
            lookup_registry_skill_tier(tmp.path(), "hand-picked"),
            (SkillTier::Featured, Some("1.0.0".to_string()))
        );
        // Skill present but no tier tag → Unknown (treated as Community by the banner).
        assert_eq!(
            lookup_registry_skill_tier(tmp.path(), "untagged"),
            (SkillTier::Unknown, Some("0.0.1".to_string()))
        );
        // Skill not in registry.json at all → Unknown with no version.
        assert_eq!(
            lookup_registry_skill_tier(tmp.path(), "missing"),
            (SkillTier::Unknown, None)
        );
    }

    #[test]
    fn lookup_registry_skill_tier_handles_missing_index() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            lookup_registry_skill_tier(tmp.path(), "anything"),
            (SkillTier::Unknown, None)
        );
    }

    #[test]
    fn lookup_registry_skill_tier_handles_malformed_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("registry.json"), "{ not json").unwrap();
        assert_eq!(
            lookup_registry_skill_tier(tmp.path(), "anything"),
            (SkillTier::Unknown, None)
        );
    }
}

#[cfg(test)]
mod prompts_section_tests {
    use super::*;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, toml: &str) -> std::path::PathBuf {
        let p = dir.join("SKILL.toml");
        std::fs::write(&p, toml).unwrap();
        p
    }

    #[test]
    fn prompts_inside_skill_section_are_loaded() {
        let tmp = TempDir::new().unwrap();
        let path = write_manifest(
            tmp.path(),
            r#"
[skill]
name = "probe"
description = "test"
version = "0.1.0"
prompts = ["If asked about XYZZY, respond YES"]
"#,
        );
        let skill = load_skill_toml(&path).unwrap();
        assert_eq!(
            skill.prompts,
            vec!["If asked about XYZZY, respond YES".to_string()]
        );
    }

    #[test]
    fn typed_slash_options_are_parsed_from_the_skill_table() {
        let tmp = TempDir::new().unwrap();
        let path = write_manifest(
            tmp.path(),
            r#"
[skill]
name = "search"
description = "Search the web"
version = "0.1.0"
tags = ["slash"]

[[skill.slash_options]]
name = "query"
description = "The search query"
type = "string"
required = true
max_length = 200

[[skill.slash_options]]
name = "sort"
description = "Sort order"
type = "string"
choices = [
    { name = "Newest", value = "new" },
    { name = "Oldest", value = "old" },
]
"#,
        );
        let skill = load_skill_toml(&path).unwrap();
        assert_eq!(skill.slash_options.len(), 2);

        let query = &skill.slash_options[0];
        assert_eq!(query.name, "query");
        assert_eq!(query.kind, "string");
        assert!(query.required);
        assert_eq!(query.max_length, Some(200));

        let sort = &skill.slash_options[1];
        assert_eq!(sort.name, "sort");
        assert!(!sort.required);
        assert_eq!(sort.choices.len(), 2);
        assert_eq!(sort.choices[0].name, "Newest");
        assert_eq!(sort.choices[0].value, "new");
    }

    #[test]
    fn description_localizations_parse_at_command_and_option_level() {
        let tmp = TempDir::new().unwrap();
        let path = write_manifest(
            tmp.path(),
            r#"
[skill]
name = "search"
description = "Search the web"
version = "0.1.0"
tags = ["slash"]
description_localizations = { fr = "Rechercher sur le web", ja = "ウェブを検索" }

[[skill.slash_options]]
name = "query"
description = "The search query"
type = "string"
description_localizations = { fr = "La requête de recherche" }
"#,
        );
        let skill = load_skill_toml(&path).unwrap();
        assert_eq!(
            skill
                .description_localizations
                .get("fr")
                .map(String::as_str),
            Some("Rechercher sur le web")
        );
        assert_eq!(
            skill
                .description_localizations
                .get("ja")
                .map(String::as_str),
            Some("ウェブを検索")
        );
        assert_eq!(
            skill.slash_options[0]
                .description_localizations
                .get("fr")
                .map(String::as_str),
            Some("La requête de recherche")
        );
    }

    #[test]
    fn skills_without_slash_options_default_to_empty() {
        let tmp = TempDir::new().unwrap();
        let path = write_manifest(
            tmp.path(),
            r#"
[skill]
name = "probe"
description = "test"
version = "0.1.0"
"#,
        );
        let skill = load_skill_toml(&path).unwrap();
        assert!(skill.slash_options.is_empty());
    }

    #[test]
    fn load_skill_md_parses_slash_options_from_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let md = r#"---
name: draft
description: Draft content to a spec.
tags: [slash]
slash_options:
  - name: format
    description: Output format.
    type: string
    required: true
    choices: [{name: Email, value: email}, {name: Tweet, value: tweet}]
  - name: words
    type: integer
    min: 10
    max: 2000
---
# Draft

Write it.
"#;
        let path = tmp.path().join("SKILL.md");
        std::fs::write(&path, md).unwrap();
        let skill = load_skill_md(&path, tmp.path()).unwrap();

        // Parity with SKILL.toml: the runtime Skill carries typed options.
        assert_eq!(skill.slash_options.len(), 2);
        assert_eq!(skill.slash_options[0].name, "format");
        assert!(skill.slash_options[0].required);
        assert_eq!(skill.slash_options[0].choices.len(), 2);
        assert_eq!(skill.slash_options[1].kind, "integer");
        assert_eq!(skill.slash_options[1].min, Some(10.0));
        assert_eq!(skill.slash_options[1].max, Some(2000.0));
        assert!(skill.tags.contains(&"slash".to_string()));

        // The options block lives in frontmatter, so the prompt (body) is clean.
        assert_eq!(skill.prompts.len(), 1);
        assert!(skill.prompts[0].contains("Write it."));
        assert!(!skill.prompts[0].contains("slash_options"));
    }

    #[test]
    fn load_skill_md_without_slash_options_is_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("SKILL.md");
        std::fs::write(&path, "---\nname: plain\ndescription: d\n---\n# Plain\n").unwrap();
        let skill = load_skill_md(&path, tmp.path()).unwrap();
        assert!(skill.slash_options.is_empty());
    }

    #[test]
    fn prompts_at_root_level_still_work() {
        let tmp = TempDir::new().unwrap();
        let path = write_manifest(
            tmp.path(),
            r#"
[skill]
name = "probe"
description = "test"
version = "0.1.0"

prompts = ["legacy root-level prompt"]
"#,
        );
        let skill = load_skill_toml(&path).unwrap();
        assert_eq!(skill.prompts, vec!["legacy root-level prompt".to_string()]);
    }

    #[test]
    fn prompts_in_both_locations_are_merged_skill_first() {
        // Root-level prompts must precede the [skill] header in TOML.
        // Per the fix, [skill]-section prompts appear first in the merged
        // list, with root-level prompts appended after.
        let tmp = TempDir::new().unwrap();
        let path = write_manifest(
            tmp.path(),
            r#"
prompts = ["from-root"]

[skill]
name = "probe"
description = "test"
version = "0.1.0"
prompts = ["from-skill-section"]
"#,
        );
        let skill = load_skill_toml(&path).unwrap();
        assert_eq!(
            skill.prompts,
            vec!["from-skill-section".to_string(), "from-root".to_string(),]
        );
    }
}

#[cfg(test)]
mod skill_manifest_tests {
    use super::*;

    #[test]
    fn parses_valid_skill_manifest() {
        let toml_str = r#"
[skill]
name = "x"
description = "y"
"#;
        let manifest: SkillManifest =
            toml::from_str(toml_str).expect("valid manifest should parse");
        assert_eq!(manifest.skill.name, "x");
        assert_eq!(manifest.skill.description, "y");
        assert_eq!(manifest.skill.version, "0.1.0");
        assert!(manifest.tools.is_empty());
        assert!(manifest.prompts.is_empty());
    }

    #[test]
    fn rejects_unknown_field_in_skill_block() {
        let toml_str = r#"
[skill]
name = "x"
description = "y"
descriptin = "oops"
"#;
        let err = toml::from_str::<SkillManifest>(toml_str)
            .expect_err("unknown field in [skill] should be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("descriptin"),
            "error should mention the unknown field 'descriptin'; got: {msg}"
        );
    }

    #[test]
    fn accepts_prompts_in_skill_block_with_strictness() {
        let toml_str = r#"
[skill]
name = "x"
description = "y"
prompts = ["one", "two"]
"#;
        let manifest: SkillManifest = toml::from_str(toml_str)
            .expect("manifest with prompts in [skill] should parse under deny_unknown_fields");
        assert_eq!(
            manifest.skill.prompts,
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn parses_skill_without_forge_block() {
        let toml_str = r#"
[skill]
name = "hand-authored"
description = "no forge block"
"#;
        let manifest: SkillManifest =
            toml::from_str(toml_str).expect("manifest without [forge] should parse cleanly");
        assert!(
            manifest.forge.is_none(),
            "forge should be None when [forge] is absent"
        );
        assert_eq!(manifest.skill.name, "hand-authored");
    }

    #[test]
    fn parses_skill_with_forge_block() {
        let toml_str = r#"
[skill]
name = "auto-integrated"
description = "from skillforge"

[forge]
source = "https://github.com/user/auto-integrated"
owner = "user"
language = "Rust"
license = true
stars = 42
updated_at = "2026-04-30"

[forge.requirements]
runtime = "zeroclaw >= 0.1"

[forge.metadata]
auto_integrated = true
forge_timestamp = "2026-04-30T12:00:00Z"
"#;
        let manifest: SkillManifest =
            toml::from_str(toml_str).expect("manifest with [forge] block should parse cleanly");
        let forge = manifest
            .forge
            .expect("forge should be Some when [forge] is present");
        assert_eq!(
            forge.source.as_deref(),
            Some("https://github.com/user/auto-integrated")
        );
        assert_eq!(forge.owner.as_deref(), Some("user"));
        assert_eq!(forge.language.as_deref(), Some("Rust"));
        assert_eq!(forge.license, Some(true));
        assert_eq!(forge.stars, Some(42));
        assert_eq!(forge.updated_at.as_deref(), Some("2026-04-30"));
        assert_eq!(
            forge.requirements.get("runtime").and_then(|v| v.as_str()),
            Some("zeroclaw >= 0.1"),
        );
        assert_eq!(
            forge
                .metadata
                .get("auto_integrated")
                .and_then(|v| v.as_bool()),
            Some(true),
        );
    }

    #[test]
    fn rejects_unknown_field_in_forge_block() {
        let toml_str = r#"
[skill]
name = "x"
description = "y"

[forge]
source = "https://github.com/user/x"
licence = true
"#;
        let err = toml::from_str::<SkillManifest>(toml_str)
            .expect_err("unknown field in [forge] should be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("licence"),
            "error should mention the unknown field 'licence'; got: {msg}"
        );
    }

    #[test]
    fn workspace_swallow_site_skips_invalid_toml_without_panicking() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        // Bad skill: typo in [skill] — rejected by deny_unknown_fields.
        let bad_dir = skills_dir.join("bad-skill");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(
            bad_dir.join("SKILL.toml"),
            r#"
[skill]
name = "bad"
description = "has a typo"
descriptin = "oops"
"#,
        )
        .unwrap();

        // Good skill: parses cleanly — must still load.
        let good_dir = skills_dir.join("good-skill");
        std::fs::create_dir_all(&good_dir).unwrap();
        std::fs::write(
            good_dir.join("SKILL.toml"),
            r#"
[skill]
name = "good"
description = "fine"
"#,
        )
        .unwrap();

        let (skills, dropped) = load_skills_from_directory(&skills_dir, false);
        // The bad skill is skipped (not panicked-on). The good skill loads.
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"good"),
            "good skill must load; got: {names:?}"
        );
        assert!(
            !names.contains(&"bad"),
            "bad skill must be skipped, not silently accepted; got: {names:?}"
        );
        // the skipped skill is surfaced as an audit drop, not silently lost.
        assert_eq!(dropped.len(), 1, "the bad TOML skill must be reported");
        assert_eq!(dropped[0].origin_hint, "workspace");
        assert!(matches!(
            dropped[0].reason,
            SkillDropReason::ManifestParseError(_)
        ));
    }

    #[test]
    fn workspace_script_bundling_skill_reported_as_scripts_blocked_drop() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let script_dir = skills_dir.join("script-skill");
        std::fs::create_dir_all(&script_dir).unwrap();
        std::fs::write(
            script_dir.join("SKILL.md"),
            "---\nname: script-skill\ndescription: bundles a shell helper\n---\n# Script Skill\n",
        )
        .unwrap();
        std::fs::write(script_dir.join("helper.sh"), "echo hi\n").unwrap();

        let (skills, dropped) = load_skills_from_directory(&skills_dir, false);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(
            !names.contains(&"script-skill"),
            "script-bundling skill must be dropped at the secure default; got: {names:?}"
        );
        assert_eq!(dropped.len(), 1, "the script skill must be reported");
        assert_eq!(dropped[0].origin_hint, "workspace");
        match &dropped[0].reason {
            SkillDropReason::AuditFindings {
                summary,
                scripts_blocked,
            } => {
                assert!(
                    *scripts_blocked,
                    "reason must flag scripts as the blocker; got: {summary}"
                );
                assert!(
                    summary.contains("script-like files are blocked"),
                    "summary must describe the script block; got: {summary}"
                );
            }
            other => panic!("expected AuditFindings, got: {other:?}"),
        }

        let (skills, dropped) = load_skills_from_directory(&skills_dir, true);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"script-skill"),
            "script-bundling skill must load once allow_scripts=true; got: {names:?}"
        );
        assert!(
            dropped.is_empty(),
            "no drops expected with allow_scripts=true; got: {dropped:?}"
        );
    }
    #[test]
    fn open_skills_swallow_site_skips_invalid_toml_without_panicking() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let skills_dir = tmp.path().join("open-skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let bad_dir = skills_dir.join("bad-open-skill");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(
            bad_dir.join("SKILL.toml"),
            r#"
[skill]
name = "bad-open"
description = "has a typo"
autor = "oops"
"#,
        )
        .unwrap();

        let good_dir = skills_dir.join("good-open-skill");
        std::fs::create_dir_all(&good_dir).unwrap();
        std::fs::write(
            good_dir.join("SKILL.toml"),
            r#"
[skill]
name = "good-open"
description = "fine"
"#,
        )
        .unwrap();

        let (skills, dropped) = load_open_skills_from_directory(&skills_dir, false);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(dropped.len(), 1, "the bad open-skill TOML must be reported");
        assert_eq!(dropped[0].origin_hint, "open-skills");
        assert!(
            names.contains(&"good-open"),
            "good open-skill must load; got: {names:?}"
        );
        assert!(
            !names.contains(&"bad-open"),
            "bad open-skill must be skipped, not silently accepted; got: {names:?}"
        );
    }
}

#[cfg(test)]
mod prompt_callable_name_tests {
    use super::*;
    use std::path::Path;

    fn tool(name: &str, kind: &str) -> SkillTool {
        SkillTool {
            name: name.to_string(),
            description: "desc".to_string(),
            kind: kind.to_string(),
            command: "echo hi".to_string(),
            args: HashMap::new(),
            target: None,
            locked_args: HashMap::new(),
            timeout_secs: None,
        }
    }

    #[test]
    fn prompt_callable_name_matches_registered_tool_name() {
        let skill = Skill {
            name: "pr-review-toolkit:code-reviewer".to_string(),
            description: "review".to_string(),
            description_localizations: Default::default(),
            version: "1.0.0".to_string(),
            author: None,
            tags: Vec::new(),
            tools: vec![tool("run.lint", "shell")],
            prompts: Vec::new(),
            slash_options: Vec::new(),
            location: None,
        };

        let prompt = skills_to_prompt_with_mode(
            std::slice::from_ref(&skill),
            Path::new("/tmp"),
            zeroclaw_config::schema::SkillsPromptInjectionMode::Full,
        );

        let registered =
            crate::tools::skill_tool::composed_tool_name(&skill.name, &skill.tools[0].name);
        assert!(
            prompt.contains(&format!("<name>{registered}</name>")),
            "prompt is missing the sanitized callable name `{registered}`:\n{prompt}",
        );
        // The raw, provider-invalid composed name must never reach the prompt.
        assert!(
            !prompt.contains("pr-review-toolkit:code-reviewer__run.lint"),
            "prompt advertised the raw, unsanitized composed name:\n{prompt}",
        );
    }

    fn tool_with_target(name: &str, kind: &str, target: &str) -> SkillTool {
        SkillTool {
            target: Some(target.to_string()),
            ..tool(name, kind)
        }
    }

    #[test]
    fn prompt_callable_predicate_matches_registration_preconditions() {
        // shell/script/http always register -> always prompt-callable.
        assert!(skill_tool_is_prompt_callable(&tool("run", "shell")));
        assert!(skill_tool_is_prompt_callable(&tool("run", "script")));
        assert!(skill_tool_is_prompt_callable(&tool("fetch", "http")));
        // builtin/mcp are elevation wrappers: callable only WITH a target.
        assert!(skill_tool_is_prompt_callable(&tool_with_target(
            "gen",
            "mcp",
            "images__generate"
        )));
        assert!(skill_tool_is_prompt_callable(&tool_with_target(
            "sh", "builtin", "shell"
        )));
        // ... and NOT callable without one (the converter's resolve_elevated_tool
        // would return None, so advertising them callable lies to the model).
        assert!(!skill_tool_is_prompt_callable(&tool("gen", "mcp")));
        assert!(!skill_tool_is_prompt_callable(&tool("sh", "builtin")));
        // A whitespace-only target is as good as absent.
        assert!(!skill_tool_is_prompt_callable(&tool_with_target(
            "gen", "mcp", "   "
        )));
        // unknown kinds are never callable.
        assert!(!skill_tool_is_prompt_callable(&tool("x", "weird")));
    }

    fn elevating_skill(target: &str) -> Skill {
        let mut elevated = tool("reach", "mcp");
        elevated.target = Some(target.to_string());
        Skill {
            name: "ops".to_string(),
            description: "d".to_string(),
            description_localizations: Default::default(),
            version: "1.0.0".to_string(),
            author: None,
            tags: Vec::new(),
            tools: vec![elevated],
            prompts: Vec::new(),
            slash_options: Vec::new(),
            location: None,
        }
    }

    fn policy_allowing(tools: &[&str]) -> std::sync::Arc<crate::security::SecurityPolicy> {
        std::sync::Arc::new(crate::security::SecurityPolicy {
            allowed_tools: Some(tools.iter().map(|t| (*t).to_string()).collect()),
            ..crate::security::SecurityPolicy::default()
        })
    }

    /// A stand-in for a connected MCP server tool, so the resolution registry
    /// actually contains the target. Without this the elevation fails at the
    /// registry lookup and the policy gate is never reached — a test using an
    /// empty registry passes whether or not the gate exists.
    struct FakeMcpTool(&'static str);

    #[async_trait::async_trait]
    impl zeroclaw_api::tool::Tool for FakeMcpTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "fake mcp tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> anyhow::Result<zeroclaw_api::tool::ToolResult> {
            unreachable!("registration-time test never executes the target")
        }
    }

    impl zeroclaw_api::attribution::Attributable for FakeMcpTool {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Tool(zeroclaw_api::attribution::ToolKind::Plugin)
        }
        fn alias(&self) -> &str {
            self.0
        }
    }

    fn registry_with(name: &'static str) -> Vec<std::sync::Arc<dyn zeroclaw_api::tool::Tool>> {
        vec![std::sync::Arc::new(FakeMcpTool(name))]
    }

    /// The hole a cold review found: a wrapper is named after its skill, so
    /// filtering on the wrapper's own name asks about a string no allow-list
    /// contains. An `mcp` elevation therefore reached a server tool the profile
    /// never granted, under a name the profile could not deny.
    #[test]
    fn elevation_cannot_reach_a_tool_the_profile_never_granted() {
        let skill = elevating_skill("images__generate");
        let registered: Vec<String> = crate::skills::skills_to_tools_with_context(
            std::slice::from_ref(&skill),
            policy_allowing(&["memory_recall"]),
            &registry_with("images__generate"),
        )
        .iter()
        .map(|t| t.name().to_string())
        .collect();

        assert!(
            registered.is_empty(),
            "the target is present and resolvable, so only the authority gate can \
             stop this; a skill must not reach an ungranted MCP tool by wrapping \
             it: {registered:?}"
        );
    }

    /// The other half, and the reason the test above is not vacuous: refusing
    /// every elevation would also "fix" the hole while destroying the feature.
    /// Same skill, same registry, only the grant differs.
    #[test]
    fn elevation_still_works_when_the_target_is_granted() {
        let skill = elevating_skill("images__generate");
        let registered: Vec<String> = crate::skills::skills_to_tools_with_context(
            std::slice::from_ref(&skill),
            policy_allowing(&["images__generate"]),
            &registry_with("images__generate"),
        )
        .iter()
        .map(|t| t.name().to_string())
        .collect();

        assert_eq!(
            registered.len(),
            1,
            "a granted target must still elevate: {registered:?}"
        );
    }

    /// `excluded_tools` must NOT gate elevation, and the distinction is easy to
    /// lose: `SecurityPolicy::is_tool_allowed` folds both lists together, so
    /// reaching for it here would silently break the sanctioned use of skills.
    ///
    /// "Do not hand the model raw `shell`" and "this agent may never touch
    /// `shell`" are different statements. A scoped wrapper with locked arguments
    /// is the answer to the first; only the second is an authority boundary.
    #[test]
    fn excluded_tools_does_not_gate_elevation_only_the_allow_list_does() {
        let mut elevated = tool("reach", "builtin");
        elevated.target = Some("shell".to_string());
        let skill = Skill {
            name: "ops".to_string(),
            description: "d".to_string(),
            description_localizations: Default::default(),
            version: "1.0.0".to_string(),
            author: None,
            tags: Vec::new(),
            tools: vec![elevated],
            prompts: Vec::new(),
            slash_options: Vec::new(),
            location: None,
        };

        let excluded_only = std::sync::Arc::new(crate::security::SecurityPolicy {
            excluded_tools: Some(vec!["shell".to_string()]),
            ..crate::security::SecurityPolicy::default()
        });
        let registered = crate::skills::skills_to_tools_with_context(
            std::slice::from_ref(&skill),
            excluded_only,
            &registry_with("shell"),
        );
        assert_eq!(
            registered.len(),
            1,
            "excluding the raw tool is exactly when a scoped wrapper is wanted; \
             gating elevation on it would delete the feature"
        );
    }

    /// An unrestricted profile keeps today's behaviour: `allowed_tools = None`
    /// means no allow-list, so elevation is not gated by one.
    #[test]
    fn an_unrestricted_profile_does_not_gate_elevation() {
        let policy = crate::security::SecurityPolicy::default();
        assert!(
            policy.allowed_tools.is_none(),
            "default policy must stay unrestricted, or this test proves nothing"
        );
        assert!(policy.is_tool_allowed("images__generate"));
    }

    #[test]
    fn converter_skips_targetless_elevation_matching_the_prompt_predicate() {
        // The end-to-end invariant the renderer relies on: the registry converter
        // registers exactly the tools `skill_tool_is_prompt_callable` marks callable
        // (for what is statically decidable). A target-less builtin/mcp elevation
        // tool is skipped by the converter, so it must not be advertised callable.
        let security = std::sync::Arc::new(crate::security::SecurityPolicy::default());
        let skill = Skill {
            name: "ops".to_string(),
            description: "d".to_string(),
            description_localizations: Default::default(),
            version: "1.0.0".to_string(),
            author: None,
            tags: Vec::new(),
            tools: vec![
                tool("run", "shell"),  // always registers
                tool("orphan", "mcp"), // no target -> skipped
                tool("sh", "builtin"), // no target -> skipped
            ],
            prompts: Vec::new(),
            slash_options: Vec::new(),
            location: None,
        };

        let registered: Vec<String> =
            crate::skills::skills_to_tools(std::slice::from_ref(&skill), security)
                .iter()
                .map(|t| t.name().to_string())
                .collect();

        // shell registers; the target-less elevation tools do not - matching the
        // prompt predicate for each.
        for t in &skill.tools {
            let composed = crate::tools::skill_tool::composed_tool_name(&skill.name, &t.name);
            let in_registry = registered.iter().any(|n| n == &composed);
            assert_eq!(
                in_registry,
                skill_tool_is_prompt_callable(t),
                "prompt-callable and registry-registered must agree for {} ({}): registry={in_registry}",
                t.name,
                t.kind,
            );
        }
    }

    #[test]
    fn prompt_lists_mcp_with_target_as_callable_and_targetless_as_not() {
        let skill = Skill {
            name: "imagegen".to_string(),
            description: "d".to_string(),
            description_localizations: Default::default(),
            version: "1.0.0".to_string(),
            author: None,
            tags: Vec::new(),
            tools: vec![
                tool_with_target("generate", "mcp", "images__generate"),
                tool("orphan", "mcp"), // no target -> not registered
            ],
            prompts: Vec::new(),
            slash_options: Vec::new(),
            location: None,
        };

        let prompt = skills_to_prompt_with_mode(
            std::slice::from_ref(&skill),
            Path::new("/tmp"),
            zeroclaw_config::schema::SkillsPromptInjectionMode::Full,
        );

        // The callable block comes first, the unregistered <tools> block after.
        let callable_idx = prompt
            .find("<callable_tools")
            .expect("callable_tools block");
        let tools_at = prompt
            .find("<tools>")
            .expect("unregistered <tools> block present for the target-less mcp tool");
        assert!(
            callable_idx < tools_at,
            "callable block precedes unregistered block"
        );

        // The targeted mcp tool is advertised as callable (composed name, under
        // <callable_tools>, before the unregistered block).
        let callable = crate::tools::skill_tool::composed_tool_name(&skill.name, "generate");
        let callable_at = prompt
            .find(&format!("<name>{callable}</name>"))
            .expect("targeted mcp skill tool must be present as a callable name");
        assert!(
            callable_at > callable_idx && callable_at < tools_at,
            "targeted mcp skill tool must render under <callable_tools>:\n{prompt}"
        );

        // The target-less mcp tool renders under the unregistered <tools> block
        // (raw name, after the callable block) - the converter would skip it.
        let orphan_at = prompt
            .find("<name>orphan</name>")
            .expect("target-less mcp skill tool must be present under <tools>");
        assert!(
            orphan_at > tools_at,
            "target-less mcp skill tool must render as unregistered, not callable:\n{prompt}"
        );
    }
}

#[cfg(test)]
mod workspace_dir_regression_tests {
    use super::*;
    use tempfile::TempDir;

    fn make_config_with_agent_workspace(
        install_root: &Path,
        data_dir: &Path,
        agent_alias: &str,
        workspace_path: PathBuf,
    ) -> zeroclaw_config::schema::Config {
        let mut config = zeroclaw_config::schema::Config {
            config_path: install_root.join("config.toml"),
            data_dir: data_dir.to_path_buf(),
            ..Default::default()
        };

        let agent = zeroclaw_config::schema::AliasedAgentConfig {
            workspace: zeroclaw_config::multi_agent::AgentWorkspaceConfig {
                path: Some(workspace_path),
                ..Default::default()
            },
            ..Default::default()
        };

        config.agents.insert(agent_alias.to_string(), agent);
        config
    }

    fn write_test_skill(workspace: &Path, skill_name: &str) {
        let skill_dir = workspace.join("skills").join(skill_name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.toml"),
            format!(
                r#"[skill]
name = "{skill_name}"
description = "regression test skill"
version = "0.1.0"
"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn load_skills_for_agent_from_config_audited_returns_dropped() {
        let install_root = TempDir::new().unwrap();
        let data_dir = TempDir::new().unwrap();
        let agent_workspace = TempDir::new().unwrap();
        let agent_alias = "audit-agent";

        write_test_skill(agent_workspace.path(), "clean-skill");
        // A broken-manifest skill in the same workspace.
        let broken = agent_workspace.path().join("skills").join("broken-skill");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(
            broken.join("SKILL.toml"),
            "[skill]\nname = \"broken-skill\"\ndescription = \"d\"\nbogus = true\n",
        )
        .unwrap();

        let config = make_config_with_agent_workspace(
            install_root.path(),
            data_dir.path(),
            agent_alias,
            agent_workspace.path().to_path_buf(),
        );

        cache::invalidate();
        let (skills, dropped, _shadows) =
            load_skills_for_agent_from_config_audited(&config, agent_alias);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"clean-skill"), "got: {names:?}");
        assert!(!names.contains(&"broken-skill"), "got: {names:?}");
        assert_eq!(dropped.len(), 1, "the broken skill must be reported");
        assert_eq!(dropped[0].origin_hint, "workspace");
        assert!(matches!(
            dropped[0].reason,
            SkillDropReason::ManifestParseError(_)
        ));
    }

    #[test]
    fn load_skills_for_agent_from_config_uses_workspace_dir_not_data_dir() {
        let install_root = TempDir::new().unwrap();
        let data_dir = TempDir::new().unwrap();
        let agent_workspace = TempDir::new().unwrap();

        let agent_alias = "test-agent";
        let skill_name = "workspace-only-regression-skill";

        write_test_skill(agent_workspace.path(), skill_name);

        let config = make_config_with_agent_workspace(
            install_root.path(),
            data_dir.path(),
            agent_alias,
            agent_workspace.path().to_path_buf(),
        );

        let workspace_dir = config.agent_workspace_dir(agent_alias);
        assert_eq!(
            workspace_dir,
            agent_workspace.path(),
            "agent_workspace_dir must resolve to the custom workspace path"
        );
        assert_ne!(
            workspace_dir, config.data_dir,
            "workspace_dir and data_dir must be distinct for this test to be meaningful"
        );

        // Test the production helper — this is what the three call sites use.
        let skills_from_helper = load_skills_for_agent_from_config(&config, agent_alias);
        let helper_skill_names: Vec<&str> =
            skills_from_helper.iter().map(|s| s.name.as_str()).collect();
        assert!(
            helper_skill_names.contains(&skill_name),
            "load_skills_for_agent_from_config must load skills from agent workspace; got: {helper_skill_names:?}"
        );

        // Verify that using data_dir directly would NOT find the skill (the bug).
        let skills_from_data_dir = load_skills_for_agent(&config.data_dir, &config, agent_alias);
        let data_dir_skill_names: Vec<&str> = skills_from_data_dir
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !data_dir_skill_names.contains(&skill_name),
            "skill in agent workspace must NOT be loaded when passing data_dir (this was the bug); got: {data_dir_skill_names:?}"
        );
    }

    #[test]
    fn load_skills_for_agent_from_config_empty_bundles_uses_workspace_dir() {
        let install_root = TempDir::new().unwrap();
        let data_dir = TempDir::new().unwrap();
        let agent_workspace = TempDir::new().unwrap();

        let agent_alias = "bundle-fallback-agent";
        let skill_name = "workspace-fallback-skill";

        write_test_skill(agent_workspace.path(), skill_name);

        let config = make_config_with_agent_workspace(
            install_root.path(),
            data_dir.path(),
            agent_alias,
            agent_workspace.path().to_path_buf(),
        );

        let skills = load_skills_for_agent_from_config(&config, agent_alias);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&skill_name),
            "with empty skill_bundles, workspace skills must still load; got: {names:?}"
        );
    }
}
