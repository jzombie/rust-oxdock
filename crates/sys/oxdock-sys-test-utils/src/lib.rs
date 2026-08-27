/// Shared test helpers used by multiple crates' tests.
///
/// Keep functionality here minimal and test-only.
use std::collections::HashMap;
use std::env;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::{ThreadId, current};

/// Per-key locks so simultaneous guards for *different* variables keep
/// working, while mutations of the *same* variable serialize process-wide.
/// `std::env` access mutates process-global state and is not thread-safe,
/// so every guard holds its key's lock for its entire lifetime.
static KEY_LOCKS: OnceLock<Mutex<HashMap<&'static str, &'static Mutex<()>>>> = OnceLock::new();

/// Which thread currently holds each key's lock, so same-thread re-entrancy
/// can fail fast with a clear message instead of deadlocking.
static KEY_OWNERS: OnceLock<Mutex<HashMap<&'static str, ThreadId>>> = OnceLock::new();

fn key_owners() -> &'static Mutex<HashMap<&'static str, ThreadId>> {
    KEY_OWNERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[allow(clippy::disallowed_methods)] // Box::leak: one lock per distinct key; bounded by test-suite vocabulary
fn key_lock(key: &'static str) -> &'static Mutex<()> {
    let locks = KEY_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(key)
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
}

fn acquire_key_lock(key: &'static str) -> MutexGuard<'static, ()> {
    let thread = current();
    {
        let owners = key_owners()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owners.get(key) == Some(&thread.id()) {
            panic!("TestEnvGuard: environment variable {key} is already guarded on this thread");
        }
    }

    let guard = key_lock(key)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    key_owners()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, thread.id());
    guard
}

pub struct TestEnvGuard {
    key: &'static str,
    value: Option<String>,
    _lock: MutexGuard<'static, ()>,
}

impl TestEnvGuard {
    /// Set `key` to `value`, restoring the prior state on drop.
    ///
    /// Blocks while another thread holds a live guard for the same variable;
    /// panics if the same thread nests a second guard for it.
    pub fn set(key: &'static str, value: &str) -> Self {
        let lock = acquire_key_lock(key);
        let prev = env::var(key).ok();
        unsafe { env::set_var(key, value) };
        Self {
            key,
            value: prev,
            _lock: lock,
        }
    }

    /// Remove `key` from the environment, restoring the prior state on drop:
    /// re-set to its previous value if it had one, otherwise left absent.
    ///
    /// Blocking/panic behavior matches [`TestEnvGuard::set`].
    pub fn remove(key: &'static str) -> Self {
        let lock = acquire_key_lock(key);
        let prev = env::var(key).ok();
        unsafe { env::remove_var(key) };
        Self {
            key,
            value: prev,
            _lock: lock,
        }
    }
}

impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        match &self.value {
            Some(value) => unsafe { env::set_var(self.key, value) },
            None => unsafe { env::remove_var(self.key) },
        }
        key_owners()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(self.key);
    }
}

#[allow(clippy::disallowed_types)]
use std::path::Path;

/// Detect whether the current process can create filesystem symlinks under
/// the provided target directory. Accepts a `&Path` to avoid depending on
/// `oxdock-fs` and creating a circular crate dependency.
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
pub fn can_create_symlinks(target: &Path) -> bool {
    #[cfg(unix)]
    {
        let _ = target;
        true
    }

    #[cfg(windows)]
    {
        use std::fs;
        use std::os::windows::fs::symlink_dir;
        let test_src = target.join("__oxdock_test_symlink_src");
        let test_dst = target.join("__oxdock_test_symlink_dst");
        // Localized allowance: sys test helper may create/remove test dirs.
        let _ = fs::create_dir_all(&test_src);
        let ok = symlink_dir(&test_src, &test_dst).is_ok();
        let _ = fs::remove_dir_all(&test_dst);
        let _ = fs::remove_dir_all(&test_src);
        ok
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = target;
        false
    }
}

/// Build a process [`std::process::ExitStatus`] from a raw exit code.
///
/// Single definition shared by the mock manager, the Miri synthetic backend,
/// and executor tests (all need to fabricate statuses without spawning).
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
pub fn exit_status_from_code(code: i32) -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatusExt::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatusExt::from_raw(code as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::TestEnvGuard;
    use std::env;

    const SET_RESTORE_ABSENT: &str = "OXDOCK_SYS_TEST_UTILS_SET_RESTORE_ABSENT";
    const SET_RESTORE_PREVIOUS: &str = "OXDOCK_SYS_TEST_UTILS_SET_RESTORE_PREVIOUS";
    const REMOVE_RESTORE: &str = "OXDOCK_SYS_TEST_UTILS_REMOVE_RESTORE";
    const KEY_A: &str = "OXDOCK_SYS_TEST_UTILS_KEY_A";
    const KEY_B: &str = "OXDOCK_SYS_TEST_UTILS_KEY_B";
    const NESTED: &str = "OXDOCK_SYS_TEST_UTILS_NESTED";

    #[test]
    fn set_guard_restores_absent_state_on_drop() {
        drop(TestEnvGuard::remove(SET_RESTORE_ABSENT));

        let guard = TestEnvGuard::set(SET_RESTORE_ABSENT, "value");
        assert_eq!(env::var(SET_RESTORE_ABSENT).as_deref(), Ok("value"));
        drop(guard);

        assert!(env::var(SET_RESTORE_ABSENT).is_err());
    }

    #[test]
    fn set_guard_restores_previous_value_on_drop() {
        // Arrange a pre-existing value directly; unique key => no other test
        // touches it and no guard is held for it here.
        unsafe { env::set_var(SET_RESTORE_PREVIOUS, "original") };

        let guard = TestEnvGuard::set(SET_RESTORE_PREVIOUS, "temporary");
        assert_eq!(env::var(SET_RESTORE_PREVIOUS).as_deref(), Ok("temporary"));
        drop(guard);

        assert_eq!(env::var(SET_RESTORE_PREVIOUS).as_deref(), Ok("original"));
        unsafe { env::remove_var(SET_RESTORE_PREVIOUS) };
    }

    #[test]
    fn remove_guard_restores_previous_value_on_drop() {
        unsafe { env::set_var(REMOVE_RESTORE, "keep-me") };

        let guard = TestEnvGuard::remove(REMOVE_RESTORE);
        assert!(env::var(REMOVE_RESTORE).is_err());
        drop(guard);

        assert_eq!(env::var(REMOVE_RESTORE).as_deref(), Ok("keep-me"));
        unsafe { env::remove_var(REMOVE_RESTORE) };
    }

    #[test]
    fn guards_for_different_keys_coexist() {
        let a = TestEnvGuard::set(KEY_A, "1");
        let b = TestEnvGuard::set(KEY_B, "2");

        assert_eq!(env::var(KEY_A).as_deref(), Ok("1"));
        assert_eq!(env::var(KEY_B).as_deref(), Ok("2"));

        drop(b);
        drop(a);
    }

    #[test]
    #[should_panic(expected = "already guarded")]
    fn same_key_nesting_panics_rather_than_deadlocking() {
        let _outer = TestEnvGuard::set(NESTED, "outer");
        let _inner = TestEnvGuard::set(NESTED, "inner");
    }
}
