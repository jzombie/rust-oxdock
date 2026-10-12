#[allow(clippy::disallowed_types)]
use oxdock_fs::UnguardedPath;
use oxdock_fs::{
    GuardPolicy, GuardedPath, PolicyPath, discover_workspace_root, normalized_path,
    to_forward_slashes,
};
use oxdock_sys_test_utils::TestEnvGuard;

#[cfg_attr(
    miri,
    ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
)]
#[test]
fn guarded_path_join_parent_and_display() {
    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    let nested = root.join("a/b").expect("join");
    let parent = nested.parent().expect("parent");
    assert_eq!(parent, root.join("a").expect("parent join"));
    let display = nested.display();
    assert!(!display.is_empty());
}

#[cfg_attr(
    miri,
    ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
)]
#[test]
fn policy_path_accessors_and_policy() {
    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let guarded = tempdir.as_guarded_path().clone();
    #[allow(clippy::disallowed_types)]
    let unguarded = UnguardedPath::external(guarded.as_path());
    let guarded_policy = PolicyPath::from(guarded.clone());
    let unguarded_policy = PolicyPath::from(unguarded.clone());
    assert_eq!(guarded_policy.policy(), GuardPolicy::Guarded);
    assert_eq!(unguarded_policy.policy(), GuardPolicy::Unguarded);
    assert_eq!(guarded_policy.as_guarded(), Some(&guarded));
    assert_eq!(unguarded_policy.as_unguarded(), Some(&unguarded));
}

#[cfg_attr(
    miri,
    ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
)]
#[test]
fn forward_slash_helpers_normalize_paths() {
    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    let child = root.join("a/b").expect("join");
    let embedded = normalized_path(&child);
    assert!(!embedded.contains('\\'));
    assert_eq!(to_forward_slashes("a\\b"), "a/b");
}

#[cfg_attr(
    miri,
    ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
)]
#[test]
fn discover_workspace_root_prefers_env() {
    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    let _guard = TestEnvGuard::set("OXDOCK_WORKSPACE_ROOT", root.display().as_str());
    let discovered = discover_workspace_root().expect("discover");
    assert_eq!(discovered.as_path(), root.as_path());
}

#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
)]
#[test]
fn nofollow_opens_refuse_planted_links() {
    use oxdock_fs::PathResolver;

    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    if !oxdock_sys_test_utils::can_create_symlinks(root.as_path()) {
        return;
    }
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
    let target = root.join("target.txt").expect("target");
    resolver.write_file(&target, b"secret").expect("write");
    let link = root.join("link.txt").expect("link");
    resolver.symlink(&target, &link).expect("symlink");
    // Plain files open normally through the nofollow path.
    let mut contents = Vec::new();
    std::io::Read::read_to_end(
        &mut resolver
            .open_read_nofollow(&target, &root)
            .expect("read open"),
        &mut contents,
    )
    .expect("read");
    assert_eq!(contents, b"secret");
    // A planted final-component link fails instead of redirecting.
    assert!(
        resolver.open_read_nofollow(&link, &root).is_err(),
        "link read must refuse"
    );
    assert!(
        resolver.open_write_nofollow(&link, &root).is_err(),
        "link write must refuse"
    );
    let kept = resolver.read_file(&target).expect("reread");
    assert_eq!(kept, b"secret", "link target must survive untouched");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg_attr(
    miri,
    ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
)]
#[test]
fn nofollow_opens_verify_descriptor_tree() {
    // A parent link redirects beneath the expected root: the `lstat`
    // layers pass (final is fresh), but the descriptor's own path
    // misses the root and the open fails. Without descriptor
    // verification this opens fine, so the test pins the new wall.
    use oxdock_fs::PathResolver;

    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    if !oxdock_sys_test_utils::can_create_symlinks(root.as_path()) {
        return;
    }
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
    let out = root.join("out").expect("out");
    resolver.create_dir_all(&out).expect("mkdir out");
    let shadow = root.join("shadow").expect("shadow");
    resolver.create_dir_all(&shadow).expect("mkdir shadow");
    let shadow_file = shadow.join("file.txt").expect("shadow file");
    resolver.write_file(&shadow_file, b"shadow").expect("write");
    let sub = out.join("sub").expect("sub");
    resolver.symlink(&shadow, &sub).expect("plant parent link");
    let through = sub.join("file.txt").expect("through path");
    assert!(
        resolver.open_read_nofollow(&through, &out).is_err(),
        "parent-redirected read must refuse"
    );
    let fresh = sub.join("new.txt").expect("fresh path");
    assert!(
        resolver.open_write_nofollow(&fresh, &out).is_err(),
        "parent-redirected write must refuse"
    );
    assert!(
        !shadow.join("new.txt").expect("shadow new").exists(),
        "rolled-back file must be gone"
    );
    // A pre-existing redirect target must survive byte-identical:
    // no truncate runs before verification, and rollback only
    // removes files this open created.
    let victim = shadow.join("victim.txt").expect("victim");
    resolver
        .write_file(&victim, b"precious")
        .expect("write victim");
    let through_victim = sub.join("victim.txt").expect("through victim");
    assert!(
        resolver.open_write_nofollow(&through_victim, &out).is_err(),
        "redirected write to existing file must refuse"
    );
    let kept = resolver.read_file(&victim).expect("reread victim");
    assert_eq!(kept, b"precious", "redirect target must survive intact");
}

#[cfg(target_os = "macos")]
#[cfg_attr(
    miri,
    ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
)]
#[test]
fn descriptor_verify_folds_case() {
    // `realpath` preserves caller spelling while `F_GETPATH` reports
    // on-disk case. Guarding the spelling before the directory exists
    // keeps its lexical (divergent) form; creating it lowercase after
    // makes the descriptor report on-disk case. Exact compare rejects
    // this one file spelled two ways; folding accepts. On
    // case-sensitive volumes the divergent spelling addresses a
    // distinct directory, so the open verifies trivially: no skip
    // needed, nothing false to catch there.
    use oxdock_fs::PathResolver;

    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
    let upper = root.join("CASEFOLD").expect("upper");
    let lower = root.join("casefold").expect("lower");
    resolver.create_dir_all(&lower).expect("mkdir");
    let file = upper.join("f.txt").expect("file");
    let mut handle = resolver
        .open_write_nofollow(&file, &upper)
        .expect("divergent-case open must verify");
    std::io::Write::write_all(&mut handle, b"folded").expect("write");
    drop(handle);
    let back = resolver.read_file(&file).expect("reread");
    assert_eq!(back, b"folded");
}

#[cfg_attr(
    miri,
    ignore = "GuardedPath::tempdir relies on OS tempdirs; blocked under Miri isolation"
)]
#[test]
fn refuse_link_ancestors_rejects_outside_member() {
    // Purely lexical gate, no filesystem access: a member outside the
    // base bails before any walk, so traversal can never reach system
    // directories no matter the path formatting.
    use oxdock_fs::PathResolver;

    let tempdir = GuardedPath::tempdir().expect("tempdir");
    let root = tempdir.as_guarded_path().clone();
    let base = root.join("base").expect("base");
    let elsewhere = root.join("elsewhere").expect("elsewhere");
    assert!(
        PathResolver::refuse_link_ancestors(&base, &elsewhere).is_err(),
        "outside member must bail before walking"
    );
}

#[cfg(feature = "mock-fs")]
#[test]
fn mock_fs_writes_and_reads_files() {
    use oxdock_fs::{EntryKind, MockFs, WorkspaceFs};

    let fs = MockFs::new();
    let root = fs.root().clone();
    let file = root.join("dir/file.txt").expect("file path");
    fs.ensure_parent_dir(&file).expect("ensure parent");
    fs.write_file(&file, b"hello").expect("write");
    let contents = fs.read_to_string(&file).expect("read");
    assert_eq!(contents, "hello");
    let dir = root.join("dir").expect("dir path");
    assert_eq!(fs.entry_kind(&dir).expect("dir kind"), EntryKind::Dir);
    assert_eq!(fs.entry_kind(&file).expect("file kind"), EntryKind::File);
}

#[cfg(feature = "mock-fs")]
#[test]
fn mock_fs_resolves_relative_paths() {
    use oxdock_fs::{MockFs, WorkspaceFs};

    let fs = MockFs::new();
    let root = fs.root().clone();
    let nested = fs.resolve_workdir(&root, "a/b").expect("resolve");
    let expected = root.join("a/b").expect("expected");
    assert_eq!(nested, expected);
    let parent = fs.resolve_workdir(&nested, "..").expect("parent");
    assert_eq!(parent, root.join("a").expect("parent"));
}
