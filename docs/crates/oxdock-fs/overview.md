Guarded filesystem and workspace utilities. `GuardedPath` guarantees that
every operation stays within a declared root; `PathResolver` abstracts read,
write, copy, and directory-creation behind a trait so host syscalls are
isolated in one crate. Tempdir PID-lock GC keeps stale `oxdock-*` directories
clean across runs.
