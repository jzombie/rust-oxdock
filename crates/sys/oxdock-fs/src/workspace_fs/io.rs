use anyhow::{Context, Result, bail};
#[cfg(not(miri))]
use std::fs;

use super::{AccessMode, PathResolver};
use crate::GuardedPath;

#[allow(clippy::disallowed_types)]
use crate::UnguardedPath;

// Guarded filesystem IO helpers (read/write/metadata etc.).
impl PathResolver {
    #[allow(clippy::disallowed_methods)]
    pub fn create_dir_all(&self, path: &GuardedPath) -> Result<()> {
        let guarded = self
            .check_access(path.as_path(), AccessMode::Write)
            .with_context(|| format!("create_dir_all denied for {}", path.display()))?;
        self.backend.create_dir_all(&self.root, &guarded)
    }

    #[allow(clippy::disallowed_methods)]
    pub fn read_dir_entries(&self, path: &GuardedPath) -> Result<Vec<super::DirEntry>> {
        let guarded = self
            .check_access(path.as_path(), AccessMode::Read)
            .or_else(|_| {
                self.check_access_with_root(&self.build_context, path.as_path(), AccessMode::Read)
            })
            .with_context(|| format!("read_dir denied for {}", path.display()))?;
        self.backend.read_dir_entries(&guarded)
    }

    #[allow(clippy::disallowed_methods)]
    pub fn read_file(&self, path: &GuardedPath) -> Result<Vec<u8>> {
        let guarded = self
            .check_access(path.as_path(), AccessMode::Read)
            .or_else(|_| {
                self.check_access_with_root(&self.build_context, path.as_path(), AccessMode::Read)
            })
            .with_context(|| format!("read denied for {}", path.display()))?;
        self.backend.read_file(&guarded)
    }

    #[cfg(not(miri))]
    #[allow(clippy::disallowed_methods)]
    pub fn read_to_string(&self, path: &GuardedPath) -> Result<String> {
        let guarded = self
            .check_access(path.as_path(), AccessMode::Read)
            .with_context(|| format!("read denied for {}", path.display()))?;
        let s = fs::read_to_string(guarded.as_path())
            .with_context(|| format!("failed to read {}", guarded.display()))?;
        Ok(s)
    }

    #[cfg(miri)]
    pub fn read_to_string(&self, path: &GuardedPath) -> Result<String> {
        let bytes = self.read_file(path)?;
        let s = String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path))?;
        Ok(s)
    }

    #[allow(clippy::disallowed_methods)]
    pub fn write_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        let guarded = self
            .check_access(path.as_path(), AccessMode::Write)
            .with_context(|| format!("write denied for {}", path.display()))?;
        self.backend.write_file(&guarded, contents)
    }

    #[allow(clippy::disallowed_methods)]
    pub fn append_file(&self, path: &GuardedPath, contents: &[u8]) -> Result<()> {
        let guarded = self
            .check_access(path.as_path(), AccessMode::Write)
            .with_context(|| format!("append denied for {}", path.display()))?;
        self.backend.append_file(&guarded, contents)
    }

    pub fn ensure_parent_dir(&self, path: &GuardedPath) -> Result<()> {
        if let Some(parent) = path.as_path().parent() {
            let parent_guard = self
                .check_access(parent, AccessMode::Write)
                .or_else(|_| {
                    self.check_access_with_root(&self.build_context, parent, AccessMode::Write)
                })
                .with_context(|| format!("parent {} escapes root", parent.display()))?;
            self.backend.create_dir_all(&self.root, &parent_guard)?;
        }
        Ok(())
    }

    #[allow(clippy::disallowed_methods)]
    pub fn canonicalize(&self, path: &GuardedPath) -> Result<GuardedPath> {
        let cand = self
            .check_access(path.as_path(), AccessMode::Read)
            .or_else(|_| {
                self.check_access_with_root(&self.build_context, path.as_path(), AccessMode::Read)
            })
            .with_context(|| format!("canonicalize denied for {}", path.display()))?;
        self.backend.canonicalize(cand)
    }

    #[allow(clippy::disallowed_methods)]
    pub fn metadata(&self, path: &GuardedPath) -> Result<std::fs::Metadata> {
        let guarded = self
            .check_access(path.as_path(), AccessMode::Read)
            .or_else(|_| {
                self.check_access_with_root(&self.build_context, path.as_path(), AccessMode::Read)
            })
            .with_context(|| format!("metadata denied for {}", path.display()))?;
        self.backend.metadata(&guarded)
    }

    pub fn entry_kind(&self, path: &GuardedPath) -> Result<super::EntryKind> {
        self.backend.entry_kind(path)
    }

    /// Lightweight existence check that avoids host `stat` calls under Miri.
    pub fn exists(&self, path: &GuardedPath) -> bool {
        self.backend.entry_kind(path).is_ok()
    }

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    pub fn metadata_unguarded(&self, path: &UnguardedPath) -> Result<std::fs::Metadata> {
        #[cfg(not(miri))]
        {
            let meta = std::fs::metadata(path.as_path())
                .with_context(|| format!("failed to stat external {}", path.as_path().display()))?;
            Ok(meta)
        }
        #[cfg(miri)]
        {
            let _ = path;
            anyhow::bail!("metadata_unguarded not supported on Miri")
        }
    }
    #[cfg(not(miri))]
    #[allow(clippy::disallowed_methods)]
    pub fn set_permissions_mode_unix(&self, path: &GuardedPath, mode: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let guarded = self
                .check_access(path.as_path(), AccessMode::Write)
                .with_context(|| format!("set_permissions denied for {}", path.display()))?;
            fs::set_permissions(guarded.as_path(), fs::Permissions::from_mode(mode))
                .with_context(|| format!("failed to set permissions on {}", guarded.display()))?;
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            let _ = mode;
        }
        Ok(())
    }

    #[cfg(miri)]
    pub fn set_permissions_mode_unix(&self, _path: &GuardedPath, _mode: u32) -> Result<()> {
        Ok(())
    }

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    pub fn open_file_unguarded(&self, path: &UnguardedPath) -> Result<std::fs::File> {
        #[cfg(not(miri))]
        {
            let f = fs::File::open(path.as_path())
                .with_context(|| format!("failed to open {}", path.as_path().display()))?;
            Ok(f)
        }
        #[cfg(miri)]
        {
            let _ = path;
            anyhow::bail!("open_file_unguarded not supported on Miri")
        }
    }

    /// Remove a file after validating it is within allowed roots.
    #[allow(clippy::disallowed_methods)]
    pub fn remove_file(&self, path: &GuardedPath) -> Result<()> {
        let guarded = self
            .check_access(path.as_path(), AccessMode::Write)
            .with_context(|| format!("remove_file denied for {}", path.display()))?;
        self.backend.remove_file(&guarded)
    }

    /// Remove a directory and its contents after validating it is within allowed roots.
    #[allow(clippy::disallowed_methods)]
    pub fn remove_dir_all(&self, path: &GuardedPath) -> Result<()> {
        let guarded = self
            .check_access(path.as_path(), AccessMode::Write)
            .with_context(|| format!("remove_dir_all denied for {}", path.display()))?;
        self.backend.remove_dir_all(&guarded)
    }

    #[cfg(not(miri))]
    #[allow(clippy::disallowed_methods)]
    pub fn symlink(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<()> {
        let guarded_src = self
            .check_access(src.as_path(), AccessMode::Read)
            .or_else(|_| {
                self.check_access_with_root(&self.build_context, src.as_path(), AccessMode::Read)
            })
            .with_context(|| format!("symlink source denied for {}", src.display()))?;
        let guarded_dst = self
            .check_access(dst.as_path(), AccessMode::Write)
            .with_context(|| format!("symlink destination denied for {}", dst.display()))?;

        if guarded_dst.as_path().exists() {
            bail!(
                "SYMLINK destination already exists: {}",
                guarded_dst.display()
            );
        }
        if guarded_src.as_path() == guarded_dst.as_path() {
            bail!(
                "SYMLINK source resolves to the destination itself: {}",
                guarded_src.display()
            );
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(guarded_src.as_path(), guarded_dst.as_path()).with_context(
                || {
                    format!(
                        "failed to symlink {} -> {}",
                        guarded_src.display(),
                        guarded_dst.display()
                    )
                },
            )?;
            Ok(())
        }

        #[cfg(all(windows, not(unix)))]
        {
            use std::os::windows::fs::{symlink_dir, symlink_file};
            let meta = fs::metadata(guarded_src.as_path()).with_context(|| {
                format!("failed to stat symlink source {}", guarded_src.display())
            })?;

            let try_link = if meta.is_dir() {
                symlink_dir(guarded_src.as_path(), guarded_dst.as_path())
            } else {
                symlink_file(guarded_src.as_path(), guarded_dst.as_path())
            };

            try_link.with_context(|| {
                format!(
                    "failed to symlink {} -> {}",
                    guarded_src.display(),
                    guarded_dst.display()
                )
            })?;
            Ok(())
        }

        #[cfg(not(any(unix, windows)))]
        {
            bail!(
                "SYMLINK unsupported on this platform ({} -> {})",
                guarded_src.display(),
                guarded_dst.display()
            );
        }
    }

    #[cfg(miri)]
    pub fn symlink(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<()> {
        let guarded_src = self
            .check_access_with_root(&self.root, src.as_path(), AccessMode::Read)
            .with_context(|| format!("symlink source denied for {}", src.display()))?;
        let guarded_dst = self
            .check_access_with_root(&self.root, dst.as_path(), AccessMode::Write)
            .with_context(|| format!("symlink destination denied for {}", dst.display()))?;

        if self.entry_kind(&guarded_dst).is_ok() {
            bail!(
                "SYMLINK destination already exists: {}",
                guarded_dst.display()
            );
        }

        let _ = self.entry_kind(&guarded_src)?;
        bail!(
            "SYMLINK unsupported under Miri ({} -> {})",
            guarded_src.display(),
            guarded_dst.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_fs::GuardedPath;

    #[cfg_attr(
        miri,
        ignore = "exercises host std::fs open calls; blocked under Miri isolation"
    )]
    #[test]
    fn resolver_read_write_and_cleanup_file() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");

        let file = root.join("dir/nested.txt").expect("join");
        resolver.ensure_parent_dir(&file).expect("ensure parent");
        resolver.write_file(&file, b"hello").expect("write");
        resolver
            .set_permissions_mode_unix(&file, 0o644)
            .expect("chmod");

        let contents = resolver.read_to_string(&file).expect("read_to_string");
        assert_eq!(contents, "hello");

        let canonical = resolver.canonicalize(&file).expect("canonicalize");
        assert_eq!(canonical.as_path(), file.as_path());

        let entries = resolver
            .read_dir_entries(&root.join("dir").expect("dir"))
            .expect("read_dir_entries");
        assert!(!entries.is_empty());

        let meta = resolver.metadata(&file).expect("metadata");
        assert!(meta.is_file());

        #[allow(clippy::disallowed_types)]
        {
            let unguarded = UnguardedPath::external(file.to_path_buf());
            let _ = resolver.open_file_unguarded(&unguarded).expect("open");
            let _ = resolver.metadata_unguarded(&unguarded).expect("stat");
        }

        resolver.remove_file(&file).expect("remove file");
        assert!(!resolver.exists(&file));
        resolver
            .remove_dir_all(&root.join("dir").expect("dir"))
            .expect("remove dir");
    }

    fn symlink_capable_root() -> Option<(GuardedPath, PathResolver)> {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        if !oxdock_sys_test_utils::can_create_symlinks(root.as_path()) {
            eprintln!("skipping: symlink creation unavailable on this host");
            return None;
        }
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");
        Some((root, resolver))
    }

    #[cfg_attr(miri, ignore = "exercises real symlinks; blocked under Miri isolation")]
    #[test]
    fn symlink_rejects_existing_destination() {
        let Some((root, resolver)) = symlink_capable_root() else {
            return;
        };

        let src = root.join("src.txt").expect("src");
        resolver.write_file(&src, b"body").expect("write src");
        let dst = root.join("dst.txt").expect("dst");
        resolver.write_file(&dst, b"existing").expect("write dst");

        let err = resolver.symlink(&src, &dst).expect_err("must bail");
        assert!(
            err.to_string().contains("already exists"),
            "unexpected error: {err}"
        );
    }

    #[cfg_attr(miri, ignore = "exercises real symlinks; blocked under Miri isolation")]
    #[test]
    fn symlink_rejects_self_destination() {
        let Some((root, resolver)) = symlink_capable_root() else {
            return;
        };

        // The destination must not exist so we reach the identity check
        // rather than tripping the earlier "already exists" gate.
        let ghost = root.join("ghost-self.txt").expect("self");

        let err = resolver.symlink(&ghost, &ghost).expect_err("must bail");
        assert!(
            err.to_string().contains("destination itself"),
            "unexpected error: {err}"
        );
    }

    #[cfg_attr(miri, ignore = "exercises real symlinks; blocked under Miri isolation")]
    #[test]
    fn symlink_creates_readable_link_within_workspace() {
        let Some((root, resolver)) = symlink_capable_root() else {
            return;
        };

        let src = root.join("real.txt").expect("src");
        resolver
            .write_file(&src, b"through-link")
            .expect("write src");
        let link = root.join("alias.txt").expect("link dst");

        resolver.symlink(&src, &link).expect("symlink");

        let contents = resolver.read_to_string(&link).expect("read through link");
        assert_eq!(contents, "through-link");
    }
}
