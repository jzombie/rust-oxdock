use anyhow::{Context, Result, anyhow};
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::path::{Path, PathBuf};

use super::{AccessMode, PathResolver, to_forward_slashes};
use crate::GuardedPath;

// Path resolution helpers (WORKDIR, READ/WRITE, COPY sources).
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl PathResolver {
    fn root_relative_path(path: &Path) -> PathBuf {
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

    #[allow(clippy::disallowed_macros)]
    fn is_absolute_or_rooted(path: &Path) -> bool {
        path.is_absolute()
            || (cfg!(windows) && path.components().next() == Some(std::path::Component::RootDir))
    }

    pub fn resolve_workdir(&self, current: &GuardedPath, new_dir: &str) -> Result<GuardedPath> {
        if new_dir == "/" {
            // Reset to the resolver root when WORKDIR is set to '/'.
            return Ok(self.root().clone());
        }
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
        let rel_path = Path::new(rel);
        if Self::is_absolute_or_rooted(rel_path) {
            if let Ok(guarded) = self.check_access(rel_path, mode) {
                return Ok(guarded);
            }
            let rel = Self::root_relative_path(rel_path);
            let candidate = self.root().as_path().join(rel);
            return self.check_access(&candidate, mode);
        }

        let candidate = cwd.as_path().join(rel);
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
}
