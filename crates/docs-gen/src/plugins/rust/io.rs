use anyhow::{Context, Result};
use oxdock_fs::{GuardedPath, PathResolver};

/// Read text through the guarded resolver.
pub fn read_text(resolver: &PathResolver, root: &GuardedPath, rel: &str) -> Result<String> {
    let path = root.join(rel)?;
    resolver
        .read_to_string(&path)
        .with_context(|| format!("read {rel}"))
}
