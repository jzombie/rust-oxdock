//! Stable host key load-or-create behind the workspace guard.
//!
//! `SSH_SERVE` with `key_path` set keeps one Ed25519 host key across
//! restarts so `known_hosts` stays valid. Empty or absent `key_path`
//! keeps the traceless ephemeral key (fresh in memory per call).
//! Creation is atomic `0600` at open time (never write-then-chmod);
//! pre-existing files load only with safe Unix permissions. Losers of a
//! first-start race re-read briefly in case they caught a partial write.

use anyhow::{Context, Result, bail};
use oxdock_core::StepCtx;
use oxdock_fs::{GuardedPath, PathResolver};
use oxdock_process::ProcessManager;
use russh::keys::{Algorithm, PrivateKey};

/// Load or create the Ed25519 host key for `SSH_SERVE`. Returns `None`
/// when `key_path` is absent or blank (ephemeral in-memory key).
pub fn load_or_create_host_key<P: ProcessManager>(
    cx: &StepCtx<P>,
    func: &str,
    key_path: Option<String>,
) -> Result<Option<PrivateKey>> {
    let Some(raw) = key_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let root = cx.cwd().root();
    let guarded = GuardedPath::new_root(root)
        .with_context(|| format!("{func} cannot guard the workspace root"))?
        .join(raw)
        .with_context(|| format!("{func} key_path escapes the workspace: {raw}"))?;
    let resolver =
        PathResolver::new(root, root).context("SSH_SERVE cannot open the workspace resolver")?;
    if !resolver.exists(&guarded) {
        let key = PrivateKey::random(&mut rand10::rng(), Algorithm::Ed25519)
            .context("generate Ed25519 host key")?;
        let pem = key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .context("encode Ed25519 host key")?;
        match resolver.write_private_file(&guarded, pem.as_bytes()) {
            Ok(()) => return Ok(Some(key)),
            Err(err) if is_io_kind(&err, std::io::ErrorKind::AlreadyExists) => {
                // Lost the first-start race: the winner creates the file
                // empty (`create_new`) and fills it after, so a first
                // read here can catch a partial PEM. Retry briefly; a
                // temp-file-plus-rename would avoid the window but break
                // the single-winner invariant (last-writer-wins leaves
                // the loser's in-memory key diverging from the file).
                return read_existing_key_retry(&resolver, &guarded, func);
            }
            Err(err) => {
                // We created the file but failed to fill it; remove the
                // partial so the next boot regenerates instead of
                // tripping the parse gate on our own rubbish.
                let _ = resolver.remove_file(&guarded);
                return Err(err);
            }
        }
    }
    // Every read below is retry-guarded: whether this caller lost the
    // `create_new` race above or observed `exists() == true` while the
    // winner is still filling the PEM bytes, a first parse failure may
    // be transient.
    read_existing_key_retry(&resolver, &guarded, func)
}

/// Bounded re-read for every host-key load: the winner's PEM bytes land
/// microseconds after the file appears, so a parse failure may be
/// transient regardless of which path the caller took (lost `create_new`
/// race, or `exists()` already true mid-write). Retry briefly, then
/// surface the last error unchanged (deterministic errors such as bad
/// permissions are still reported, just ~50ms later).
fn read_existing_key_retry(
    resolver: &PathResolver,
    guarded: &GuardedPath,
    func: &str,
) -> Result<Option<PrivateKey>> {
    let mut attempts = 0;
    loop {
        match read_existing_key(resolver, guarded, func) {
            Ok(key) => return Ok(key),
            Err(_) if attempts < 5 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => return Err(err),
        }
    }
}

/// Validate permissions and parse a pre-existing host key file.
fn read_existing_key(
    resolver: &PathResolver,
    guarded: &GuardedPath,
    func: &str,
) -> Result<Option<PrivateKey>> {
    let meta = resolver
        .metadata(guarded)
        .with_context(|| format!("cannot stat host key {}", guarded.display()))?;
    assert_safe_mode(guarded, &meta, func)?;
    let bytes = resolver
        .read_file(guarded)
        .with_context(|| format!("cannot read host key {}", guarded.display()))?;
    let key = PrivateKey::from_openssh(&bytes)
        .with_context(|| format!("cannot parse host key {}", guarded.display()))?;
    if key.algorithm() != Algorithm::Ed25519 {
        bail!(
            "{func} host key {} must be Ed25519, found {:?}",
            guarded.display(),
            key.algorithm()
        );
    }
    Ok(Some(key))
}

/// Bail when group or world have any access to a pre-existing key file
/// (Unix-only; Windows ACLs are a follow-up).
fn assert_safe_mode(guarded: &GuardedPath, meta: &std::fs::Metadata, func: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{func} insecure permissions on host key file {}: expected 0600, got 0{mode:o}",
                guarded.display()
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (guarded, meta, func);
    }
    Ok(())
}

/// True when any error in the chain is a std IO error of `kind` (survives
/// anyhow context wrapping).
fn is_io_kind(err: &anyhow::Error, kind: std::io::ErrorKind) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == kind)
    })
}
