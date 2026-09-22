use anyhow::{Context, Result, anyhow};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::path::{Path, PathBuf};

use super::{AccessMode, CopySourceRoot, PathResolver, to_forward_slashes};
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
        if self.is_system() {
            return self.resolve_workdir_system(current, new_dir);
        }
        if self.is_cache() {
            self.ensure_cache()?;
        }
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

    /// Unconfined workdir resolution for `WORKSPACE SYSTEM` (issue #163).
    /// `/` resets to the display-only system anchor; absolute candidates
    /// resolve as-is; relative candidates join onto the current directory.
    /// Results anchor at their own filesystem anchor so every drive and
    /// share resolves. The backend still creates the directory when
    /// missing, as with confined roots.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn resolve_workdir_system(&self, current: &GuardedPath, new_dir: &str) -> Result<GuardedPath> {
        if new_dir == "/" {
            return self.backend.resolve_workdir(self.system_anchor.clone());
        }
        let normalized = to_forward_slashes(new_dir);
        let new_dir_path = Path::new(&normalized);
        let base = if current.as_path().is_absolute() {
            current.as_path().to_path_buf()
        } else {
            self.build_context.as_path().join(current.as_path())
        };
        let absolute = if new_dir_path.is_absolute() {
            new_dir_path.to_path_buf()
        } else if Self::is_absolute_or_rooted(new_dir_path) {
            super::cache::system_anchor(&base).join(Self::root_relative_path(new_dir_path))
        } else {
            base.join(new_dir_path)
        };
        let wrapped = super::cache::system_wrap_absolute(&absolute);
        self.backend.resolve_workdir(wrapped)
    }

    /// Unconfined read/write resolution for `WORKSPACE SYSTEM` (issue #163).
    /// Purely lexical: no snapshot materialization, no root-prefix
    /// containment. Read/write mode imposes no distinction under SYSTEM.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn resolve_system(&self, cwd: &GuardedPath, rel: &str) -> Result<GuardedPath> {
        let normalized = to_forward_slashes(rel);
        let rel_path = Path::new(&normalized);
        let base = if cwd.as_path().is_absolute() {
            cwd.as_path().to_path_buf()
        } else {
            self.build_context.as_path().join(cwd.as_path())
        };
        let absolute = if rel_path.is_absolute() {
            rel_path.to_path_buf()
        } else if Self::is_absolute_or_rooted(rel_path) {
            super::cache::system_anchor(&base).join(Self::root_relative_path(rel_path))
        } else {
            base.join(rel_path)
        };
        Ok(super::cache::system_wrap_absolute(&absolute))
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
            // SYSTEM bypass (issue #163): absolute sources on any drive or
            // share resolve without confinement. Relative sources keep the
            // existing build-context chain below.
            if self.is_system() {
                let wrapped = super::cache::system_wrap_absolute(from_path);
                return self.backend.resolve_copy_source(wrapped);
            }
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
        self.resolve_copy_source_from_root(workspace_root, from)
    }

    /// COPY source resolution against an explicit `--from-workspace` root
    /// (issue #163). SNAPSHOT requires a materialized snapshot (reading
    /// from a pending anchor would fabricate an empty source); LOCAL keeps
    /// the workspace-root semantics of the former boolean flag; CACHE
    /// ensures the persistent directory first; SYSTEM resolves absolute
    /// sources on any drive or share without confinement while relative
    /// sources stay anchored onto the build context.
    pub fn resolve_copy_source_from_target(
        &self,
        root: CopySourceRoot,
        from: &str,
    ) -> Result<GuardedPath> {
        match root {
            CopySourceRoot::Local => self.resolve_copy_source_from_workspace(from),
            CopySourceRoot::Snapshot => {
                let concrete = self.shared.concrete.get().ok_or_else(|| {
                    anyhow!("COPY from SNAPSHOT requires a materialized snapshot")
                })?;
                self.resolve_copy_source_from_root(concrete, from)
            }
            CopySourceRoot::Cache => {
                let guard = self.ensure_cache()?;
                self.resolve_copy_source_from_root(&guard, from)
            }
            CopySourceRoot::System => {
                let from_path = Path::new(from);
                if Self::is_absolute_or_rooted(from_path) {
                    let wrapped = super::cache::system_wrap_absolute(from_path);
                    return self.backend.resolve_copy_source(wrapped);
                }
                let candidate = self.build_context.as_path().join(from);
                self.check_access_with_root(&self.build_context, &candidate, AccessMode::Read)
                    .with_context(|| {
                        format!("failed to resolve COPY source {}", candidate.display())
                    })
                    .and_then(|guarded| self.backend.resolve_copy_source(guarded))
            }
        }
    }

    fn resolve_copy_source_from_root(
        &self,
        source_root: &GuardedPath,
        from: &str,
    ) -> Result<GuardedPath> {
        let from_path = Path::new(from);
        if Self::is_absolute_or_rooted(from_path) {
            return self
                .check_access_with_root(source_root, from_path, AccessMode::Read)
                .with_context(|| format!("failed to resolve COPY source {}", from_path.display()))
                .and_then(|guarded| self.backend.resolve_copy_source(guarded));
        }

        let candidate = source_root.as_path().join(from);
        self.check_access_with_root(source_root, &candidate, AccessMode::Read)
            .with_context(|| format!("failed to resolve COPY source {}", candidate.display()))
            .and_then(|guarded| self.backend.resolve_copy_source(guarded))
    }

    fn resolve(&self, cwd: &GuardedPath, rel: &str, mode: AccessMode) -> Result<GuardedPath> {
        // SYSTEM bypass (issue #163): no snapshot materialization, no
        // root-prefix containment. Each result anchors at its own
        // filesystem anchor so every drive and share resolves.
        if self.is_system() {
            return self.resolve_system(cwd, rel);
        }
        // CACHE choke point (issue #163): ensure the persistent directory
        // exists before the confined flow below operates on it.
        if self.is_cache() {
            self.ensure_cache()?;
        }
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

    /// Pin the exact cache dir for hermetic tests (issue #163). Shared with
    /// the `cache` module's guard so parallel tests cannot race overrides.
    fn pin_cache_dir(dir: &GuardedPath) -> super::super::cache::SerialCacheEnv {
        let dir_str = dir.as_path().to_string_lossy().into_owned();
        super::super::cache::SerialCacheEnv::new(&[(crate::env::CACHE_DIR, Some(dir_str.as_str()))])
    }

    /// `WORKSPACE CACHE` round trip (issue #163): switching selects the
    /// persistent guard, writes land under the pinned dir, reads see them,
    /// and the snapshot handle never materializes.
    #[test]
    fn cache_selection_reads_and_writes_under_pinned_dir() {
        let pin_temp = GuardedPath::tempdir().unwrap();
        let pin_root = pin_temp.as_guarded_path().clone();
        let _env = pin_cache_dir(&pin_root);

        let local_temp = GuardedPath::tempdir().unwrap();
        let local = local_temp.as_guarded_path().clone();
        let mut resolver = PathResolver::new_lazy(local.clone()).unwrap();
        let handle = resolver.snapshot_handle();
        resolver.switch_to_cache();

        let cwd = resolver.root().clone();
        assert!(
            cwd.as_path().starts_with(pin_root.as_path()),
            "cache root must live under the pinned dir"
        );
        let target = resolver.resolve_write(&cwd, "cached.txt").unwrap();
        assert!(
            target.as_path().starts_with(pin_root.as_path()),
            "cache writes must stay under the pinned dir"
        );
        resolver.write_file(&target, b"persistent").unwrap();
        let back = resolver.resolve_read(&cwd, "cached.txt").unwrap();
        assert_eq!(resolver.read_file(&back).unwrap(), b"persistent");
        assert!(
            !handle.is_materialized(),
            "cache activity must not materialize the snapshot"
        );

        // A fresh resolver with the same pin sees the same persistent entry.
        let mut second = PathResolver::new_lazy(local.clone()).unwrap();
        second.switch_to_cache();
        let second_cwd = second.root().clone();
        let probe = second.resolve_read(&second_cwd, "cached.txt").unwrap();
        assert_eq!(second.read_file(&probe).unwrap(), b"persistent");
    }

    /// `WORKSPACE CACHE` still confines: `..` escapes above the cache root
    /// fail exactly like snapshot and local escapes.
    #[test]
    fn cache_selection_rejects_escapes() {
        let pin_temp = GuardedPath::tempdir().unwrap();
        let pin_root = pin_temp.as_guarded_path().clone();
        let _env = pin_cache_dir(&pin_root);

        let local_temp = GuardedPath::tempdir().unwrap();
        let local = local_temp.as_guarded_path().clone();
        let mut resolver = PathResolver::new_lazy(local).unwrap();
        resolver.switch_to_cache();
        let cwd = resolver.root().clone();

        assert!(resolver.resolve_write(&cwd, "../escape.txt").is_err());
        assert!(resolver.resolve_read(&cwd, "a/../../escape.txt").is_err());
        resolver.resolve_write(&cwd, "ok.txt").unwrap();
    }

    /// `set_root` restores four-way selection (issue #163): scope push/pop
    /// round-trips through every root without collapsing CACHE or SYSTEM
    /// onto snapshot or local.
    #[test]
    fn set_root_restores_all_four_selections() {
        let pin_temp = GuardedPath::tempdir().unwrap();
        let pin_root = pin_temp.as_guarded_path().clone();
        let _env = pin_cache_dir(&pin_root);

        let local_temp = GuardedPath::tempdir().unwrap();
        let local = local_temp.as_guarded_path().clone();
        let mut resolver = PathResolver::new_lazy(local.clone()).unwrap();

        let snapshot_root = resolver.root().clone();
        resolver.switch_to_local();
        let local_root = resolver.root().clone();
        resolver.switch_to_cache();
        let cache_root = resolver.root().clone();
        resolver.switch_to_system();
        let system_root = resolver.root().clone();

        resolver.set_root(&cache_root);
        assert_eq!(resolver.root(), &cache_root);
        let cache_cwd = resolver.root().clone();
        resolver.resolve_write(&cache_cwd, "c.txt").unwrap();

        resolver.set_root(&system_root);
        let system_cwd = resolver.root().clone();
        let sys_target = resolver.resolve_write(&system_cwd, "s.txt").unwrap();
        assert!(sys_target.as_path().is_absolute());

        resolver.set_root(&local_root);
        assert_eq!(resolver.root(), &local);
        resolver.set_root(&snapshot_root);
        assert!(resolver.is_snapshot_pending());
    }

    /// `WORKSPACE SYSTEM` bypass (issue #163): absolute paths resolve on any
    /// location without confinement, relative paths join the cwd, and `..`
    /// normalizes lexically instead of erroring.
    #[test]
    fn system_selection_resolves_absolute_paths_without_confinement() {
        let local_temp = GuardedPath::tempdir().unwrap();
        let local = local_temp.as_guarded_path().clone();
        let mut resolver = PathResolver::new_lazy(local.clone()).unwrap();
        resolver.switch_to_system();

        let outside_temp = GuardedPath::tempdir().unwrap();
        let outside = outside_temp.as_guarded_path().clone();
        let probe = outside.join("sys-probe.txt").expect("probe path");
        let probe_str = probe.as_path().to_string_lossy().into_owned();

        // A confined resolver re-anchors the same absolute path under
        // its own root instead of honoring it: the bypass is SYSTEM-only.
        let confined = PathResolver::new_guarded(local.clone(), local.clone()).expect("confined");
        let rebased = confined.resolve_read(&local, &probe_str).expect("rebase");
        assert_ne!(rebased.as_path(), probe.as_path());
        assert!(rebased.as_path().starts_with(local.as_path()));

        let cwd = resolver.system_entry_cwd(&local);
        // Seed through the SYSTEM resolver itself so the write lands in
        // the backend namespace the read observes (the Miri synthetic
        // backend keys state by guard root).
        let target = resolver.resolve_write(&cwd, &probe_str).unwrap();
        assert_eq!(target.as_path(), probe.as_path());
        resolver.write_file(&target, b"system").unwrap();
        let back = resolver.resolve_read(&cwd, &probe_str).unwrap();
        assert_eq!(resolver.read_file(&back).unwrap(), b"system");

        let rel = resolver.resolve_write(&cwd, "rel.txt").unwrap();
        assert!(rel.as_path().starts_with(cwd.as_path()));

        let dotdot = resolver
            .resolve_read(&probe, "../sys-probe.txt")
            .expect("lexical dotdot under system");
        assert_eq!(dotdot.as_path(), probe.as_path());
    }

    /// Entering SYSTEM from a pending snapshot must not leak the virtual
    /// anchor into unconfined resolution (issue #163): the entry cwd leaves
    /// the never-created anchor behind, landing on the concrete root once
    /// materialized.
    #[test]
    fn system_entry_cwd_rebases_off_the_snapshot_anchor() {
        let local_temp = GuardedPath::tempdir().unwrap();
        let local = local_temp.as_guarded_path().clone();
        let resolver = PathResolver::new_lazy(local.clone()).unwrap();

        let anchor_cwd = resolver.root().clone();
        assert!(resolver.is_snapshot_pending());
        let entry = resolver.system_entry_cwd(&anchor_cwd);
        assert!(!entry.as_path().starts_with(resolver.anchor_path()));
        assert_ne!(entry.as_path(), anchor_cwd.as_path());

        let materialized = resolver
            .resolve_write(&anchor_cwd, "seed.txt")
            .expect("materialize snapshot");
        resolver.write_file(&materialized, b"x").expect("seed");
        let rebased = resolver.system_entry_cwd(&anchor_cwd);
        assert_eq!(rebased.root(), materialized.root());
        // Native only: under Miri the reserved anchor and the concrete
        // synthetic root coincide by design, so prefix-distinctness is
        // vacuous there.
        #[cfg(not(miri))]
        assert!(!rebased.as_path().starts_with(resolver.anchor_path()));
    }
}
