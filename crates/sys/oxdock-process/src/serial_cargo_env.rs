use oxdock_fs::GuardedPath;
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII helper that serializes and scopes mutations of `CARGO_*` environment variables.
///
/// Tests that simulate different crate layouts frequently need to tweak `CARGO_MANIFEST_DIR`
/// and `CARGO_PRIMARY_PACKAGE`. Those environment variables are global, so concurrent mutations
/// can introduce racy failures. `SerialCargoEnv` acquires a process-wide mutex before mutating
/// the variables, remembers the previous values, and restores them when dropped so each test can
/// safely run in isolation.
pub struct SerialCargoEnv<'a> {
    _lock: MutexGuard<'a, ()>,
    prev_manifest: Option<String>,
    prev_primary: Option<String>,
}

impl<'a> SerialCargoEnv<'a> {
    /// Acquire the guard and set the cargo environment to point at `manifest_dir`.
    ///
    /// `primary` controls whether `CARGO_PRIMARY_PACKAGE` is set to `"1"` or `"0"`.
    /// The previous values are restored automatically when the guard drops.
    pub fn new(manifest_dir: &GuardedPath, primary: bool) -> Self {
        // A panicked sibling test must not cascade poison into every later
        // guard user; the critical section only touches process env state.
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prev_manifest = std::env::var("CARGO_MANIFEST_DIR").ok();
        let prev_primary = std::env::var("CARGO_PRIMARY_PACKAGE").ok();
        // SAFETY: std::env setters are marked unsafe due to global mutation, but we serialize
        // access via `ENV_LOCK` to keep mutations ordered and scoped by this guard.
        unsafe {
            std::env::set_var("CARGO_MANIFEST_DIR", manifest_dir.as_path());
            std::env::set_var("CARGO_PRIMARY_PACKAGE", if primary { "1" } else { "0" });
        }
        Self {
            _lock: lock,
            prev_manifest,
            prev_primary,
        }
    }
}

impl Drop for SerialCargoEnv<'_> {
    fn drop(&mut self) {
        unsafe {
            if let Some(prev) = &self.prev_manifest {
                std::env::set_var("CARGO_MANIFEST_DIR", prev);
            } else {
                std::env::remove_var("CARGO_MANIFEST_DIR");
            }

            if let Some(prev) = &self.prev_primary {
                std::env::set_var("CARGO_PRIMARY_PACKAGE", prev);
            } else {
                std::env::remove_var("CARGO_PRIMARY_PACKAGE");
            }
        }
    }
}

/// Convenience wrapper that constructs a [`SerialCargoEnv`].
pub fn manifest_env_guard<'a>(manifest_dir: &'a GuardedPath, primary: bool) -> SerialCargoEnv<'a> {
    SerialCargoEnv::new(manifest_dir, primary)
}

#[cfg(test)]
impl SerialCargoEnv<'_> {
    /// Test-only view of the state captured at construction time, so tests
    /// can assert restoration without racing ambient environment reads.
    pub fn previous(&self) -> (Option<String>, Option<String>) {
        (self.prev_manifest.clone(), self.prev_primary.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::manifest_env_guard;
    use oxdock_fs::GuardedPath;

    #[test]
    fn guard_sets_manifest_dir_and_primary_flag() {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();

        {
            let _guard = manifest_env_guard(&root, true);
            let expected = root.as_path().to_string_lossy().into_owned();
            assert_eq!(
                std::env::var("CARGO_MANIFEST_DIR").as_deref(),
                Ok(expected.as_str())
            );
            assert_eq!(std::env::var("CARGO_PRIMARY_PACKAGE").as_deref(), Ok("1"));
        }

        {
            let _guard = manifest_env_guard(&root, false);
            assert_eq!(std::env::var("CARGO_PRIMARY_PACKAGE").as_deref(), Ok("0"));
        }
    }

    #[test]
    fn guard_captures_and_restores_prior_state() {
        let temp_a = GuardedPath::tempdir().expect("tempdir");
        let temp_b = GuardedPath::tempdir().expect("tempdir");
        let root_a = temp_a.as_guarded_path().clone();
        let root_b = temp_b.as_guarded_path().clone();

        // Probe pass: capture the ambient state under the lock.
        let expected = {
            let probe = manifest_env_guard(&root_a, true);
            probe.previous()
        };

        // Second construction must observe exactly the restored ambient state.
        {
            let guard = manifest_env_guard(&root_b, false);
            assert_eq!(guard.previous(), expected);
            let expected_dir = root_b.as_path().to_string_lossy().into_owned();
            assert_eq!(
                std::env::var("CARGO_MANIFEST_DIR").as_deref(),
                Ok(expected_dir.as_str())
            );
        }
    }
}
