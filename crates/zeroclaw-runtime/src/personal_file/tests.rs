//! LA1 boundary discriminations (#271) and negative-capability scans.
//!
//! Each required discrimination from the issue appears here under its
//! exact name. The race test injects attacker steps through the
//! `cfg(test)`-only hook around the safety primitives; production
//! ordering is untouched. Filesystem-backed discriminations run on Unix
//! (the platform the safety core supports); every other platform runs
//! the fail-closed gate.

use std::path::Path;

use super::domain::{
    ExpectedContentIdentity, MAX_TEXT_BYTES, PersonalFileError, PersonalFileRefusal,
    PersonalFileResult, PersonalRelativePath, PersonalRootRef, TRASH_NAMESPACE,
};
use super::service::{MoveDestination, MoveSource, PersonalFileService};

// ─────────────────────────────────────────────────────────────────────
// Fixtures (Unix: the safety core's supported platform)
// ─────────────────────────────────────────────────────────────────────

/// The test-only race hook is process-global and fires on every
/// mutation, so fs-backed discriminations run serialized against it.
#[cfg(unix)]
static FS_TEST_SERIALIZER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(unix)]
async fn fs_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    FS_TEST_SERIALIZER.lock().await
}

#[cfg(unix)]
fn service_with_rw_root(dir: &Path) -> (PersonalFileService, PersonalRootRef) {
    let root = PersonalFileService::admit_read_write(dir).expect("admit read-write root");
    let service = PersonalFileService::new(vec![root.clone()], vec![]).expect("service");
    (service, root)
}

#[cfg(unix)]
async fn read_text_of(
    service: &PersonalFileService,
    root: &PersonalRootRef,
    raw: &str,
) -> Result<String, PersonalFileError> {
    let path = PersonalRelativePath::parse(raw)?;
    match service.read_text(root, &path).await? {
        PersonalFileResult::ReadText { text, .. } => Ok(text),
        other => panic!("unexpected result class: {:?}", other.operation()),
    }
}

#[cfg(unix)]
async fn expect_refused<T>(
    what: &str,
    result: Result<T, PersonalFileError>,
    matches_refusal: impl Fn(&PersonalFileRefusal) -> bool,
) {
    match result {
        Ok(_) => panic!("{what}: operation must be refused"),
        Err(PersonalFileError::Refused(refusal)) => {
            assert!(
                matches_refusal(&refusal),
                "{what}: wrong refusal: {refusal}"
            );
        }
        Err(other) => panic!("{what}: expected a typed refusal, got {other}"),
    }
}

#[cfg(unix)]
fn unix_symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("create symlink");
}

/// One-shot hook installation for the race discriminations: the attacker
/// step runs exactly once, at the first safety-primitive boundary.
#[cfg(unix)]
fn install_one_shot_race_hook(step: impl Fn() + Send + Sync + 'static) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    CALLS.store(0, Ordering::SeqCst);
    let counter = &CALLS;
    *super::safety::RACE_HOOK.lock().expect("hook mutex") = Some(Box::new(move || {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            step();
        }
    }));
}

#[cfg(unix)]
fn clear_race_hook() {
    *super::safety::RACE_HOOK.lock().expect("hook mutex") = None;
}

// ─────────────────────────────────────────────────────────────────────
// Required discriminations
// ─────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn managed_personal_text_create_read_replace_and_trash_roundtrip() {
    let _fs_serialized = fs_test_guard().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, root) = service_with_rw_root(tmp.path());

    // create
    let notes = PersonalRelativePath::parse("notes/todo.txt").expect("path");
    let created = service
        .create_text_no_clobber(&root, &notes, "buy oat milk\n")
        .await
        .expect("create");
    assert!(matches!(created, PersonalFileResult::Created { .. }));

    // read (content + content identity)
    let identity = match service.read_text(&root, &notes).await.expect("read") {
        PersonalFileResult::ReadText { text, identity } => {
            assert_eq!(text, "buy oat milk\n");
            identity
        }
        other => panic!("unexpected result class: {:?}", other.operation()),
    };
    assert_eq!(
        identity,
        ExpectedContentIdentity::of_content(b"buy oat milk\n")
    );

    // replace with the expected identity: atomic, prior recoverable
    let prior_in_trash = match service
        .replace_text_if_expected(&root, &notes, &identity, "buy oat milk\nwater the fern\n")
        .await
        .expect("replace")
    {
        PersonalFileResult::Replaced {
            identity: new_identity,
            prior_in_trash,
        } => {
            assert_eq!(
                new_identity,
                ExpectedContentIdentity::of_content(b"buy oat milk\nwater the fern\n")
            );
            prior_in_trash
        }
        other => panic!("unexpected result class: {:?}", other.operation()),
    };
    assert_eq!(
        read_text_of(&service, &root, "notes/todo.txt")
            .await
            .expect("read"),
        "buy oat milk\nwater the fern\n"
    );
    let prior_path = tmp.path().join(prior_in_trash);
    assert_eq!(
        std::fs::read(&prior_path).expect("prior content recoverable"),
        b"buy oat milk\n"
    );

    // delete to trash; ordinary read then misses the file, the trash
    // copy still holds the content
    let trash_location = match service.delete_to_trash(&root, &notes).await.expect("trash") {
        PersonalFileResult::Trashed { trash_location } => trash_location,
        other => panic!("unexpected result class: {:?}", other.operation()),
    };
    match read_text_of(&service, &root, "notes/todo.txt").await {
        Err(PersonalFileError::NotFound(_)) => {}
        other => panic!("deleted file must be not-found, got {other:?}"),
    }
    let trashed_path = tmp.path().join(trash_location);
    assert_eq!(
        std::fs::read(&trashed_path).expect("trashed content"),
        b"buy oat milk\nwater the fern\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn absolute_or_unadmitted_root_is_unrepresentable_or_refused() {
    let _fs_serialized = fs_test_guard().await;
    // Unrepresentable: the typed path API cannot carry an absolute path.
    assert!(matches!(
        PersonalRelativePath::parse("/etc/passwd"),
        Err(PersonalFileError::Refused(
            PersonalFileRefusal::AbsolutePath { .. }
        ))
    ));

    // Unrepresentable: there is no ambient admission. A root admitted
    // for one service is refused by another service instance.
    let tmp_a = tempfile::tempdir().expect("tempdir a");
    let tmp_b = tempfile::tempdir().expect("tempdir b");
    let (service_a, root_a) = service_with_rw_root(tmp_a.path());
    let (_service_b, root_b) = service_with_rw_root(tmp_b.path());

    let path = PersonalRelativePath::parse("f.txt").expect("path");
    service_a
        .create_text_no_clobber(&root_a, &path, "a")
        .await
        .expect("create in own root");
    match service_a.read_text(&root_b, &path).await {
        Err(PersonalFileError::Refused(PersonalFileRefusal::UnadmittedRoot)) => {}
        other => panic!("foreign root must be refused, got {other:?}"),
    }
}

#[tokio::test]
async fn relative_escape_and_dotdot_are_refused() {
    for raw in ["../escape.txt", "a/../../escape.txt", "a/..", ".."] {
        assert!(
            matches!(
                PersonalRelativePath::parse(raw),
                Err(PersonalFileError::Refused(
                    PersonalFileRefusal::ParentComponent { .. }
                ))
            ),
            "{raw} must refuse as a parent traversal"
        );
    }
    for raw in ["./dot.txt", "a/./b.txt", "a/", "", "."] {
        assert!(
            PersonalRelativePath::parse(raw).is_err(),
            "{raw:?} must refuse to parse"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_root_ancestor_and_leaf_are_refused() {
    let _fs_serialized = fs_test_guard().await;
    let tmp = tempfile::tempdir().expect("tempdir");

    // A symlinked root refuses admission instead of silently admitting
    // the redirect target.
    let real = tempfile::tempdir_in(tmp.path()).expect("real dir");
    unix_symlink(real.path(), &tmp.path().join("link"));
    match PersonalFileService::admit_read_write(&tmp.path().join("link")) {
        Err(PersonalFileError::Refused(PersonalFileRefusal::SymlinkedRoot { .. })) => {}
        other => panic!("symlinked root must refuse admission, got {other:?}"),
    }

    std::fs::create_dir_all(tmp.path().join("root/sub")).expect("subdir");
    std::fs::write(tmp.path().join("root/sub/inside.txt"), "inside").expect("file");
    let (service, root) = service_with_rw_root(&tmp.path().join("root"));

    // A symlinked leaf is refused (no-follow), even though its target
    // would be readable.
    unix_symlink(
        &tmp.path().join("root/sub/inside.txt"),
        &tmp.path().join("root/alias.txt"),
    );
    let alias = PersonalRelativePath::parse("alias.txt").expect("path");
    expect_refused(
        "symlinked leaf",
        service.read_text(&root, &alias).await,
        |refusal| matches!(refusal, PersonalFileRefusal::Symlink { .. }),
    )
    .await;

    // A symlinked ancestor is refused: the walk never resolves through
    // it (nor adopts its target's identity).
    unix_symlink(
        &tmp.path().join("root/sub"),
        &tmp.path().join("root/sub-link"),
    );
    let through_link = PersonalRelativePath::parse("sub-link/inside.txt").expect("path");
    expect_refused(
        "symlinked ancestor",
        service.read_text(&root, &through_link).await,
        |refusal| matches!(refusal, PersonalFileRefusal::Symlink { .. }),
    )
    .await;

    // The real path through the real directory still works.
    assert_eq!(
        read_text_of(&service, &root, "sub/inside.txt")
            .await
            .expect("read"),
        "inside"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn race_swapped_ancestor_or_leaf_cannot_escape_or_mutate() {
    let _fs_serialized = fs_test_guard().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, root) = service_with_rw_root(tmp.path());

    // A victim directory OUTSIDE the root: the race must never reach it.
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("victim.txt"), "do not touch").expect("victim");
    let outside_file = outside.path().join("victim.txt");

    std::fs::create_dir_all(tmp.path().join("notes")).expect("notes dir");
    let seed = "seed content";
    let note_path = tmp.path().join("notes/plan.txt");
    std::fs::write(&note_path, seed).expect("seed file");
    let path = PersonalRelativePath::parse("notes/plan.txt").expect("path");
    let identity = match service.read_text(&root, &path).await.expect("read") {
        PersonalFileResult::ReadText { identity, .. } => identity,
        other => panic!("unexpected result class: {:?}", other.operation()),
    };

    // Attack 1: between verification and publication, swap the LEAF for
    // a symlink pointing at the outside victim. The no-follow re-check
    // must refuse before publication; the victim stays untouched.
    let leaf_target = note_path.clone();
    let victim = outside_file.clone();
    let hook_leaf = leaf_target.clone();
    install_one_shot_race_hook(move || {
        std::fs::remove_file(&hook_leaf).expect("remove leaf");
        std::os::unix::fs::symlink(&victim, &hook_leaf).expect("swap leaf for symlink");
    });
    let attack = service
        .replace_text_if_expected(&root, &path, &identity, "attacker text")
        .await;
    clear_race_hook();
    expect_refused("swapped leaf during replace", attack, |refusal| {
        matches!(
            refusal,
            PersonalFileRefusal::Symlink { .. }
                | PersonalFileRefusal::ConcurrentModification { .. }
        )
    })
    .await;
    assert_eq!(
        std::fs::read(&outside_file).expect("victim read"),
        b"do not touch",
        "the outside victim must never be mutated"
    );
    assert!(
        std::fs::symlink_metadata(&leaf_target)
            .expect("leaf still present")
            .file_type()
            .is_symlink(),
        "publication must not have replaced the swapped name"
    );
    std::fs::remove_file(&leaf_target).expect("cleanup attack");
    std::fs::write(&note_path, seed).expect("restore seed");

    // Attack 2: before a create lands, swap the ANCESTOR directory for a
    // symlink to the outside dir. The walk already holds the real
    // directory descriptor, so the write must land inside the real
    // (relocated) directory — never inside the outside dir.
    let ancestor = tmp.path().join("notes");
    let outside_dir = outside.path().to_path_buf();
    let relocated = tmp.path().join("notes.relocated");
    let hook_relocated = relocated.clone();
    let hook_ancestor = ancestor.clone();
    install_one_shot_race_hook(move || {
        std::fs::rename(&hook_ancestor, &hook_relocated).expect("relocate ancestor");
        std::os::unix::fs::symlink(&outside_dir, &hook_ancestor)
            .expect("swap ancestor for symlink");
    });
    let new_file = PersonalRelativePath::parse("notes/new.txt").expect("path");
    let attack = service
        .create_text_no_clobber(&root, &new_file, "created under race")
        .await;
    clear_race_hook();
    attack.expect("create must survive the ancestor swap without escaping");
    assert!(
        !outside.path().join("new.txt").exists(),
        "nothing may land in the outside dir"
    );
    assert_eq!(
        std::fs::read(relocated.join("new.txt")).expect("created file in real dir"),
        b"created under race"
    );
    // Ordinary path resolution now hits the swapped symlink — and is
    // refused there too (no adoption of swapped components).
    match read_text_of(&service, &root, "notes/new.txt").await {
        Err(PersonalFileError::Refused(PersonalFileRefusal::Symlink { .. })) => {}
        other => panic!("swapped ancestor read must be refused, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn hardlinked_foreign_inode_is_not_modified() {
    let _fs_serialized = fs_test_guard().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, root) = service_with_rw_root(tmp.path());

    let path = PersonalRelativePath::parse("shared.txt").expect("path");
    service
        .create_text_no_clobber(&root, &path, "original")
        .await
        .expect("create");
    std::fs::hard_link(
        tmp.path().join("shared.txt"),
        tmp.path().join("alias-hard.txt"),
    )
    .expect("hard link");
    let identity = match service.read_text(&root, &path).await.expect("read") {
        PersonalFileResult::ReadText { identity, .. } => identity,
        other => panic!("unexpected result class: {:?}", other.operation()),
    };

    expect_refused(
        "replace of hard-linked file",
        service
            .replace_text_if_expected(&root, &path, &identity, "corrupted")
            .await,
        |refusal| matches!(refusal, PersonalFileRefusal::Hardlinked { .. }),
    )
    .await;
    expect_refused(
        "delete of hard-linked file",
        service.delete_to_trash(&root, &path).await,
        |refusal| matches!(refusal, PersonalFileRefusal::Hardlinked { .. }),
    )
    .await;
    let dst = PersonalRelativePath::parse("moved.txt").expect("path");
    expect_refused(
        "move of hard-linked file",
        service
            .move_no_clobber(
                MoveSource {
                    root: &root,
                    path: &path,
                },
                MoveDestination {
                    root: &root,
                    path: &dst,
                },
            )
            .await,
        |refusal| matches!(refusal, PersonalFileRefusal::Hardlinked { .. }),
    )
    .await;

    // both names still carry the original content
    assert_eq!(
        std::fs::read(tmp.path().join("shared.txt")).expect("read"),
        b"original"
    );
    assert_eq!(
        std::fs::read(tmp.path().join("alias-hard.txt")).expect("read"),
        b"original"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn repo_root_and_nested_git_worktree_are_refused() {
    let _fs_serialized = fs_test_guard().await;
    let tmp = tempfile::tempdir().expect("tempdir");

    // A root that IS a repository (holds a .git directory) refuses
    // admission as a write root.
    let repo = tempfile::tempdir_in(tmp.path()).expect("repo dir");
    std::fs::create_dir(repo.path().join(".git")).expect("git dir");
    match PersonalFileService::admit_read_write(repo.path()) {
        Err(PersonalFileError::Refused(PersonalFileRefusal::GitRepository { .. })) => {}
        other => panic!("repo root must refuse admission, got {other:?}"),
    }

    // A root holding a .git WORKTREE FILE (gitdir: pointer) refuses
    // admission as a write root too.
    let worktree = tempfile::tempdir_in(tmp.path()).expect("worktree dir");
    std::fs::write(worktree.path().join(".git"), "gitdir: /somewhere/else").expect("git file");
    match PersonalFileService::admit_read_write(worktree.path()) {
        Err(PersonalFileError::Refused(PersonalFileRefusal::GitRepository { .. })) => {}
        other => panic!("git worktree root must refuse admission, got {other:?}"),
    }

    // A root INSIDE a repository refuses admission (ancestor scan).
    let nested_root = tmp.path().join("outer/inner");
    std::fs::create_dir_all(&nested_root).expect("nested");
    std::fs::create_dir(tmp.path().join("outer/.git")).expect("outer git");
    match PersonalFileService::admit_read_write(&nested_root) {
        Err(PersonalFileError::Refused(PersonalFileRefusal::GitRepository { .. })) => {}
        other => panic!("root inside a repo must refuse admission, got {other:?}"),
    }

    // A nested repository INSIDE an admitted root: mutations under it
    // are refused at operation time, before any mutation; the rest of
    // the root keeps working.
    let clean = tempfile::tempdir_in(tmp.path()).expect("clean dir");
    let (service, root) = service_with_rw_root(clean.path());
    std::fs::create_dir_all(clean.path().join("vault/.git")).expect("nested git");
    let inside = PersonalRelativePath::parse("vault/notes.txt").expect("path");
    expect_refused(
        "create under nested repo",
        service.create_text_no_clobber(&root, &inside, "x").await,
        |refusal| matches!(refusal, PersonalFileRefusal::GitRepository { .. }),
    )
    .await;
    expect_refused(
        "delete under nested repo",
        service.delete_to_trash(&root, &inside).await,
        |refusal| matches!(refusal, PersonalFileRefusal::GitRepository { .. }),
    )
    .await;
    // a directory that is itself a repository root refuses deletion
    let vault_dir = PersonalRelativePath::parse("vault").expect("path");
    expect_refused(
        "delete of a repo directory",
        service.delete_to_trash(&root, &vault_dir).await,
        |refusal| matches!(refusal, PersonalFileRefusal::GitRepository { .. }),
    )
    .await;

    // .git itself is unreachable as a path component (type boundary).
    assert!(matches!(
        PersonalRelativePath::parse(".git/config"),
        Err(PersonalFileError::Refused(
            PersonalFileRefusal::GitMetadataPath { .. }
        ))
    ));
    // plain reads keep working next to the refused subtree
    std::fs::write(clean.path().join("plain.txt"), "readable").expect("plain file");
    assert_eq!(
        read_text_of(&service, &root, "plain.txt")
            .await
            .expect("read"),
        "readable"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn create_is_no_clobber() {
    let _fs_serialized = fs_test_guard().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, root) = service_with_rw_root(tmp.path());

    let path = PersonalRelativePath::parse("doc.txt").expect("path");
    service
        .create_text_no_clobber(&root, &path, "first")
        .await
        .expect("first create");
    match service.create_text_no_clobber(&root, &path, "second").await {
        Err(PersonalFileError::AlreadyExists(_)) => {}
        other => panic!("second create must be a typed conflict, got {other:?}"),
    }
    assert_eq!(
        read_text_of(&service, &root, "doc.txt")
            .await
            .expect("read"),
        "first",
        "the original content must be intact"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn replace_requires_matching_expected_identity() {
    let _fs_serialized = fs_test_guard().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, root) = service_with_rw_root(tmp.path());

    let path = PersonalRelativePath::parse("doc.txt").expect("path");
    service
        .create_text_no_clobber(&root, &path, "version one")
        .await
        .expect("create");

    // Wrong expected identity -> typed conflict, zero mutation.
    let wrong = ExpectedContentIdentity::of_content(b"something else");
    match service
        .replace_text_if_expected(&root, &path, &wrong, "version two")
        .await
    {
        Err(PersonalFileError::Conflict { expected, actual }) => {
            assert_eq!(expected, wrong.as_hex());
            assert_eq!(
                actual,
                ExpectedContentIdentity::of_content(b"version one").as_hex()
            );
        }
        other => panic!("mismatched identity must conflict, got {other:?}"),
    }
    assert_eq!(
        read_text_of(&service, &root, "doc.txt")
            .await
            .expect("read"),
        "version one",
        "a conflicted replace must not mutate"
    );

    // Missing file -> typed not-found, not a conflict.
    let missing = PersonalRelativePath::parse("absent.txt").expect("path");
    match service
        .replace_text_if_expected(&root, &missing, &wrong, "x")
        .await
    {
        Err(PersonalFileError::NotFound(_)) => {}
        other => panic!("missing leaf must be not-found, got {other:?}"),
    }

    // Matching identity -> atomic replace with recoverable prior content.
    let identity = ExpectedContentIdentity::of_content(b"version one");
    let replaced = service
        .replace_text_if_expected(&root, &path, &identity, "version two")
        .await
        .expect("replace");
    assert!(matches!(replaced, PersonalFileResult::Replaced { .. }));
    assert_eq!(
        read_text_of(&service, &root, "doc.txt")
            .await
            .expect("read"),
        "version two"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn same_root_move_is_no_clobber_and_cross_root_is_unsupported() {
    let _fs_serialized = fs_test_guard().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, root) = service_with_rw_root(tmp.path());

    let src = PersonalRelativePath::parse("draft.txt").expect("path");
    service
        .create_text_no_clobber(&root, &src, "move me")
        .await
        .expect("create");

    // same-root move works
    let dst = PersonalRelativePath::parse("renamed.txt").expect("path");
    let moved = service
        .move_no_clobber(
            MoveSource {
                root: &root,
                path: &src,
            },
            MoveDestination {
                root: &root,
                path: &dst,
            },
        )
        .await
        .expect("move");
    assert!(matches!(moved, PersonalFileResult::Moved));
    assert!(matches!(
        service.read_text(&root, &src).await,
        Err(PersonalFileError::NotFound(_))
    ));
    assert_eq!(
        read_text_of(&service, &root, "renamed.txt")
            .await
            .expect("read"),
        "move me"
    );

    // destination no-clobber
    service
        .create_text_no_clobber(&root, &src, "another draft")
        .await
        .expect("create again");
    match service
        .move_no_clobber(
            MoveSource {
                root: &root,
                path: &src,
            },
            MoveDestination {
                root: &root,
                path: &dst,
            },
        )
        .await
    {
        Err(PersonalFileError::AlreadyExists(_)) => {}
        other => panic!("move onto an existing leaf must conflict, got {other:?}"),
    }
    assert_eq!(
        read_text_of(&service, &root, "renamed.txt")
            .await
            .expect("read"),
        "move me",
        "the destination must be intact after a refused move"
    );

    // a refused move (missing source) leaves NO freshly created
    // destination parents behind (no litter law)
    let absent = PersonalRelativePath::parse("does-not-exist.txt").expect("path");
    let deep_dst = PersonalRelativePath::parse("new/a/final.txt").expect("path");
    match service
        .move_no_clobber(
            MoveSource {
                root: &root,
                path: &absent,
            },
            MoveDestination {
                root: &root,
                path: &deep_dst,
            },
        )
        .await
    {
        Err(PersonalFileError::NotFound(_)) => {}
        other => panic!("missing source must be not-found, got {other:?}"),
    }
    assert!(
        !tmp.path().join("new").exists(),
        "a refused move must not create destination parents"
    );

    // cross-root move is typed unsupported
    let tmp2 = tempfile::tempdir().expect("tempdir 2");
    let (_service2, root2) = service_with_rw_root(tmp2.path());
    let elsewhere = PersonalRelativePath::parse("elsewhere.txt").expect("path");
    match service
        .move_no_clobber(
            MoveSource {
                root: &root,
                path: &src,
            },
            MoveDestination {
                root: &root2,
                path: &elsewhere,
            },
        )
        .await
    {
        Err(PersonalFileError::UnsupportedSafely(_)) => {}
        other => panic!("cross-root move must be unsupported_safely, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn trash_is_hidden_from_ordinary_listing_and_search() {
    let _fs_serialized = fs_test_guard().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, root) = service_with_rw_root(tmp.path());

    for name in ["keep-a.txt", "keep-b.txt", "drop.txt"] {
        service
            .create_text_no_clobber(
                &root,
                &PersonalRelativePath::parse(name).expect("path"),
                "x",
            )
            .await
            .expect("create");
    }
    let dropped = PersonalRelativePath::parse("drop.txt").expect("path");
    service
        .delete_to_trash(&root, &dropped)
        .await
        .expect("trash");

    // The reserved namespace never appears in ordinary listing, even
    // though it physically exists inside the root.
    let entries = match service.list(&root, None, 100).await.expect("list") {
        PersonalFileResult::Listed { entries } => entries,
        other => panic!("unexpected result class: {:?}", other.operation()),
    };
    let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert!(names.contains(&"keep-a.txt"));
    assert!(names.contains(&"keep-b.txt"));
    assert!(!names.contains(&"drop.txt"));
    assert!(
        !names.iter().any(|name| name.contains(TRASH_NAMESPACE)),
        "the trash namespace must be invisible to ordinary listing"
    );

    // Repeated deletion of the same name: each lands in its own fresh
    // slot — deterministic, never clobbering.
    let mut trash_locations = Vec::new();
    for round in 0..2 {
        service
            .create_text_no_clobber(&root, &dropped, &format!("round {round}"))
            .await
            .expect("recreate");
        match service
            .delete_to_trash(&root, &dropped)
            .await
            .expect("trash")
        {
            PersonalFileResult::Trashed { trash_location } => trash_locations.push(trash_location),
            other => panic!("unexpected result class: {:?}", other.operation()),
        }
    }
    assert_ne!(trash_locations[0], trash_locations[1]);
    for (round, location) in trash_locations.iter().enumerate() {
        assert_eq!(
            std::fs::read(tmp.path().join(location)).expect("trash copy"),
            format!("round {round}").into_bytes(),
        );
    }

    // The namespace itself is unreachable as a user path.
    assert!(matches!(
        PersonalRelativePath::parse(TRASH_NAMESPACE),
        Err(PersonalFileError::Refused(
            PersonalFileRefusal::ReservedNamespace { .. }
        ))
    ));
    assert!(matches!(
        PersonalRelativePath::parse("x/.zeroclaw-trash/y"),
        Err(PersonalFileError::Refused(
            PersonalFileRefusal::ReservedNamespace { .. }
        ))
    ));

    // over-bound listing answers the bound typed rather than truncating
    match service.list(&root, None, 1).await {
        Err(PersonalFileError::TooManyEntries(1)) => {}
        other => panic!("over-bound listing must be typed, got {other:?}"),
    }
}

#[test]
fn unsupported_platform_safety_fails_closed() {
    // On every platform the gate is explicit: either descriptor-bound
    // containment is supported (Unix) and the kernel is live, or
    // admission itself answers unsupported_safely and nothing can ever
    // fall back to string containment.
    if cfg!(unix) {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            PersonalFileService::admit_read_write(tmp.path()).is_ok(),
            "supported platform must admit roots"
        );
        assert!(
            PersonalFileService::admit_read_only(tmp.path()).is_ok(),
            "supported platform must admit read-only roots"
        );
    } else {
        let tmp = tempfile::tempdir().expect("tempdir");
        for admit in [
            PersonalFileService::admit_read_write(tmp.path()),
            PersonalFileService::admit_read_only(tmp.path()),
        ] {
            match admit {
                Err(PersonalFileError::UnsupportedSafely(_)) => {}
                other => panic!("unsupported platform must fail closed, got {other:?}"),
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn read_failure_is_not_reported_as_empty() {
    let _fs_serialized = fs_test_guard().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (service, root) = service_with_rw_root(tmp.path());

    // missing -> typed not-found, never an empty read
    let missing = PersonalRelativePath::parse("absent.txt").expect("path");
    match service.read_text(&root, &missing).await {
        Err(PersonalFileError::NotFound(_)) => {}
        other => panic!("missing file must be not-found, got {other:?}"),
    }

    // a directory at the leaf -> typed refusal, never an empty read
    std::fs::create_dir(tmp.path().join("folder")).expect("dir");
    let folder = PersonalRelativePath::parse("folder").expect("path");
    expect_refused(
        "read of a directory",
        service.read_text(&root, &folder).await,
        |refusal| matches!(refusal, PersonalFileRefusal::NotRegularFile { .. }),
    )
    .await;

    // a genuinely empty file IS an empty read (the file exists and is
    // readable) — the two cases above must stay distinct from this one
    let empty = PersonalRelativePath::parse("empty.txt").expect("path");
    service
        .create_text_no_clobber(&root, &empty, "")
        .await
        .expect("create empty");
    match service.read_text(&root, &empty).await.expect("read") {
        PersonalFileResult::ReadText { text, .. } => assert_eq!(text, ""),
        other => panic!("unexpected result class: {:?}", other.operation()),
    }

    // binary content -> typed unsupported, never corrupt or empty text
    std::fs::write(tmp.path().join("blob.bin"), [0xFF, 0xFE, 0x00, 0xD8]).expect("binary");
    let blob = PersonalRelativePath::parse("blob.bin").expect("path");
    match service.read_text(&root, &blob).await {
        Err(PersonalFileError::NotText(_)) => {}
        other => panic!("binary content must be typed unsupported, got {other:?}"),
    }

    // over-bound content -> typed bound error, never a truncation
    std::fs::write(
        tmp.path().join("huge.txt"),
        vec![b'a'; (MAX_TEXT_BYTES + 1) as usize],
    )
    .expect("big file");
    let big = PersonalRelativePath::parse("huge.txt").expect("path");
    match service.read_text(&root, &big).await {
        Err(PersonalFileError::TooLarge { .. }) => {}
        other => panic!("over-bound read must be typed, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn staged_cleanup_leaves_foreign_files_alone() {
    let _fs_serialized = fs_test_guard().await;
    use crate::personal_file::safety;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = PersonalFileService::admit_read_write(tmp.path()).expect("admit");
    let dir = rustix::io::dup(&root.inner.dir).expect("dup root fd");

    // Our staged object, dropped exactly like the publication flow.
    let staged_name = format!(".victim.{}", uuid::Uuid::new_v4());
    let (_file, identity) = safety::write_staged_file(&dir, &staged_name, b"ours").expect("stage");

    // An attacker swaps the name for a foreign file before cleanup.
    std::fs::remove_file(tmp.path().join(&staged_name)).expect("swap away ours");
    std::fs::write(tmp.path().join(&staged_name), b"foreign").expect("plant foreign");

    safety::remove_staged(&dir, &staged_name, &identity);
    assert_eq!(
        std::fs::read(tmp.path().join(&staged_name)).expect("foreign file survives"),
        b"foreign",
        "cleanup must never remove a name that no longer names our object"
    );

    // The positive direction: cleanup removes our own staged object
    // while the name still names it.
    let our_name = format!(".victim.{}", uuid::Uuid::new_v4());
    let (_file2, our_identity) =
        safety::write_staged_file(&dir, &our_name, b"ours-2").expect("stage 2");
    safety::remove_staged(&dir, &our_name, &our_identity);
    assert!(
        !tmp.path().join(&our_name).exists(),
        "cleanup removes our own staged object"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Read-only roots
// ─────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn read_only_root_refuses_mutation_but_allows_reads() {
    let _fs_serialized = fs_test_guard().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("source.txt"), "source").expect("file");
    let root = PersonalFileService::admit_read_only(tmp.path()).expect("admit ro");
    let service = PersonalFileService::new(vec![], vec![root.clone()]).expect("service");

    assert_eq!(
        read_text_of(&service, &root, "source.txt")
            .await
            .expect("read"),
        "source"
    );
    let path = PersonalRelativePath::parse("source.txt").expect("path");
    expect_refused(
        "create through read-only root",
        service.create_text_no_clobber(&root, &path, "x").await,
        |refusal| matches!(refusal, PersonalFileRefusal::ReadOnlyRoot),
    )
    .await;
    expect_refused(
        "delete through read-only root",
        service.delete_to_trash(&root, &path).await,
        |refusal| matches!(refusal, PersonalFileRefusal::ReadOnlyRoot),
    )
    .await;

    // a read-only root cannot be smuggled in as a write root
    match PersonalFileService::new(vec![root.clone()], vec![]) {
        Err(PersonalFileError::Refused(PersonalFileRefusal::RootKindMismatch { .. })) => {}
        other => panic!("kind mismatch must be typed, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Static/negative capability scans
// ─────────────────────────────────────────────────────────────────────

/// The kernel module must have no dependency/call path expression to
/// process spawn, shell, git, build/test runners, network, provider key
/// material, Tachi submission, or tool registration. Lexical per-file
/// law (the execution-subagent discipline), not a reachability proof.
#[test]
fn module_source_scans_hold() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let files = [
        "personal_file/mod.rs",
        "personal_file/domain.rs",
        "personal_file/safety.rs",
        "personal_file/service.rs",
        "personal_file/tests.rs",
    ];
    // Assembled at runtime so this scan does not itself read as a
    // capability site to scanners over this file.
    let bans = [
        ["std::", "process"].join(""),
        ["tokio::", "process"].join(""),
        ["process", "::Command"].join(""),
        ["Command", "::new"].join(""),
        ["std::", "net"].join(""),
        ["tokio::", "net"].join(""),
        ["git", "2::"].join(""),
        ["cargo ", "run"].join(""),
        ["api_", "key"].join(""),
        ["cre", "dential"].join(""),
        ["tachi", "_bridge"].join(""),
        ["Tachi", "TaskBridge"].join(""),
        ["async_", "trait"].join(""),
        ["::tool", "::Tool"].join(""),
        ["register", "_tool"].join(""),
    ];
    for file in files {
        let source = std::fs::read_to_string(format!("{manifest_dir}/src/{file}"))
            .unwrap_or_else(|error| panic!("read {file}: {error}"));
        for banned in &bans {
            assert!(
                !source.contains(banned.as_str()),
                "{file} must not contain {banned:?} (capability-ban law)"
            );
        }
    }
}

/// R2 finding #2 and the additive law: nothing anywhere registers a
/// `personal_file` tool. The minimal membership table, the runtime tool
/// assembly, and the Reasoning/Supervisor exact-capability surfaces must
/// not name the capability (LA1 is model-unregistered; LA2/LA3 own any
/// later wiring).
#[test]
fn personal_file_is_registered_nowhere() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let repo_root = Path::new(manifest_dir)
        .parent()
        .and_then(|path| path.parent())
        .expect("repo root");
    let surfaces = [
        "crates/zeroclaw-config/src/composition.rs",
        "crates/zeroclaw-runtime/src/tools/mod.rs",
        "crates/zeroclaw-runtime/src/subagent_v1/mod.rs",
        "crates/zeroclaw-runtime/src/supervisor_v1/mod.rs",
    ];
    let token = ["personal", "_file"].join("");
    for surface in surfaces {
        let source = std::fs::read_to_string(repo_root.join(surface))
            .unwrap_or_else(|error| panic!("read {surface}: {error}"));
        assert!(
            !source.contains(&token),
            "{surface} must not reference the {token} capability (model-unregistered law)"
        );
    }
}
