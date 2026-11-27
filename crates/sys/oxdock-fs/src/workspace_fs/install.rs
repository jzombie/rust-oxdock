use oxdock_diagnostics::DiagnosticResult;

use super::{AccessMode, PathResolver};
use crate::GuardedPath;
use crate::diagnostics as fs_diag;

// Guarded atomic-install helper: places an already-verified binary with
// executable modes. Ordering is load-bearing: `ensure_parent_dir` first,
// then stage as a temp FILE inside the dest dir, set mode, then rename.
impl PathResolver {
    /// Atomically place `src` at `dst` with unix `mode`.
    ///
    /// `mode` is the caller's policy (`INSTALL` passes `0o755`); the
    /// mechanism stays centralized here. The mode applies on Unix only:
    /// octal modes are meaningless on Windows, so the parameter is
    /// ignored there rather than making false promises.
    #[allow(clippy::disallowed_methods)]
    pub fn install_file_atomic(
        &self,
        src: &GuardedPath,
        dst: &GuardedPath,
        mode: u32,
    ) -> DiagnosticResult<()> {
        self.ensure_parent_dir(dst).map_err(fs_diag::from_anyhow)?;
        let guarded_dst = self
            .check_access(dst.as_path(), AccessMode::Write)
            .map_err(fs_diag::from_anyhow)
            .map_err(|diag| {
                diag.with_contexts(vec![format!(
                    "install destination denied for {}",
                    dst.display()
                )])
            })?;
        let guarded_src = self
            .recheck_resolved_source(src)
            .map_err(fs_diag::from_anyhow)
            .map_err(|diag| {
                diag.with_contexts(vec![format!("install source denied for {}", src.display())])
            })?;
        #[cfg(miri)]
        {
            let data = self
                .backend
                .read_file(&guarded_src)
                .map_err(fs_diag::from_anyhow)?;
            self.backend
                .write_file(&guarded_dst, &data)
                .map_err(fs_diag::from_anyhow)?;
            let _ = mode;
            Ok(())
        }
        #[cfg(not(miri))]
        {
            self.install_file_atomic_host(&guarded_src, &guarded_dst, mode)
        }
    }

    #[cfg(not(miri))]
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    fn install_file_atomic_host(
        &self,
        src: &GuardedPath,
        dst: &GuardedPath,
        mode: u32,
    ) -> DiagnosticResult<()> {
        let data = self.backend.read_file(src).map_err(fs_diag::from_anyhow)?;
        let parent = dst
            .as_path()
            .parent()
            .ok_or_else(|| fs_diag::install_no_parent(dst))?;
        let staged = stage_bytes_in_dir(parent, &data).map_err(|diag| {
            diag.with_contexts(vec![format!(
                "staging install payload in {}",
                parent.display()
            )])
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode))
                .map_err(|err| fs_diag::install_set_mode_failed(&staged, &err))?;
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
        }
        rename_staged_into_place(&staged, dst.as_path()).map_err(|diag| {
            diag.with_contexts(vec![format!(
                "placing {} as {}",
                staged.display(),
                dst.display()
            )])
        })?;
        Ok(())
    }
}

/// Write `data` to a uniquely-named temp file inside `dir`. Never a
/// tempdir: renaming a file out of a staging dir would orphan the empty
/// dir in the user's binary path. `create_new` plus retry makes the
/// reservation atomic; a failed attempt cleans up its own partial file.
#[cfg(not(miri))]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
fn stage_bytes_in_dir(dir: &std::path::Path, data: &[u8]) -> DiagnosticResult<std::path::PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut attempts = 0u32;
    loop {
        let id = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let leaf = format!(
            ".oxdock_install_{}_{}_{x}",
            std::process::id(),
            id,
            x = nanos
        );
        let candidate = dir.join(leaf);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                let write_result = std::io::Write::write_all(&mut file, data)
                    .map_err(|err| fs_diag::install_stage_failed(&candidate, &err));
                drop(file);
                if let Err(e) = write_result {
                    let _ = std::fs::remove_file(&candidate);
                    return Err(e);
                }
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                attempts += 1;
            }
            Err(e) => {
                return Err(fs_diag::install_stage_failed(dir, &e));
            }
        }
    }
}

/// Move the staged file onto its final path. Unix renames atomically
/// (a running image swaps inode natively). Windows renames the existing
/// dest aside first when present: plain overwrite-copy fails with a
/// sharing violation on a running image, while rename of a mapped image
/// succeeds. The aside copy is removed best-effort; a mapped image holds
/// it until handle close, so failure is ignored.
#[cfg(not(miri))]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
fn rename_staged_into_place(
    staged: &std::path::Path,
    dst: &std::path::Path,
) -> DiagnosticResult<()> {
    #[cfg(unix)]
    {
        if let Err(e) = std::fs::rename(staged, dst) {
            let _ = std::fs::remove_file(staged);
            return Err(fs_diag::install_place_failed(staged, dst, &e));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let dst_exists = std::fs::metadata(dst).is_ok();
        if dst_exists {
            let aside = aside_path_for(dst);
            if let Err(e) = std::fs::rename(dst, &aside) {
                let _ = std::fs::remove_file(staged);
                return Err(fs_diag::install_place_failed(dst, &aside, &e));
            }
            if let Err(e) = std::fs::rename(staged, dst) {
                let _ = std::fs::rename(&aside, dst);
                let _ = std::fs::remove_file(staged);
                return Err(fs_diag::install_place_failed(staged, dst, &e));
            }
            let _ = std::fs::remove_file(&aside);
            Ok(())
        } else {
            if let Err(e) = std::fs::rename(staged, dst) {
                let _ = std::fs::remove_file(staged);
                return Err(fs_diag::install_place_failed(staged, dst, &e));
            }
            Ok(())
        }
    }
}

/// Aside path for the Windows cold-aware rename: `<dest>.old.<rand>`.
#[cfg(all(not(miri), not(unix)))]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
fn aside_path_for(dst: &std::path::Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ASIDE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = ASIDE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file_name = dst
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "oxdock".to_string());
    let aside_name = format!(
        "{file_name}.old.{}_{x}",
        std::process::id(),
        x = (nanos ^ (id as u128))
    );
    match dst.parent() {
        Some(parent) => parent.join(aside_name),
        None => std::path::PathBuf::from(aside_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_fs::GuardedPath;

    #[cfg_attr(
        miri,
        ignore = "exercises host staging files; blocked under Miri isolation"
    )]
    #[test]
    fn install_places_bytes_with_executable_mode() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");

        let src = root.join("payload.bin").expect("src");
        resolver
            .write_file(&src, b"install-payload")
            .expect("seed src");
        let dst = root.join("bin/tool").expect("dst");

        resolver
            .install_file_atomic(&src, &dst, 0o755)
            .expect("install");

        let body = resolver.read_file(&dst).expect("read back");
        assert_eq!(body, b"install-payload");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = resolver.metadata(&dst).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "installed binary carries 0755");
        }
    }

    #[cfg_attr(
        miri,
        ignore = "exercises host staging files; blocked under Miri isolation"
    )]
    #[test]
    fn install_replaces_existing_destination() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let resolver = PathResolver::new_guarded(root.clone(), root.clone()).expect("resolver");

        let src = root.join("new.bin").expect("src");
        resolver.write_file(&src, b"new-bytes").expect("seed src");
        let dst = root.join("tool").expect("dst");
        resolver.write_file(&dst, b"old-bytes").expect("seed dst");

        resolver
            .install_file_atomic(&src, &dst, 0o755)
            .expect("reinstall");

        let body = resolver.read_file(&dst).expect("read back");
        assert_eq!(body, b"new-bytes");
    }
}
