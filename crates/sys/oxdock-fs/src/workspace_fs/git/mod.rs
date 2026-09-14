use anyhow::{Context, Result};
#[cfg(not(miri))]
use std::ffi::OsStr;
#[cfg(not(miri))]
#[allow(clippy::disallowed_types)]
use std::process::{Command, Output};

#[cfg(not(miri))]
use super::AccessMode;
use super::PathResolver;
use crate::GuardedPath;
#[cfg(not(miri))]
use anyhow::bail;

pub mod config;
pub use config::{GitIdentity, ensure_git_identity};

#[allow(clippy::disallowed_types)]
pub fn git_root_from_path(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut cur: Option<&std::path::Path> = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

pub fn current_head_commit(root: &GuardedPath) -> Result<Option<String>> {
    #[cfg(miri)]
    {
        let _ = root;
        Ok(None)
    }

    #[cfg(not(miri))]
    {
        let mut cmd = GitCommand::new(root.as_path());
        cmd.arg("rev-parse").arg("HEAD");
        let output = match cmd.output() {
            Ok(out) => out,
            Err(_) => return Ok(None),
        };
        if !output.status.success() {
            return Ok(None);
        }
        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if hash.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hash))
        }
    }
}

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
impl PathResolver {
    #[cfg(not(miri))]
    fn git_command(&self) -> GitCommand {
        GitCommand::new(self.build_context.as_path())
    }

    /// Detect whether a .git directory exists at or above the resolver root.
    pub fn has_git_dir(&self) -> Result<bool> {
        // GuardedPath::parent() cannot traverse above the configured root, which
        // prevents nested crates from seeing a workspace-level .git directory
        // (common for fixtures that live under tests/fixtures/*). Walk the
        // raw filesystem path upward instead so we correctly detect .git
        // without relaxing the guarded access for other operations.
        let mut cur: Option<&std::path::Path> = Some(self.root().as_path());
        while let Some(p) = cur {
            if p.join(".git").exists() {
                return Ok(true);
            }
            cur = p.parent();
        }
        Ok(false)
    }

    pub fn copy_from_git(
        &self,
        rev: &str,
        from: &str,
        to: &GuardedPath,
        include_dirty: bool,
    ) -> Result<()> {
        let source = self
            .resolve_copy_source(from)
            .with_context(|| format!("COPY_GIT failed to resolve source {}", from))?;

        #[cfg(miri)]
        {
            let _ = rev;
            let _ = include_dirty;
            if let Some(parent) = to.as_path().parent() {
                let parent_guard = GuardedPath::new(to.root(), parent)?;
                self.create_dir_all(&parent_guard)
                    .with_context(|| format!("creating parent {}", parent.display()))?;
            }
            match self.entry_kind(&source)? {
                super::EntryKind::Dir => self.copy_dir_recursive(&source, to)?,
                super::EntryKind::File => {
                    self.copy_file(&source, to)?;
                }
                // Following inspection resolves through links.
                super::EntryKind::Symlink => {
                    unreachable!("following entry_kind never reports symlinks")
                }
            }
            return Ok(());
        }

        #[cfg(not(miri))]
        {
            if source.root() != self.build_context.as_path() {
                bail!("COPY_GIT source must resolve within build context");
            }
            let rel = source
                .as_path()
                .strip_prefix(self.build_context.as_path())
                .with_context(|| {
                    format!(
                        "COPY_GIT source {} is not under build context {}",
                        source.display(),
                        self.build_context.display()
                    )
                })?;
            let rel = if rel.as_os_str().is_empty() {
                ".".to_string()
            } else {
                super::to_forward_slashes(&rel.to_string_lossy())
            };

            let _ = self
                .check_access_with_root(self.effective_root(), to.as_path(), AccessMode::Write)
                .with_context(|| format!("destination {} escapes allowed root", to.display()))?;

            let _ = self
                .check_access(self.build_context.as_path(), AccessMode::Read)
                .with_context(|| {
                    format!(
                        "build context {} not under root",
                        self.build_context.display()
                    )
                })?;

            let cat_type = {
                let mut cmd = self.git_command();
                cmd.arg("cat-file")
                    .arg("-t")
                    .arg(format!("{}:{}", rev, rel));
                cmd.output()
            };

            if let Ok(tout) = cat_type
                && tout.status.success()
            {
                let typ = String::from_utf8_lossy(&tout.stdout).trim().to_string();
                if typ == "blob" {
                    let mut show_cmd = self.git_command();
                    show_cmd.arg("show").arg(format!("{}:{}", rev, rel));
                    let show = show_cmd
                        .output()
                        .with_context(|| format!("failed to run git show for {}:{}", rev, rel))?;

                    if show.status.success() {
                        if let Some(parent) = to.as_path().parent() {
                            let parent_guard = GuardedPath::new(to.root(), parent)?;
                            self.create_dir_all(&parent_guard)
                                .with_context(|| format!("creating parent {}", parent.display()))?;
                        }
                        self.write_file(to, &show.stdout)
                            .with_context(|| format!("writing git blob to {}", to.display()))?;
                        if include_dirty {
                            if let Some(parent) = to.as_path().parent() {
                                let parent_guard = GuardedPath::new(to.root(), parent)?;
                                self.create_dir_all(&parent_guard).with_context(|| {
                                    format!("creating parent {}", parent.display())
                                })?;
                            }
                            match self.entry_kind(&source)? {
                                super::EntryKind::Dir => self.copy_dir_recursive(&source, to)?,
                                super::EntryKind::File => {
                                    self.copy_file(&source, to)?;
                                }
                                // Following inspection resolves through links.
                                super::EntryKind::Symlink => {
                                    unreachable!("following entry_kind never reports symlinks")
                                }
                            }
                        }
                        return Ok(());
                    }
                }
            }

            let mut ls_cmd = self.git_command();
            ls_cmd
                .arg("ls-tree")
                .arg("-r")
                .arg("-z")
                .arg(rev)
                .arg("--")
                .arg(&rel);
            let ls = ls_cmd
                .output()
                .with_context(|| format!("failed to run git ls-tree for {}:{}", rev, rel))?;

            if !ls.status.success() {
                bail!("git ls-tree failed for {}:{}", rev, rel);
            }

            let out = ls.stdout;
            if out.is_empty() {
                bail!("path {} not found in rev {}", rel, rev);
            }

            for chunk in out.split(|b| *b == 0) {
                if chunk.is_empty() {
                    continue;
                }
                if let Some(tab_pos) = chunk.iter().position(|b| *b == b'\t') {
                    let header = &chunk[..tab_pos];
                    let path = &chunk[tab_pos + 1..];
                    let header_str = String::from_utf8_lossy(header);
                    let mut parts = header_str.split_whitespace();
                    let mode = parts.next().unwrap_or("");
                    let typ = parts.next().unwrap_or("");
                    let _hash = parts.next().unwrap_or("");

                    let path_str = String::from_utf8_lossy(path).into_owned();
                    let rel = if path_str.starts_with(&format!("{}/", rel)) {
                        path_str[rel.len() + 1..].to_string()
                    } else if path_str == rel {
                        String::new()
                    } else {
                        path_str.clone()
                    };

                    let dest = if rel.is_empty() {
                        to.clone()
                    } else {
                        to.join(&rel)?
                    };

                    if let Some(parent) = dest.as_path().parent() {
                        let parent_guard = GuardedPath::new(dest.root(), parent)?;
                        self.create_dir_all(&parent_guard)
                            .with_context(|| format!("creating parent {}", parent.display()))?;
                    }

                    match typ {
                        "blob" => {
                            let mut blob_cmd = self.git_command();
                            blob_cmd.arg("show").arg(format!("{}:{}", rev, path_str));
                            let blob = blob_cmd.output().with_context(|| {
                                format!("failed to run git show for {}:{}", rev, path_str)
                            })?;
                            if !blob.status.success() {
                                bail!("git show failed for {}:{}", rev, path_str);
                            }

                            // Git uses filemode `120000` to represent symbolic links.
                            // Handle symlink entries by using the blob contents as the
                            // link target: on Unix create an actual symlink from the
                            // target text; on Windows write a textual placeholder file
                            // containing the link target (trimmed). The placeholder is
                            // not an OS-level symlink and may lossy-convert non-UTF8
                            // bytes because `from_utf8_lossy` is used above.
                            if mode == "120000" {
                                let target = String::from_utf8_lossy(&blob.stdout).into_owned();
                                #[cfg(unix)]
                                {
                                    use std::os::unix::fs::symlink;
                                    symlink(target.trim_end_matches('\n'), dest.as_path())
                                        .with_context(|| {
                                            format!(
                                                "creating symlink {} -> {}",
                                                dest.display(),
                                                target
                                            )
                                        })?;
                                }
                                #[cfg(windows)]
                                {
                                    // On Windows create a textual placeholder file containing the
                                    // symlink target (trimmed). This does NOT create an OS-level
                                    // symlink and may lose or alter non-UTF8 bytes (from_utf8_lossy
                                    // is used above).
                                    self.write_file(
                                        &dest,
                                        target.trim_end_matches('\n').as_bytes(),
                                    )
                                    .with_context(|| {
                                        format!(
                                            "writing symlink placeholder {} -> {}",
                                            dest.display(),
                                            target
                                        )
                                    })?;
                                }
                            } else {
                                self.write_file(&dest, &blob.stdout).with_context(|| {
                                    format!("writing blob to {}", dest.display())
                                })?;
                            }
                        }
                        "commit" => {
                            bail!("submodule entries are not supported: {}", path_str);
                        }
                        _ => {}
                    }
                }
            }

            if include_dirty {
                if let Some(parent) = to.as_path().parent() {
                    let parent_guard = GuardedPath::new(to.root(), parent)?;
                    self.create_dir_all(&parent_guard)
                        .with_context(|| format!("creating parent {}", parent.display()))?;
                }
                match self.entry_kind(&source)? {
                    super::EntryKind::Dir => self.copy_dir_recursive(&source, to)?,
                    super::EntryKind::File => {
                        self.copy_file(&source, to)?;
                    }
                    // Following inspection resolves through links.
                    super::EntryKind::Symlink => {
                        unreachable!("following entry_kind never reports symlinks")
                    }
                }
            }
            return Ok(());
        }

        #[allow(unreachable_code)]
        Ok(())
    }
}

#[cfg(not(miri))]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
struct GitCommand {
    inner: Command,
}

#[cfg(not(miri))]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
impl GitCommand {
    fn new(cwd: &std::path::Path) -> Self {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(cwd);
        Self { inner: cmd }
    }

    fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    fn output(&mut self) -> Result<Output> {
        let desc = format!("{:?}", self.inner);
        let out = self
            .inner
            .output()
            .with_context(|| format!("failed to run {desc}"))?;
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg_attr(
        miri,
        ignore = "creates temporary directories via tempfile::tempdir which touches the host filesystem; blocked under Miri isolation"
    )]
    #[test]
    fn git_root_from_path_finds_parent_git() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().join("a").join("b");
        fs::create_dir_all(&base).expect("mkdirs");
        // create .git at temp.path()
        fs::create_dir_all(temp.path().join(".git")).expect("mkdir .git");
        let found = git_root_from_path(&base);
        assert!(found.is_some());
        assert_eq!(found.unwrap(), temp.path().to_path_buf());
    }

    #[cfg(not(miri))]
    #[test]
    fn current_head_commit_returns_hash_for_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // init git repo
        let repo = tmp.path();
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(repo)
            .status()
            .expect("git init");
        fs::write(repo.join("x.txt"), b"hi").expect("write");
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("add")
            .arg(".")
            .status()
            .expect("git add");
        // configure identity for commits in test environment
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("config")
            .arg("user.email")
            .arg("test@example.com")
            .status()
            .expect("git config email");
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("config")
            .arg("user.name")
            .arg("Test User")
            .status()
            .expect("git config name");
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("commit")
            .arg("-m")
            .arg("init")
            .status()
            .expect("git commit");

        let guard_temp = GuardedPath::tempdir().expect("guarded temp");
        let _guard = guard_temp.as_guarded_path().clone();
        // Use current_head_commit directly with the guarded path pointing at repo
        let guarded = GuardedPath::new_root(repo).expect("guard root");
        let got = current_head_commit(&guarded).expect("current_head");
        assert!(got.is_some());
        assert!(!got.unwrap().is_empty());
    }
}
