use anyhow::{Context, Result, anyhow};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::path::{Path, PathBuf};

use super::{AccessMode, PathResolver, to_forward_slashes};
use crate::GuardedPath;

// Path resolution helpers (WORKDIR, READ/WRITE, COPY sources).
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl PathResolver {
    /// Strip an absolute or rooted path down to its root-relative form:
    /// root/prefix markers drop, `.` drops, `..` survives so the guard
    /// still rejects escapes after re-anchoring. Shared by the
    /// absolute-fallback arms and by host callers (e.g. plugin option
    /// paths) that anchor user strings to the workspace root.
    pub fn root_relative_path(path: &Path) -> PathBuf {
        let mut rel = PathBuf::new();
        for comp in path.components() {
            match comp {
                std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => rel.push(".."),
                std::path::Component::Normal(seg) => rel.push(seg),
            }
        }
        rel
    }

    /// Whether `path` is absolute (or Windows-rooted): the shapes the
    /// absolute-fallback arms rebase under the workspace root instead of
    /// joining onto the cwd.
    #[allow(clippy::disallowed_macros)]
    pub fn is_absolute_or_rooted(path: &Path) -> bool {
        path.is_absolute()
            || (cfg!(windows) && path.components().next() == Some(std::path::Component::RootDir))
    }

    pub fn resolve_workdir(&self, current: &GuardedPath, new_dir: &str) -> Result<GuardedPath> {
        if new_dir == "/" {
            // Reset to the resolver root when WORKDIR is set to '/'. Pure
            // selection, no I/O: a pending anchor is returned as-is and the
            // first snapshot-targeted use materializes through the choke below.
            return Ok(self.root().clone());
        }
        // Choke point (issue #131): snapshot-targeted resolution materializes
        // first, so every join/guard below operates on a concrete root.
        // Local callers pass through untouched.
        let current = self.rebased_cwd(current)?;
        let new_dir_path = Path::new(new_dir);
        if Self::is_absolute_or_rooted(new_dir_path) {
            if let Ok(resolved) = self.check_access(new_dir_path, AccessMode::Write) {
                return self.backend.resolve_workdir(resolved);
            }

            let rel = Self::root_relative_path(new_dir_path);
            let candidate = self.root().as_path().join(rel);
            let resolved = self
                .check_access(&candidate, AccessMode::Write)
                .with_context(|| format!("WORKDIR {} escapes root", candidate.display()))?;
            return self.backend.resolve_workdir(resolved);
        }

        let candidate = current.as_path().join(new_dir);
        let resolved = self
            .check_access(&candidate, AccessMode::Write)
            .with_context(|| format!("WORKDIR {} escapes root", candidate.display()))?;
        self.backend.resolve_workdir(resolved)
    }

    pub fn resolve_read(&self, cwd: &GuardedPath, rel: &str) -> Result<GuardedPath> {
        self.resolve(cwd, rel, AccessMode::Read)
    }

    pub fn resolve_write(&self, cwd: &GuardedPath, rel: &str) -> Result<GuardedPath> {
        self.resolve(cwd, rel, AccessMode::Write)
    }

    pub fn resolve_copy_source(&self, from: &str) -> Result<GuardedPath> {
        let from_path = Path::new(from);
        if Self::is_absolute_or_rooted(from_path) {
            if let Some(workspace_root) = &self.workspace_root
                && let Ok(guarded) =
                    self.check_access_with_root(workspace_root, from_path, AccessMode::Read)
            {
                return self.backend.resolve_copy_source(guarded);
            }
            if let Ok(guarded) =
                self.check_access_with_root(&self.build_context, from_path, AccessMode::Read)
            {
                return self.backend.resolve_copy_source(guarded);
            }

            let rel = Self::root_relative_path(from_path);
            let root = self.workspace_root.as_ref().unwrap_or(&self.build_context);
            let candidate = root.as_path().join(rel);
            return self
                .check_access_with_root(root, &candidate, AccessMode::Read)
                .with_context(|| format!("failed to resolve COPY source {}", candidate.display()))
                .and_then(|guarded| self.backend.resolve_copy_source(guarded));
        }
        let candidate = self.build_context.as_path().join(from);

        match self
            .check_access_with_root(&self.build_context, &candidate, AccessMode::Read)
            .with_context(|| format!("failed to resolve COPY source {}", candidate.display()))
        {
            Ok(guarded) => self.backend.resolve_copy_source(guarded),
            Err(primary) => {
                if let Some(workspace_root) = &self.workspace_root {
                    let workspace_candidate = workspace_root.as_path().join(from);
                    if let Ok(workspace_guarded) = self.check_access_with_root(
                        workspace_root,
                        &workspace_candidate,
                        AccessMode::Read,
                    ) {
                        return self.backend.resolve_copy_source(workspace_guarded);
                    }
                }
                Err(primary)
            }
        }
    }

    pub fn resolve_copy_source_from_workspace(&self, from: &str) -> Result<GuardedPath> {
        let workspace_root = self
            .workspace_root
            .as_ref()
            .ok_or_else(|| anyhow!("no workspace root set for workspace-relative COPY"))?;

        let from_path = Path::new(from);
        if Self::is_absolute_or_rooted(from_path) {
            return self
                .check_access_with_root(workspace_root, from_path, AccessMode::Read)
                .with_context(|| format!("failed to resolve COPY source {}", from_path.display()))
                .and_then(|guarded| self.backend.resolve_copy_source(guarded));
        }

        let candidate = workspace_root.as_path().join(from);
        self.check_access_with_root(workspace_root, &candidate, AccessMode::Read)
            .with_context(|| format!("failed to resolve COPY source {}", candidate.display()))
            .and_then(|guarded| self.backend.resolve_copy_source(guarded))
    }

    fn resolve(&self, cwd: &GuardedPath, rel: &str, mode: AccessMode) -> Result<GuardedPath> {
        // Normalize backslashes to forward slashes before path construction
        let normalized = to_forward_slashes(rel);
        // Choke point (issue #131): snapshot-targeted resolution materializes
        // first, so every join/guard below operates on a concrete root.
        // Local callers (and COPY sources, which never reach here) pass
        // through untouched.
        let cwd = self.rebased_cwd(cwd)?;
        let rel_path = Path::new(&normalized);
        if Self::is_absolute_or_rooted(rel_path) {
            if let Ok(guarded) = self.check_access(rel_path, mode) {
                return Ok(guarded);
            }
            let rel = Self::root_relative_path(rel_path);
            let candidate = self.root().as_path().join(rel);
            return self.check_access(&candidate, mode);
        }

        let candidate = cwd.as_path().join(rel_path);
        self.check_access(&candidate, mode)
    }

    /// Parse a string that may originate from an environment variable or
    /// external input and resolve it to a guarded path. This normalizes common
    /// forms such as `file://` prefixes, surrounding quotes, and backslashes
    /// before delegating to the resolver's `resolve_read` logic.
    pub fn parse_env_path(&self, cwd: &GuardedPath, input: &str) -> Result<GuardedPath> {
        let mut s = input.trim();
        // Strip surrounding quotes if present
        if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
            s = &s[1..s.len() - 1];
        }

        // Strip common file:// URI prefix
        if let Some(stripped) = s.strip_prefix("file:///") {
            s = stripped;
        } else if let Some(stripped) = s.strip_prefix("file://") {
            s = stripped;
        }

        // Normalize backslashes to forward slashes using the shared helper.
        let normalized = to_forward_slashes(s);

        // Delegate to resolve_read which handles absolute vs relative and
        // workspace/build-context fallbacks.
        self.resolve_read(cwd, &normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GuardedPath;

    /// Lazy resolver (issue #131): snapshot-targeted `resolve_write` creates
    /// the directory exactly once; local-selected resolution creates nothing.
    #[test]
    fn lazy_resolver_materializes_exactly_once_on_snapshot_write() {
        let local_temp = GuardedPath::tempdir().unwrap();
        let local = local_temp.as_guarded_path().clone();
        let resolver = PathResolver::new_lazy(local.clone()).unwrap();
        let handle = resolver.snapshot_handle();
        assert!(!handle.is_materialized());

        let cwd = resolver.root().clone();
        let first = resolver.resolve_write(&cwd, "note.txt").unwrap();
        assert!(handle.is_materialized());
        #[cfg(not(miri))]
        assert!(
            first.root().exists(),
            "materialized snapshot root must exist on disk"
        );
        let second = resolver.resolve_write(&cwd, "other.txt").unwrap();
        assert_eq!(
            first.root(),
            second.root(),
            "one physical snapshot dir per resolver"
        );
        assert!(second.as_path().starts_with(first.root()));
    }

    /// `WORKSPACE LOCAL` selection: resolution stays local with zero snapshot
    /// allocation, and `root()` reports the build context.
    #[test]
    fn lazy_resolver_local_selection_creates_no_snapshot() {
        let local_temp = GuardedPath::tempdir().unwrap();
        let local = local_temp.as_guarded_path().clone();
        let mut resolver = PathResolver::new_lazy(local.clone()).unwrap();
        resolver.switch_to_local();
        let handle = resolver.snapshot_handle();
        assert_eq!(resolver.root(), &local);
        let cwd = resolver.root().clone();
        let target = resolver.resolve_write(&cwd, "out.txt").unwrap();
        assert!(!handle.is_materialized());
        resolver.write_file(&target, b"hi").unwrap();
        assert!(!handle.is_materialized());
        assert_eq!(target.root(), local.as_path());
    }

    /// Bare snapshot selection materializes nothing: `WORKSPACE SNAPSHOT`
    /// alone only flips selection, and `root()` serves the pending anchor
    /// (display renders the sentinel, never this value).
    #[test]
    fn lazy_resolver_bare_snapshot_selection_creates_nothing() {
        let local_temp = GuardedPath::tempdir().unwrap();
        let local = local_temp.as_guarded_path().clone();
        let resolver = PathResolver::new_lazy(local.clone()).unwrap();
        let handle = resolver.snapshot_handle();
        assert!(resolver.is_snapshot_pending());
        assert!(!handle.is_materialized());
        assert_eq!(resolver.root().root(), resolver.root().as_path());
        // On-disk absence, not just flag state: the pending anchor is a
        // deterministic path, so its non-existence is directly observable.
        // (`GuardedPath::exists()` would route through `guard_path` and
        // create the anchor it checks, so stat the raw path instead.)
        #[cfg(not(miri))]
        assert!(
            !resolver.root().as_path().exists(),
            "pending anchor must not exist on disk"
        );
    }

    /// Cross-clone convergence (async-fork coherence, issue #131): a fork
    /// clone created BEFORE materialization observes the origin's directory
    /// afterwards through the shared publication. No per-clone remapping
    /// step, no second directory.
    #[test]
    fn lazy_resolver_fork_clone_converges_onto_shared_concrete_root() {
        let local_temp = GuardedPath::tempdir().unwrap();
        let local = local_temp.as_guarded_path().clone();
        let resolver = PathResolver::new_lazy(local.clone()).unwrap();
        let fork = resolver.clone();
        assert!(fork.is_snapshot_pending());

        let cwd = resolver.root().clone();
        let target = resolver.resolve_write(&cwd, "a.txt").unwrap();
        resolver.write_file(&target, b"a").unwrap();

        assert!(!fork.is_snapshot_pending());
        assert_eq!(fork.root(), resolver.root());
        let fork_cwd = fork.root().clone();
        let fork_target = fork.resolve_read(&fork_cwd, "a.txt").unwrap();
        assert_eq!(fork_target.as_path(), target.as_path());
    }

    /// Concurrent first touches serialize onto a single directory: both
    /// threads observe the same concrete root.
    #[test]
    fn lazy_resolver_concurrent_first_touch_shares_one_directory() {
        use std::sync::{Arc, Barrier};
        let local_temp = GuardedPath::tempdir().unwrap();
        let local = local_temp.as_guarded_path().clone();
        let resolver = Arc::new(PathResolver::new_lazy(local.clone()).unwrap());
        let barrier = Arc::new(Barrier::new(2));
        let cwd = resolver.root().clone();

        let worker =
            |resolver: Arc<PathResolver>, barrier: Arc<Barrier>, cwd: GuardedPath, name: &str| {
                let _ = barrier.wait();
                let target = resolver.resolve_write(&cwd, name).unwrap();
                resolver.write_file(&target, b"x").unwrap();
                target
            };
        let (r1, b1, c1) = (Arc::clone(&resolver), Arc::clone(&barrier), cwd.clone());
        let (r2, b2, c2) = (Arc::clone(&resolver), Arc::clone(&barrier), cwd);
        let t1 = std::thread::spawn(move || worker(r1, b1, c1, "t1.txt"));
        let t2 = std::thread::spawn(move || worker(r2, b2, c2, "t2.txt"));
        let p1 = t1.join().expect("worker one");
        let p2 = t2.join().expect("worker two");
        assert_eq!(p1.root(), p2.root());
        assert!(resolver.snapshot_handle().is_materialized());
    }

    #[test]
    fn absolute_paths_resolve_under_root() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
        let resolved = resolver.resolve_write(&root, "/client").unwrap();
        assert_eq!(resolved.as_path(), root.as_path().join("client"));
    }

    #[test]
    fn absolute_paths_within_root_are_preserved() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
        let target = root.as_path().join("client").join("from_env.txt");
        let target_str = target.to_string_lossy().to_string();
        let resolved = resolver.resolve_write(&root, &target_str).unwrap();
        assert_eq!(resolved.as_path(), target);
    }

    #[test]
    fn absolute_workdir_within_root_is_preserved() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();
        let target = root.as_path().join("nested").join("dir");
        let target_str = target.to_string_lossy().to_string();
        let resolved = resolver.resolve_workdir(&root, &target_str).unwrap();
        assert_eq!(resolved.as_path(), target);
    }

    #[test]
    fn absolute_copy_source_resolves_under_workspace_root() {
        let snapshot = GuardedPath::tempdir().unwrap();
        let workspace = GuardedPath::tempdir().unwrap();
        let snapshot_root = snapshot.as_guarded_path().clone();
        let workspace_root = workspace.as_guarded_path().clone();
        let mut resolver =
            PathResolver::new_guarded(snapshot_root.clone(), workspace_root.clone()).unwrap();
        resolver.set_workspace_root(workspace_root.clone());
        let workspace_resolver =
            PathResolver::new_guarded(workspace_root.clone(), workspace_root.clone()).unwrap();
        let client = GuardedPath::new(
            workspace_root.root(),
            &workspace_root.as_path().join("client"),
        )
        .unwrap();
        workspace_resolver.create_dir_all(&client).unwrap();
        let resolved = resolver.resolve_copy_source("/client").unwrap();
        assert_eq!(resolved.as_path(), workspace_root.as_path().join("client"));
    }

    #[test]
    fn absolute_copy_source_within_workspace_root_is_preserved() {
        let snapshot = GuardedPath::tempdir().unwrap();
        let workspace = GuardedPath::tempdir().unwrap();
        let snapshot_root = snapshot.as_guarded_path().clone();
        let workspace_root = workspace.as_guarded_path().clone();
        let mut resolver =
            PathResolver::new_guarded(snapshot_root.clone(), workspace_root.clone()).unwrap();
        resolver.set_workspace_root(workspace_root.clone());
        let workspace_resolver =
            PathResolver::new_guarded(workspace_root.clone(), workspace_root.clone()).unwrap();
        let client = GuardedPath::new(
            workspace_root.root(),
            &workspace_root.as_path().join("client"),
        )
        .unwrap();
        workspace_resolver.create_dir_all(&client).unwrap();
        let source = client.join("input.txt").unwrap();
        workspace_resolver
            .write_file(&source, b"preserve path")
            .unwrap();
        let source_str = source.as_path().to_string_lossy().to_string();
        let resolved = resolver.resolve_copy_source(&source_str).unwrap();
        assert_eq!(resolved.as_path(), source.as_path());
    }

    #[test]
    fn workspace_copy_source_absolute_within_root_is_allowed() {
        let snapshot = GuardedPath::tempdir().unwrap();
        let workspace = GuardedPath::tempdir().unwrap();
        let snapshot_root = snapshot.as_guarded_path().clone();
        let workspace_root = workspace.as_guarded_path().clone();
        let mut resolver =
            PathResolver::new_guarded(snapshot_root.clone(), workspace_root.clone()).unwrap();
        resolver.set_workspace_root(workspace_root.clone());

        let workspace_resolver =
            PathResolver::new_guarded(workspace_root.clone(), workspace_root.clone()).unwrap();
        let ws_dir = workspace_root.join("ws").unwrap();
        workspace_resolver.create_dir_all(&ws_dir).unwrap();
        let source = ws_dir.join("input.txt").unwrap();
        workspace_resolver
            .write_file(&source, b"workspace file")
            .unwrap();

        let source_str = source.as_path().to_string_lossy().to_string();
        let resolved = resolver
            .resolve_copy_source_from_workspace(&source_str)
            .unwrap();
        assert_eq!(resolved.as_path(), source.as_path());
    }

    #[test]
    fn workspace_copy_source_relative_within_root_is_allowed() {
        let snapshot = GuardedPath::tempdir().unwrap();
        let workspace = GuardedPath::tempdir().unwrap();
        let snapshot_root = snapshot.as_guarded_path().clone();
        let workspace_root = workspace.as_guarded_path().clone();
        let mut resolver =
            PathResolver::new_guarded(snapshot_root.clone(), workspace_root.clone()).unwrap();
        resolver.set_workspace_root(workspace_root.clone());

        let workspace_resolver =
            PathResolver::new_guarded(workspace_root.clone(), workspace_root.clone()).unwrap();
        let ws_dir = workspace_root.join("ws_rel").unwrap();
        workspace_resolver.create_dir_all(&ws_dir).unwrap();
        let source = ws_dir.join("input.txt").unwrap();
        workspace_resolver
            .write_file(&source, b"workspace rel file")
            .unwrap();

        let resolved = resolver
            .resolve_copy_source_from_workspace("ws_rel/input.txt")
            .unwrap();
        assert_eq!(resolved.as_path(), source.as_path());
    }

    #[test]
    fn workspace_copy_source_cannot_escape_workspace_root() {
        let snapshot = GuardedPath::tempdir().unwrap();
        let workspace = GuardedPath::tempdir().unwrap();
        let snapshot_root = snapshot.as_guarded_path().clone();
        let workspace_root = workspace.as_guarded_path().clone();
        let mut resolver =
            PathResolver::new_guarded(snapshot_root.clone(), workspace_root.clone()).unwrap();
        resolver.set_workspace_root(workspace_root.clone());

        // Absolute path pointing outside the workspace must fail, even if it exists.
        let outside_temp = GuardedPath::tempdir().unwrap();
        let outside_root = outside_temp.as_guarded_path().clone();
        let outside_resolver =
            PathResolver::new_guarded(outside_root.clone(), outside_root.clone()).unwrap();
        let outside_file = outside_root.join("escape.txt").unwrap();
        outside_resolver
            .write_file(&outside_file, b"outside workspace")
            .unwrap();
        let outside_abs = outside_file.as_path().to_string_lossy().to_string();
        let res = resolver.resolve_copy_source_from_workspace(&outside_abs);
        assert!(
            res.is_err(),
            "expected error for absolute path outside workspace root"
        );

        // Relative paths that attempt to traverse above the workspace root must also fail.
        let rel_escape = "../escape.txt";
        let rel_res = resolver.resolve_copy_source_from_workspace(rel_escape);
        assert!(
            rel_res.is_err(),
            "expected error for relative path escaping workspace root"
        );
    }

    #[cfg_attr(
        miri,
        ignore = "creates host files to resolve against; blocked under Miri isolation"
    )]
    #[test]
    fn parse_env_path_normalizes_common_env_var_forms() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();

        let file = root.join("target/data.txt").unwrap();
        resolver.write_file(&file, b"env-form").unwrap();
        let expected = file.as_path().to_string_lossy().to_string();

        let plain = resolver
            .parse_env_path(&root, "target/data.txt")
            .expect("plain relative");
        assert_eq!(plain.as_path().to_string_lossy(), expected);

        for quoted in ["\"target/data.txt\"", "'target/data.txt'"] {
            let resolved = resolver.parse_env_path(&root, quoted).expect("quoted form");
            assert_eq!(resolved.as_path().to_string_lossy(), expected);
        }

        let backslashed = resolver
            .parse_env_path(&root, "target\\data.txt")
            .expect("backslash form");
        assert_eq!(backslashed.as_path().to_string_lossy(), expected);

        let padded = resolver
            .parse_env_path(&root, "   target/data.txt   ")
            .expect("whitespace trimmed");
        assert_eq!(padded.as_path().to_string_lossy(), expected);

        let absolute = format!("file:///{expected}");
        let uri = resolver
            .parse_env_path(&root, &absolute)
            .expect("file:/// URI form");
        assert_eq!(uri.as_path().to_string_lossy(), expected);
    }

    #[test]
    fn parse_env_path_rejects_paths_outside_root() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();

        let escape = resolver.parse_env_path(&root, "../outside.txt");
        assert!(escape.is_err(), "env-supplied escapes must be rejected");
    }

    #[test]
    fn resolve_read_rejects_parent_dir_escapes() {
        // LOAD_TOML / LOAD_JSON ride `resolve_read`: escapes above the root
        // must fail, while `..` that stays contained keeps resolving.
        let temp = GuardedPath::tempdir().unwrap();
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).unwrap();

        assert!(resolver.resolve_read(&root, "../outside.txt").is_err());
        assert!(resolver.resolve_read(&root, "a/../../escape.txt").is_err());

        let expected = root.join("sub").unwrap().join("file.txt").unwrap();
        let resolved = resolver
            .resolve_read(&root, "sub/../sub/file.txt")
            .expect("contained dotdot resolves");
        assert_eq!(resolved.as_path(), expected.as_path());
    }
}
