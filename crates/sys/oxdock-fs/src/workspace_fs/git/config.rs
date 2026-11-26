use crate::diagnostics as fs_diag;
use crate::workspace_fs::GuardedPath;
#[cfg(not(miri))]
use anyhow::Context;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
}

#[cfg(miri)]
pub fn ensure_git_identity(_repo: &GuardedPath) -> Result<GitIdentity> {
    Err(anyhow::Error::new(fs_diag::git_identity_miri()))
}

#[cfg(not(miri))]
/// Ensures the repository has a usable `user.name`/`user.email` configuration.
/// Existing repo/local/global/system settings win; we fall back to common
/// environment variables and, as a last resort, deterministic defaults so CI
/// or fixture repositories can commit without bespoke setup.
pub fn ensure_git_identity(repo: &GuardedPath) -> Result<GitIdentity> {
    let mut name = read_config(repo, "user.name")?;
    let mut email = read_config(repo, "user.email")?;
    let needs_name_config = name.is_none();
    let needs_email_config = email.is_none();

    if name.is_none() {
        name = env_fallback(&["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME", "USER", "USERNAME"]);
    }
    if email.is_none() {
        email = env_fallback(&["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL", "EMAIL"]);
    }

    let name = name.unwrap_or_else(|| "Test User".to_string());
    let email = email.unwrap_or_else(|| "test@example.com".to_string());

    if needs_name_config {
        write_config(repo, "user.name", &name)?;
    }
    if needs_email_config {
        write_config(repo, "user.email", &email)?;
    }

    Ok(GitIdentity { name, email })
}

#[cfg(not(miri))]
fn read_config(repo: &GuardedPath, key: &str) -> Result<Option<String>> {
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.as_path())
        .arg("config")
        .arg("--get")
        .arg(key)
        .output()
        .with_context(|| format!("failed to query git config for {key}"))?;

    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if value.is_empty() {
            return Ok(None);
        }
        return Ok(Some(value));
    }

    match output.status.code() {
        Some(1) => Ok(None),
        _ => Err(anyhow::Error::new(fs_diag::git_config_get_failed(
            key,
            output.status,
        ))),
    }
}

#[cfg(not(miri))]
fn write_config(repo: &GuardedPath, key: &str, value: &str) -> Result<()> {
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.as_path())
        .arg("config")
        .arg("--local")
        .arg(key)
        .arg(value)
        .status()
        .with_context(|| format!("failed to set git config {key}"))?;
    if !status.success() {
        return Err(anyhow::Error::new(fs_diag::git_config_set_failed(
            key, status,
        )));
    }
    Ok(())
}

#[cfg(not(miri))]
fn env_fallback(keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::super::GitCommand;
    use super::*;

    use oxdock_sys_test_utils::TestEnvGuard;

    fn with_git_config_isolation(f: impl FnOnce()) {
        let _g = TestEnvGuard::set("GIT_CONFIG_GLOBAL", "/dev/null");
        let _s = TestEnvGuard::set("GIT_CONFIG_SYSTEM", "/dev/null");
        f();
    }

    fn init_repo(repo: &GuardedPath) {
        let mut cmd = GitCommand::new(repo.as_path());
        cmd.arg("init");
        let output = cmd.output().expect("run git init");
        assert!(
            output.status.success(),
            "git init failed: {}",
            output.status
        );
    }

    #[test]
    fn ensure_git_identity_writes_defaults_when_missing() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let repo = temp.as_guarded_path().clone();
        init_repo(&repo);

        with_git_config_isolation(|| {
            let ident = ensure_git_identity(&repo).expect("identity");
            assert!(!ident.name.is_empty());
            assert!(!ident.email.is_empty());
        });
    }

    #[test]
    fn ensure_git_identity_reads_env_fallback() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let repo = temp.as_guarded_path().clone();
        init_repo(&repo);

        with_git_config_isolation(|| {
            let _n = TestEnvGuard::set("GIT_AUTHOR_NAME", "CI Bot");
            let _e = TestEnvGuard::set("GIT_AUTHOR_EMAIL", "ci@example.com");
            let ident = ensure_git_identity(&repo).expect("identity");
            assert_eq!(ident.name, "CI Bot");
            assert_eq!(ident.email, "ci@example.com");
        });
    }
}
