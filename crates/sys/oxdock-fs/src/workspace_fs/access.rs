#[cfg(not(miri))]
use anyhow::Context;
use anyhow::{Result, bail};
#[cfg(not(miri))]
#[allow(clippy::disallowed_types)]
use std::ffi::OsString;
#[allow(clippy::disallowed_types)]
use std::path::Path;
#[cfg(miri)]
#[cfg_attr(miri, allow(clippy::disallowed_types, clippy::disallowed_methods))]
use std::path::PathBuf;
#[cfg(not(miri))]
#[allow(clippy::disallowed_types)]
use std::path::PathBuf;

use super::{AccessMode, GuardedPath, PathResolver};

/// Ensure `candidate` stays within `root`, even if parts of the path do not yet exist.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub(crate) fn guard_path(
    root: &Path,
    candidate: &Path,
    mode: AccessMode,
) -> Result<std::path::PathBuf> {
    #[cfg(miri)]
    {
        guard_path_miri(root, candidate, mode)
    }

    #[cfg(not(miri))]
    {
        if !root.exists() {
            std::fs::create_dir_all(root)
                .with_context(|| format!("failed to create root {}", root.display()))?;
        }
        let root_abs = std::fs::canonicalize(root)
            .with_context(|| format!("failed to canonicalize root {}", root.display()))?;

        let cand_abs = normalize_candidate(&root_abs, candidate)?;
        if !cand_abs.starts_with(&root_abs) {
            bail!(
                "{} access to {} escapes allowed root {}",
                mode.name(),
                cand_abs.display(),
                root_abs.display()
            );
        }

        Ok(cand_abs)
    }
}

#[cfg(not(miri))]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn normalize_candidate(root_abs: &Path, candidate: &Path) -> Result<PathBuf> {
    if let Ok(cand_abs) = std::fs::canonicalize(candidate) {
        return Ok(cand_abs);
    }

    let mut ancestor = candidate;
    while !ancestor.exists() {
        if let Some(parent) = ancestor.parent() {
            ancestor = parent;
        } else {
            ancestor = root_abs;
            break;
        }
    }

    let ancestor_abs = std::fs::canonicalize(ancestor)
        .with_context(|| format!("failed to canonicalize ancestor {}", ancestor.display()))?;

    let mut rem_components: Vec<OsString> = Vec::new();
    {
        let mut skip = ancestor.components();
        let mut full = candidate.components();
        loop {
            match (skip.next(), full.next()) {
                (Some(s), Some(f)) if s == f => continue,
                (_opt_s, opt_f) => {
                    if let Some(f) = opt_f {
                        rem_components.push(f.as_os_str().to_os_string());
                        for comp in full {
                            rem_components.push(comp.as_os_str().to_os_string());
                        }
                    }
                    break;
                }
            }
        }
    }

    let mut cand_abs = ancestor_abs.clone();
    for c in rem_components.iter() {
        let s = std::ffi::OsStr::new(&c);
        if s == "." {
            continue;
        }
        if s == ".." {
            cand_abs = cand_abs
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(cand_abs);
            continue;
        }
        cand_abs.push(s);
    }

    Ok(cand_abs)
}

#[cfg(miri)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn guard_path_miri(root: &Path, candidate: &Path, mode: AccessMode) -> Result<PathBuf> {
    // Avoid host filesystem syscalls under Miri by normalizing paths purely in memory.
    let root_abs = if root.is_absolute() {
        normalize_no_fs(root)
    } else {
        normalize_no_fs(&Path::new("/miri").join(root))
    };

    let candidate_abs = if candidate.is_absolute() {
        normalize_no_fs(candidate)
    } else {
        normalize_no_fs(&root_abs.join(candidate))
    };

    if !candidate_abs.starts_with(&root_abs) {
        bail!(
            "{} access to {} escapes allowed root {}",
            mode.name(),
            candidate_abs.display(),
            root_abs.display()
        );
    }

    Ok(candidate_abs)
}

#[cfg(miri)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn normalize_no_fs(path: &Path) -> PathBuf {
    let mut parts: Vec<PathBuf> = Vec::new();
    let is_abs = path.is_absolute();

    for comp in path.components() {
        match comp {
            std::path::Component::RootDir => parts.clear(),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if let Some(last) = parts.last()
                    && !last.as_os_str().is_empty()
                {
                    parts.pop();
                }
            }
            std::path::Component::Normal(seg) => parts.push(seg.into()),
            std::path::Component::Prefix(p) => parts.push(PathBuf::from(p.as_os_str())),
        }
    }

    let mut out = if is_abs {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };
    for seg in parts {
        out.push(seg);
    }
    out
}

// Guard and canonicalize paths under the configured roots.
impl PathResolver {
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub(crate) fn check_access_with_root(
        &self,
        root: &GuardedPath,
        candidate: &Path,
        mode: AccessMode,
    ) -> Result<GuardedPath> {
        match guard_path(root.as_path(), candidate, mode) {
            Ok(guarded) => {
                #[cfg(miri)]
                {
                    Ok(GuardedPath::from_guarded_parts(
                        root.root().to_path_buf(),
                        guarded,
                    ))
                }

                #[cfg(not(miri))]
                {
                    Ok(GuardedPath::from_guarded_parts(root.to_path_buf(), guarded))
                }
            }
            Err(primary) => {
                #[cfg(not(miri))]
                {
                    if root.as_path() == self.root.as_path()
                        && let Some(workspace_root) = &self.workspace_root
                        && let Ok(root_abs) = std::fs::canonicalize(root.as_path())
                        && let Ok(canon) = normalize_candidate(&root_abs, candidate)
                        && canon.starts_with(workspace_root.as_path())
                    {
                        return Ok(GuardedPath::from_guarded_parts(
                            root.to_path_buf(),
                            candidate.to_path_buf(),
                        ));
                    }
                }
                Err(primary)
            }
        }
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub(crate) fn check_access(&self, candidate: &Path, mode: AccessMode) -> Result<GuardedPath> {
        self.check_access_with_root(&self.root, candidate, mode)
    }
}

#[cfg(test)]
mod security_tests {
    use crate::GuardedPath;
    use crate::workspace_fs::PathResolver;

    /// An adversarial symlink inside the root pointing OUTSIDE must never
    /// yield a guarded path: canonicalization resolves the link and the
    /// containment check rejects the resolved location.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    #[cfg(not(miri))]
    #[test]
    fn guard_rejects_symlink_escaping_root() {
        let inside = GuardedPath::tempdir().expect("inside root");
        let outside = GuardedPath::tempdir().expect("outside target");
        let root = inside.as_guarded_path().clone();

        if !oxdock_sys_test_utils::can_create_symlinks(root.as_path()) {
            eprintln!("skipping: symlink creation unavailable on this host");
            return;
        }

        let escape_target = outside
            .as_guarded_path()
            .join("secret.txt")
            .expect("target");
        std::fs::write(escape_target.as_path(), b"leak").expect("outside file");

        let link = root.join("door").expect("link path");
        {
            #[cfg(unix)]
            std::os::unix::fs::symlink(escape_target.as_path(), link.as_path())
                .expect("create symlink");
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(escape_target.as_path(), link.as_path())
                .expect("create symlink");
        }

        // Direct guard construction.
        let direct = GuardedPath::new(root.as_path(), link.as_path());
        assert!(direct.is_err(), "symlink to outside root must be rejected");

        // And through a resolver read.
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
        assert!(
            resolver.read_file(&link).is_err(),
            "reading through an escaping symlink must be rejected"
        );
    }

    /// `..` traversal matrix: mid-path escapes are rejected, `..` that lands
    /// back on the root itself is allowed, and climbing above the filesystem
    /// root cannot smuggle a candidate past containment.
    #[test]
    fn traversal_matrix_is_clamped_to_root() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");

        assert!(
            resolver
                .resolve_read(&root, "sub/../../escape.txt")
                .is_err(),
            "mid-path traversal must be rejected"
        );
        assert!(
            resolver.resolve_read(&root, "../../../escape.txt").is_err(),
            "above-root traversal must be rejected"
        );

        let back_to_root = resolver.resolve_read(&root, "sub/..").expect("clamped");
        assert_eq!(back_to_root.as_path(), root.as_path());

        let dotform = resolver
            .resolve_read(&root, "./sub/./file.txt")
            .expect("dot components allowed");
        assert!(dotform.as_path().starts_with(root.as_path()));
        assert!(dotform.as_path().ends_with("sub/file.txt"));
    }
}
