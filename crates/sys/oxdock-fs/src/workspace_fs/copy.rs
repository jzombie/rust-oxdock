use anyhow::{Context, Result, bail};
use std::fs;
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
use std::path::Path;

use super::{AccessMode, PathResolver};
use crate::GuardedPath;

#[allow(clippy::disallowed_types)]
use crate::UnguardedPath;

enum CopyEntryKind {
    Dir,
    File,
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn entry_kind_follow_symlink(file_type: &fs::FileType, src_path: &Path) -> Result<CopyEntryKind> {
    if file_type.is_dir() {
        return Ok(CopyEntryKind::Dir);
    }
    if file_type.is_file() {
        return Ok(CopyEntryKind::File);
    }
    if file_type.is_symlink() {
        let meta = fs::metadata(src_path)?;
        if meta.is_dir() {
            return Ok(CopyEntryKind::Dir);
        }
        if meta.is_file() {
            return Ok(CopyEntryKind::File);
        }
    }
    bail!("unsupported file type: {}", src_path.display());
}

// Copy helpers for guarded and external sources.
impl PathResolver {
    /// Re-validate an already-resolved source at use time (issue #163).
    /// The source guard records the root it was validated under, so
    /// re-check against that root first: cross-root uses (a CACHE, SYSTEM,
    /// or SNAPSHOT source under a different selection) survive selection
    /// changes while keeping the TOCTOU re-canonicalization. Sources
    /// wrapped at their own filesystem anchor (SYSTEM) re-wrap lexically
    /// with no confinement, mirroring resolution. The legacy
    /// effective-root and build-context fallbacks stay last. Shared by
    /// COPY and SYMLINK.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    pub(crate) fn recheck_resolved_source(&self, src: &GuardedPath) -> Result<GuardedPath> {
        let root_guard =
            GuardedPath::from_guarded_parts(src.root().to_path_buf(), src.root().to_path_buf());
        if let Ok(guarded) =
            self.check_access_with_root(&root_guard, src.as_path(), AccessMode::Read)
        {
            return Ok(guarded);
        }
        if src.root() == super::cache::system_anchor(src.as_path()).as_path() {
            return Ok(super::cache::system_wrap_absolute(src.as_path()));
        }
        self.check_access(src.as_path(), AccessMode::Read)
            .or_else(|_| {
                self.check_access_with_root(&self.build_context, src.as_path(), AccessMode::Read)
            })
    }

    #[cfg(not(miri))]
    #[allow(clippy::disallowed_methods)]
    pub fn copy_file(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<u64> {
        let guarded_dst = self
            .check_access(dst.as_path(), AccessMode::Write)
            .with_context(|| format!("copy destination denied for {}", dst.display()))?;
        let guarded_src = self
            .recheck_resolved_source(src)
            .with_context(|| format!("copy source denied for {}", src.display()))?;
        if let Some(parent) = guarded_dst.as_path().parent() {
            // Defer validation to `create_dir_all`'s own checks: the
            // parent of a directory destination (e.g. `COPY file .`)
            // legitimately sits outside the destination root, and
            // `GuardedPath::new` would falsely reject it here.
            let parent_guard = GuardedPath::from_guarded_parts(
                guarded_dst.root().to_path_buf(),
                parent.to_path_buf(),
            );
            self.create_dir_all(&parent_guard)
                .with_context(|| format!("creating dir {}", parent.display()))?;
        }
        let n = fs::copy(guarded_src.as_path(), guarded_dst.as_path()).with_context(|| {
            format!(
                "copying {} to {}",
                guarded_src.display(),
                guarded_dst.display()
            )
        })?;
        Ok(n)
    }

    #[cfg(miri)]
    pub fn copy_file(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<u64> {
        let guarded_dst = self
            .check_access(dst.as_path(), AccessMode::Write)
            .with_context(|| format!("copy destination denied for {}", dst.display()))?;
        let guarded_src = self
            .recheck_resolved_source(src)
            .with_context(|| format!("copy source denied for {}", src.display()))?;
        // Backend-direct I/O: both guards were validated above, so this
        // must not route through the re-validating `read_file`/`write_file`
        // trait paths (which only know the current selection).
        let data = self.backend.read_file(&guarded_src)?;
        self.backend.write_file(&guarded_dst, &data)?;
        Ok(data.len() as u64)
    }

    #[allow(clippy::disallowed_methods)]
    #[allow(clippy::only_used_in_recursion)]
    #[cfg(not(miri))]
    pub fn copy_dir_recursive(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<()> {
        let guarded_dst_root = self
            .check_access(dst.as_path(), AccessMode::Write)
            .with_context(|| format!("copy destination denied for {}", dst.display()))?;
        self.create_dir_all(&guarded_dst_root)
            .with_context(|| format!("creating dir {}", guarded_dst_root.display()))?;

        for entry in fs::read_dir(src.as_path())? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let src_path = entry.path();
            let dst_path = guarded_dst_root.as_path().join(entry.file_name());

            let entry_guard =
                GuardedPath::from_guarded_parts(src.root().to_path_buf(), src_path.clone());
            let guarded_src = self
                .recheck_resolved_source(&entry_guard)
                .with_context(|| format!("copy source denied for {}", src_path.display()))?;

            let guarded_dst = self
                .check_access(&dst_path, AccessMode::Write)
                .with_context(|| format!("copy destination denied for {}", dst_path.display()))?;

            match entry_kind_follow_symlink(&file_type, &src_path)? {
                CopyEntryKind::Dir => {
                    self.create_dir_all(&guarded_dst)
                        .with_context(|| format!("creating dir {}", guarded_dst.display()))?;
                    self.copy_dir_recursive(&guarded_src, &guarded_dst)?;
                }
                CopyEntryKind::File => {
                    if let Some(parent) = guarded_dst.as_path().parent() {
                        // Same as `copy_file`: defer validation to
                        // `create_dir_all`, whose own checks admit parents
                        // outside the destination root.
                        let parent_guard = GuardedPath::from_guarded_parts(
                            guarded_dst.root().to_path_buf(),
                            parent.to_path_buf(),
                        );
                        self.create_dir_all(&parent_guard)
                            .with_context(|| format!("creating dir {}", parent.display()))?;
                    }
                    fs::copy(guarded_src.as_path(), guarded_dst.as_path()).with_context(|| {
                        format!(
                            "copying {} to {}",
                            guarded_src.display(),
                            guarded_dst.display()
                        )
                    })?;
                }
            }
        }
        Ok(())
    }

    #[cfg(miri)]
    pub fn copy_dir_recursive(&self, src: &GuardedPath, dst: &GuardedPath) -> Result<()> {
        let guarded_dst_root = self
            .check_access(dst.as_path(), AccessMode::Write)
            .with_context(|| format!("copy destination denied for {}", dst.display()))?;
        self.create_dir_all(&guarded_dst_root)?;

        // Backend-direct listing: `src` was validated by the caller, so
        // this must not route through the re-validating `read_dir_entries`
        // trait path (which only knows the current selection).
        for entry in self.backend.read_dir_entries(src)? {
            let file_type = entry.file_type()?;
            let src_path = entry.path();
            let dst_path = guarded_dst_root.as_path().join(entry.file_name());

            let entry_guard =
                GuardedPath::from_guarded_parts(src.root().to_path_buf(), src_path.clone());
            let guarded_src = self
                .recheck_resolved_source(&entry_guard)
                .with_context(|| format!("copy source denied for {}", src_path.display()))?;

            let guarded_dst = self
                .check_access(&dst_path, AccessMode::Write)
                .with_context(|| format!("copy destination denied for {}", dst_path.display()))?;

            if file_type.is_dir() {
                self.create_dir_all(&guarded_dst)
                    .with_context(|| format!("creating dir {}", guarded_dst.display()))?;
                self.copy_dir_recursive(&guarded_src, &guarded_dst)?;
            } else {
                if let Some(parent) = guarded_dst.as_path().parent() {
                    // Same as `copy_file`: defer validation to
                    // `create_dir_all`, whose own checks admit parents
                    // outside the destination root.
                    let parent_guard = GuardedPath::from_guarded_parts(
                        guarded_dst.root().to_path_buf(),
                        parent.to_path_buf(),
                    );
                    self.create_dir_all(&parent_guard)
                        .with_context(|| format!("creating dir {}", parent_guard.display()))?;
                }
                self.copy_file(&guarded_src, &guarded_dst)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    pub fn copy_dir_from_unguarded(&self, src: &UnguardedPath, dst: &GuardedPath) -> Result<()> {
        let guarded_dst_root = self
            .check_access(dst.as_path(), AccessMode::Write)
            .with_context(|| format!("copy destination denied for {}", dst.display()))?;
        fs::create_dir_all(guarded_dst_root.as_path())
            .with_context(|| format!("creating dir {}", guarded_dst_root.display()))?;

        for entry in fs::read_dir(src.as_path())? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let src_path = entry.path();
            let dst_path = guarded_dst_root.as_path().join(entry.file_name());

            match entry_kind_follow_symlink(&file_type, &src_path)? {
                CopyEntryKind::Dir => {
                    fs::create_dir_all(&dst_path)
                        .with_context(|| format!("creating dir {}", dst_path.display()))?;
                    self.copy_dir_from_unguarded(
                        &UnguardedPath::external(src_path),
                        &GuardedPath::new(guarded_dst_root.root(), &dst_path)?,
                    )?;
                }
                CopyEntryKind::File => {
                    if let Some(parent) = dst_path.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("creating dir {}", parent.display()))?;
                    }
                    fs::copy(&src_path, &dst_path).with_context(|| {
                        format!("copying {} to {}", src_path.display(), dst_path.display())
                    })?;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    pub fn copy_file_from_unguarded(&self, src: &UnguardedPath, dst: &GuardedPath) -> Result<u64> {
        let guarded_dst = self
            .check_access(dst.as_path(), AccessMode::Write)
            .with_context(|| format!("copy destination denied for {}", dst.display()))?;
        if let Some(parent) = guarded_dst.as_path().parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating dir {}", parent.display()))?;
        }
        let n = fs::copy(src.as_path(), guarded_dst.as_path()).with_context(|| {
            format!(
                "copying {} to {}",
                src.as_path().display(),
                guarded_dst.display()
            )
        })?;
        Ok(n)
    }
}

#[cfg(all(test, miri))]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use anyhow::Result;

    fn resolver() -> Result<(PathResolver, crate::GuardedTempDir)> {
        let temp = GuardedPath::tempdir()?;
        let guard = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(guard.clone(), guard)?;
        Ok((resolver, temp))
    }

    #[test]
    fn copy_file_writes_data_in_synthetic_fs() -> Result<()> {
        let (resolver, _temp) = resolver()?;
        let src = resolver.root().join("input.txt")?;
        resolver.write_file(&src, b"hello world")?;
        let dst = resolver.root().join("output.txt")?;

        resolver.copy_file(&src, &dst)?;

        assert_eq!(resolver.read_file(&dst)?, b"hello world");
        Ok(())
    }

    #[test]
    fn copy_dir_recursive_clones_nested_structure() -> Result<()> {
        let (resolver, _temp) = resolver()?;
        let src_dir = resolver.root().join("src")?;
        let nested_file = src_dir.join("nested").and_then(|n| n.join("file.txt"))?;
        resolver.write_file(&nested_file, b"data")?;
        let dst_dir = resolver.root().join("dst")?;

        resolver.copy_dir_recursive(&src_dir, &dst_dir)?;

        let copied_file = dst_dir.join("nested").and_then(|n| n.join("file.txt"))?;
        assert_eq!(resolver.read_file(&copied_file)?, b"data");
        Ok(())
    }
}

#[cfg(all(test, not(miri), unix))]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod unix_tests {
    use super::*;
    use anyhow::Result;
    use std::os::unix::fs::symlink;

    #[test]
    fn copy_dir_recursive_follows_symlink_files() -> Result<()> {
        let temp = GuardedPath::tempdir()?;
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;

        let src_dir = root.join("src")?;
        resolver.create_dir_all(&src_dir)?;
        let src_file = src_dir.join("file.txt")?;
        resolver.write_file(&src_file, b"hello")?;
        let link_path = src_dir.join("link.txt")?;
        symlink(src_file.as_path(), link_path.as_path())?;

        let dst_dir = root.join("dst")?;
        resolver.copy_dir_recursive(&src_dir, &dst_dir)?;

        let copied_link = dst_dir.join("link.txt")?;
        let data = resolver.read_file(&copied_link)?;
        assert_eq!(data, b"hello");
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod extra_tests {
    use super::*;
    use crate::UnguardedPath;

    #[cfg_attr(
        miri,
        ignore = "uses tempfile::tempdir which touches the host filesystem; blocked under Miri isolation"
    )]
    #[test]
    fn copy_file_from_unguarded_copies_contents() -> anyhow::Result<()> {
        let temp = GuardedPath::tempdir()?;
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;

        let other = tempfile::tempdir()?;
        let src = other.path().join("in.txt");
        std::fs::write(&src, b"payload")?;

        let dst = root.join("out.txt")?;
        resolver.copy_file_from_unguarded(&UnguardedPath::external(src), &dst)?;

        let got = resolver.read_file(&dst)?;
        assert_eq!(got, b"payload");
        Ok(())
    }

    #[cfg_attr(
        miri,
        ignore = "uses tempfile::tempdir which touches the host filesystem; blocked under Miri isolation"
    )]
    #[test]
    fn copy_dir_from_unguarded_round_trips_nested_tree() -> anyhow::Result<()> {
        let temp = GuardedPath::tempdir()?;
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone())?;

        // Build an unguarded source tree: file at top level + nested subdir.
        let other = tempfile::tempdir()?;
        let src_root = other.path();
        std::fs::write(src_root.join("top.txt"), b"top")?;
        std::fs::create_dir_all(src_root.join("nested"))?;
        std::fs::write(src_root.join("nested/deep.txt"), b"deep")?;

        let dst = root.join("copied")?;
        resolver.copy_dir_from_unguarded(&UnguardedPath::external(src_root.to_path_buf()), &dst)?;

        let top = dst.join("top.txt")?;
        assert_eq!(resolver.read_file(&top)?, b"top");
        let deep = dst.join("nested/deep.txt")?;
        assert_eq!(resolver.read_file(&deep)?, b"deep");
        Ok(())
    }
}
