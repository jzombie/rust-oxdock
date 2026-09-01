Shared test helpers for environment-variable guarding. `TestEnvGuard` sets or
removes an env var, serialises access per key across threads, and restores the
prior state on drop.
