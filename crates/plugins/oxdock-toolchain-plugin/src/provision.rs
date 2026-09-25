//! Toolchain provisioning under the dedicated cache group (issue #179).
//!
//! All toolchain artifacts live under `<cache-root>/toolchain`, resolved
//! through `PathResolver::toolchain_guard` and touched only through a
//! resolver rooted at that guard. Downloads use the same pure-Rust
//! `ureq` plus `rustls` stack as `NET_FETCH`, never host curl or tar.
//! Hashes pin per fetched tarball in `provision.json` (trust on first
//! use, verified after), plus an exact `OXDOCK_TOOLCHAIN_SHA256`
//! override for air-gapped or audited setups.

use anyhow::{Context, Result, bail};
use oxdock_fs::{GuardedPath, PathResolver};
use std::time::Duration;

use crate::targets::{
    PINNED_RUST_VERSION, ZIG_SHA256_ENV, host_exe_name, rust_dist_url, validate_triple,
    zig_bundle_platform, zig_dist_url, zig_version,
};

/// Exact-digest override for the toolchain tarball. When set, the
/// download verifies against it instead of the recorded pin.
pub const TOOLCHAIN_SHA256_ENV: &str = "OXDOCK_TOOLCHAIN_SHA256";

/// Download timeout for toolchain tarballs (hundreds of megabytes).
const DIST_TIMEOUT: Duration = Duration::from_secs(600);

/// Fetch retries for HELPER downloads (transient transport plus HTTP
/// 5xx; 4xx fails immediately like `NET_FETCH`).
const FETCH_RETRIES: u32 = 2;

/// Compression of a cached distributor archive.
#[derive(Clone, Copy)]
enum ArchiveKind {
    Gz,
    Xz,
}

/// Provisioned toolchain binaries and roots, as display strings for MAP
/// returns. Every path sits under the toolchain cache guard.
pub struct ToolchainInfo {
    pub cargo: String,
    pub rustc: String,
    pub sysroot: String,
    pub triple: String,
}

/// Layout roots under the toolchain guard for one triple.
pub struct ToolchainDirs {
    pub dist: GuardedPath,
    pub bin: GuardedPath,
    pub src: GuardedPath,
    pub target: GuardedPath,
}

/// Resolver rooted at the toolchain guard: every path below passes
/// containment, so all I/O stays inside the cache group by construction.
pub fn toolchain_resolver(anchor: &GuardedPath) -> Result<PathResolver> {
    let scratch = PathResolver::new_guarded(anchor.clone(), anchor.clone())
        .context("TOOLCHAIN cannot open the workspace resolver")?;
    let guard = scratch.ensure_toolchain()?;
    PathResolver::new_guarded(guard.clone(), guard)
        .context("TOOLCHAIN cannot root a resolver at the toolchain guard")
}

/// Layout roots for `triple` below `resolver`, which must already root
/// at the toolchain guard. Creates nothing.
pub fn toolchain_dirs(resolver: &PathResolver, triple: &str) -> Result<ToolchainDirs> {
    let guard = resolver.root().clone();
    let dist = guard
        .join(&format!("dist/{triple}"))
        .context("TOOLCHAIN cannot join dist dir")?;
    let bin = guard
        .join(&format!("dist/{triple}/bin"))
        .context("TOOLCHAIN cannot join bin dir")?;
    let src = guard
        .join("src")
        .context("TOOLCHAIN cannot join src dir")?;
    let target = guard
        .join(&format!("target/{triple}"))
        .context("TOOLCHAIN cannot join target dir")?;
    Ok(ToolchainDirs {
        dist,
        bin,
        src,
        target,
    })
}

/// Ensure `triple` is provisioned: layout exists and `cargo` plus
/// `rustc` resolve under `dist/<triple>/bin`. Downloads and unpacks the
/// pinned standalone tarball when the binaries are missing, then returns
/// their paths. Idempotent: present binaries short-circuit before any
/// network or disk mutation beyond directory creation.
pub fn provision_toolchain(anchor: &GuardedPath, triple: &str) -> Result<ToolchainInfo> {
    validate_triple(triple)?;
    let resolver = toolchain_resolver(anchor)?;
    let dirs = toolchain_dirs(&resolver, triple)?;
    for dir in [&dirs.dist, &dirs.bin, &dirs.src, &dirs.target] {
        resolver.create_dir_all(dir)?;
    }
    if let Some(info) = present_toolchain(&resolver, &dirs)? {
        return Ok(info);
    }
    let url = rust_dist_url(PINNED_RUST_VERSION, triple);
    let file_name = format!("rust-standalone-{PINNED_RUST_VERSION}-{triple}.tar.gz");
    let (tarball, ..) = ensure_pinned_file(
        &resolver,
        &dirs.dist,
        &file_name,
        &url,
        PINNED_RUST_VERSION,
        triple,
        "provision.json",
        TOOLCHAIN_SHA256_ENV,
        DIST_TIMEOUT,
    )?;
    let image = dirs
        .dist
        .join("image")
        .context("TOOLCHAIN cannot join image dir")?;
    ensure_unpacked(&resolver, &tarball, &image, ArchiveKind::Gz)?;
    wire_binaries(&resolver, &dirs, &image)
}

/// Fast path: both binaries already present.
fn present_toolchain(
    resolver: &PathResolver,
    dirs: &ToolchainDirs,
) -> Result<Option<ToolchainInfo>> {
    let cargo_name = host_exe_name("cargo");
    let rustc_name = host_exe_name("rustc");
    let cargo = dirs
        .bin
        .join(&cargo_name)
        .context("TOOLCHAIN cannot join cargo")?;
    let rustc = dirs
        .bin
        .join(&rustc_name)
        .context("TOOLCHAIN cannot join rustc")?;
    if resolver.exists(&cargo) && resolver.exists(&rustc) {
        return Ok(Some(ToolchainInfo {
            cargo: cargo.display().to_string(),
            rustc: rustc.display().to_string(),
            sysroot: dirs.dist.display().to_string(),
            triple: triple_of(&dirs.dist)?,
        }));
    }
    Ok(None)
}

/// Derive the triple back from the `dist/<triple>` guard path.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn triple_of(dist: &GuardedPath) -> Result<String> {
    let name = dist
        .as_path()
        .file_name()
        .and_then(|s| s.to_str())
        .context("TOOLCHAIN dist dir has no name")?;
    Ok(name.to_string())
}

/// Stream `url` into an open handle with retry, returning the SHA-256
/// hex of every byte written. Transport errors and HTTP 5xx retry up to
/// `retries` extra attempts; HTTP 4xx fails immediately, mirroring
/// `NET_FETCH` without depending on its private surface.
fn fetch_into(
    url: &str,
    timeout: Duration,
    retries: u32,
    handle: &mut (dyn std::io::Write + Send),
) -> Result<String> {
    let mut attempts = 0u32;
    loop {
        match fetch_once(url, timeout, handle) {
            Ok(digest) => return Ok(digest),
            Err(err) => {
                if attempts >= retries || !is_retryable(&err) {
                    return Err(err);
                }
                attempts += 1;
                std::thread::sleep(Duration::from_millis(100 * u64::from(attempts)));
            }
        }
    }
}

/// Retryable means transport failure or HTTP 5xx. HTTP 4xx, bad URLs,
/// and sink errors fail fast.
fn is_retryable(err: &anyhow::Error) -> bool {
    if err.chain().any(|cause| {
        cause
            .downcast_ref::<ureq::Error>()
            .is_some_and(|e| !matches!(e, ureq::Error::StatusCode(_)))
    }) {
        return true;
    }
    err.to_string().contains("HTTP status 5")
}

/// Single streaming attempt: status gate first, then chunked body into
/// the handle while the digest accumulates. Nothing buffers.
fn fetch_once(
    url: &str,
    timeout: Duration,
    handle: &mut (dyn std::io::Write + Send),
) -> Result<String> {
    use sha2::{Digest, Sha256};
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into();
    let response = agent
        .get(url)
        .call()
        .with_context(|| format!("TOOLCHAIN download of {url:?} failed"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        bail!("TOOLCHAIN download of {url:?} failed with HTTP status {status}");
    }
    let mut reader = response.into_body().into_reader();
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 8192];
    loop {
        use std::io::Read;
        let n = reader
            .read(&mut chunk)
            .with_context(|| format!("TOOLCHAIN download of {url:?} stalled"))?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
        handle
            .write_all(&chunk[..n])
            .context("TOOLCHAIN cannot write the download")?;
    }
    handle.flush().ok();
    Ok(hex::encode(hasher.finalize()))
}

/// Hash a cached file in chunks. Never buffers the whole file.
fn hash_file(resolver: &PathResolver, path: &GuardedPath) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut reader = resolver.open_read(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 8192];
    loop {
        use std::io::Read;
        let n = reader
            .read(&mut chunk)
            .with_context(|| format!("TOOLCHAIN cannot hash {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Ensure a verified distributor file exists in `dir`, crash-safe.
/// A final file with a matching pin is reused with no network. A final
/// file with a mismatching pin bails (upstream changed; delete to
/// re-pin). Anything else downloads to `.part`, verifies, and
/// publishes. Returns `(final_path, fresh)` where fresh means
/// downloaded this run. A killed run leaves at most a `.part` file
/// plus an unpinned final, both reclaimed by re-download on the next
/// run. No interrupted state is ever trusted or pinned.
#[allow(clippy::too_many_arguments)]
fn ensure_pinned_file(
    resolver: &PathResolver,
    dir: &GuardedPath,
    file_name: &str,
    url: &str,
    version: &str,
    subject: &str,
    pin_name: &str,
    env_name: &str,
    timeout: Duration,
) -> Result<(GuardedPath, bool)> {
    let final_path = dir
        .join(file_name)
        .with_context(|| format!("TOOLCHAIN cannot join {file_name}"))?;
    let part_path = dir
        .join(&format!("{file_name}.part"))
        .with_context(|| format!("TOOLCHAIN cannot join {file_name}.part"))?;
    let pin_path = dir
        .join(pin_name)
        .with_context(|| format!("TOOLCHAIN cannot join {pin_name}"))?;
    if resolver.exists(&final_path) && resolver.exists(&pin_path) {
        let observed = hash_file(resolver, &final_path)?;
        let recorded = read_pin_digest(resolver, &pin_path)?;
        if recorded == observed {
            if resolver.exists(&part_path) {
                resolver.remove_file(&part_path)?;
            }
            return Ok((final_path, false));
        }
        bail!(
            "TOOLCHAIN {subject} artifact changed since provisioning (recorded {recorded}, observed {observed}): delete {} to re-pin",
            pin_path.display()
        );
    }
    if resolver.exists(&final_path) {
        resolver.remove_file(&final_path)?;
    }
    if resolver.exists(&part_path) {
        resolver.remove_file(&part_path)?;
    }
    resolver.ensure_parent_dir(&part_path)?;
    let mut handle = resolver.open_write(&part_path)?;
    let observed = fetch_into(url, timeout, FETCH_RETRIES, &mut handle)?;
    drop(handle);
    match std::env::var(env_name)
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(pinned) => {
            let pinned = pinned.to_lowercase();
            if observed != pinned {
                bail!("TOOLCHAIN {subject} sha256 mismatch: expected {pinned}, got {observed}");
            }
        }
        None => {
            if resolver.exists(&pin_path) {
                let recorded = read_pin_digest(resolver, &pin_path)?;
                if recorded != observed {
                    bail!(
                        "TOOLCHAIN {subject} artifact changed since provisioning (recorded {recorded}, observed {observed}): delete {} to re-pin",
                        pin_path.display()
                    );
                }
                // Pin already matches: publish without rewriting it.
                return publish_part(resolver, &part_path, &final_path).map(|()| (final_path, true));
            }
        }
    }
    publish_part(resolver, &part_path, &final_path)?;
    write_pin(resolver, &pin_path, version, subject, url, &observed)?;
    Ok((final_path, true))
}

/// Publish a verified staging file: copy, then remove the staging
/// copy. A kill between the two leaves final-plus-no-pin, which the
/// next run reclaims by re-download.
fn publish_part(
    resolver: &PathResolver,
    part_path: &GuardedPath,
    final_path: &GuardedPath,
) -> Result<()> {
    resolver.copy_file(part_path, final_path)?;
    resolver.remove_file(part_path)?;
    Ok(())
}

/// Record a pin after a verified publish. Never called with
/// unverified bytes.
fn write_pin(
    resolver: &PathResolver,
    pin_path: &GuardedPath,
    version: &str,
    subject: &str,
    url: &str,
    digest: &str,
) -> Result<()> {
    let record = format!(
        "{{\n  \"version\": \"{version}\",\n  \"subject\": \"{subject}\",\n  \"url\": \"{url}\",\n  \"sha256\": \"{digest}\"\n}}\n"
    );
    resolver.write_file(pin_path, record.as_bytes())?;
    Ok(())
}

/// Pull the recorded digest out of a pin record without a JSON parser.
fn read_pin_digest(resolver: &PathResolver, pin_path: &GuardedPath) -> Result<String> {
    let recorded = resolver.read_file(pin_path)?;
    let recorded =
        String::from_utf8(recorded).context("TOOLCHAIN pin record is not UTF-8")?;
    Ok(recorded
        .split("\"sha256\"")
        .nth(1)
        .and_then(|rest| rest.split('"').nth(1))
        .unwrap_or_default()
        .to_string())
}

/// Unpack marker: written only after a complete extraction. A missing
/// marker means the image is partial, so the next run wipes and
/// re-extracts instead of trusting it.
fn unpack_marker(image: &GuardedPath) -> Result<GuardedPath> {
    image
        .join(".unpacked")
        .context("TOOLCHAIN cannot join unpack marker")
}

/// Extract `archive` under `image/` when the unpack marker is absent.
/// A markerless image is wiped first, so kills mid-extraction recover
/// by re-extracting from the verified archive.
fn ensure_unpacked(
    resolver: &PathResolver,
    archive: &GuardedPath,
    image: &GuardedPath,
    kind: ArchiveKind,
) -> Result<()> {
    let marker = unpack_marker(image)?;
    if resolver.exists(&marker) {
        return Ok(());
    }
    if resolver.exists(image) {
        resolver.remove_dir_all(image)?;
    }
    unpack_archive(resolver, archive, image, kind)?;
    resolver.write_file(&marker, b"ok")?;
    Ok(())
}

/// Unpack a distributor archive under `image/`, keeping the vendor
/// tree intact so the binaries keep discovering their sysroot
/// relatively. The single top-level vendor directory is skipped.
/// Directories materialize; regular files stream out; symlinks
/// recreate under containment; anything else (hardlinks, devices,
/// FIFOs) bails loudly instead of corrupting silently.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn unpack_archive(
    resolver: &PathResolver,
    archive: &GuardedPath,
    image: &GuardedPath,
    kind: ArchiveKind,
) -> Result<()> {
    resolver.create_dir_all(image)?;
    match kind {
        ArchiveKind::Gz => {
            let stream = resolver.open_read(archive)?;
            let decoder = flate2::read::GzDecoder::new(stream);
            extract_tar(resolver, decoder, image)?;
        }
        ArchiveKind::Xz => {
            // Pure-Rust xz decodes stream-to-stream, but tar needs a
            // seekable read source, so stage the decompressed bytes to
            // a sibling file first (disk, never RAM) and stream the
            // tar off it. The staging name is deterministic: a kill
            // leaves it behind and the next run overwrites it.
            let staging = stage_xz(resolver, archive)?;
            let stream = resolver.open_read(&staging)?;
            let result = extract_tar(resolver, stream, image);
            let _ = resolver.remove_file(&staging);
            result?;
        }
    }
    Ok(())
}

/// Decompress an xz archive into a deterministic sibling staging file.
fn stage_xz(resolver: &PathResolver, archive: &GuardedPath) -> Result<GuardedPath> {
    let parent = archive
        .parent()
        .context("TOOLCHAIN archive has no parent")?;
    let staging = parent
        .join("archive.tar.part")
        .context("TOOLCHAIN cannot join xz staging path")?;
    let input = resolver.open_read(archive)?;
    let mut output = resolver.open_write(&staging)?;
    let mut buffered = std::io::BufReader::new(input);
    lzma_rs::xz_decompress(&mut buffered, &mut output)
        .map_err(|err| anyhow::anyhow!("TOOLCHAIN cannot xz-decompress {}: {err}", archive.display()))?;
    use std::io::Write;
    output.flush().ok();
    Ok(staging)
}

/// Stream tar entries out of an already-decompressed reader into
/// `image/`, skipping the single top-level vendor directory.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn extract_tar(
    resolver: &PathResolver,
    reader: impl std::io::Read + Send,
    image: &GuardedPath,
) -> Result<()> {
    let mut tar = tar::Archive::new(reader);
    let entries = tar
        .entries()
        .context("TOOLCHAIN cannot list the archive entries")?;
    for entry in entries {
        let mut entry = entry.context("TOOLCHAIN cannot read an archive entry")?;
        let rel: Option<String> = entry
            .path()
            .ok()
            .and_then(|p| {
                p.components()
                    .skip(1)
                    .collect::<std::path::PathBuf>()
                    .to_str()
                    .map(str::to_string)
            })
            .filter(|s| !s.is_empty());
        let Some(rel) = rel else { continue };
        let dest = image
            .join(&rel.replace('\\', "/"))
            .with_context(|| format!("TOOLCHAIN entry escapes the image: {rel}"))?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            resolver.create_dir_all(&dest)?;
        } else if entry_type.is_file() {
            resolver.ensure_parent_dir(&dest)?;
            let mut out = resolver.open_write(&dest)?;
            std::io::copy(&mut entry, &mut out)
                .context("TOOLCHAIN cannot extract an archive entry")?;
        } else if entry_type.is_symlink() {
            extract_symlink(resolver, &mut entry, &dest)?;
        } else {
            bail!(
                "TOOLCHAIN archive entry {rel} has unsupported type {entry_type:?}: refusing to unpack"
            );
        }
    }
    Ok(())
}

/// Recreate a tar symlink under containment. Absolute targets or
/// targets escaping the image bail instead of linking out.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn extract_symlink(
    resolver: &PathResolver,
    entry: &mut tar::Entry<'_, impl std::io::Read>,
    dest: &GuardedPath,
) -> Result<()> {
    let Some(target) = entry.link_name().ok().flatten() else {
        bail!(
            "TOOLCHAIN archive entry {} is a symlink with no target",
            dest.display()
        );
    };
    let Some(target) = target.to_str() else {
        bail!(
            "TOOLCHAIN archive entry {} has a non-UTF8 link target",
            dest.display()
        );
    };
    let Some(parent) = dest.parent() else {
        bail!(
            "TOOLCHAIN archive entry {} has no parent",
            dest.display()
        );
    };
    let link_src = parent
        .join(&target.replace('\\', "/"))
        .with_context(|| {
            format!(
                "TOOLCHAIN symlink target escapes the image: {target}"
            )
        })?;
    resolver.ensure_parent_dir(dest)?;
    if resolver.exists(dest) {
        resolver.remove_file(dest)?;
    }
    resolver.symlink(&link_src, dest)?;
    Ok(())
}

/// Locate `cargo` and `rustc` under the image and publish them into
/// `bin/`. Both must sit under a `bin/` parent so the pair reads as a
/// deliberate toolchain, not a stray match. Names follow the host
/// platform: the published binaries execute locally.
fn wire_binaries(
    resolver: &PathResolver,
    dirs: &ToolchainDirs,
    image: &GuardedPath,
) -> Result<ToolchainInfo> {
    let cargo_name = host_exe_name("cargo");
    let rustc_name = host_exe_name("rustc");
    let cargo_src = find_binned(resolver, image, &cargo_name, 8)?.with_context(|| {
        format!("TOOLCHAIN image has no bin/cargo under {}", image.display())
    })?;
    let rustc_src = find_binned(resolver, image, &rustc_name, 8)?.with_context(|| {
        format!("TOOLCHAIN image has no bin/rustc under {}", image.display())
    })?;
    for (src, name) in [(&cargo_src, cargo_name.as_str()), (&rustc_src, rustc_name.as_str())] {
        let dest = dirs
            .bin
            .join(name)
            .with_context(|| format!("TOOLCHAIN cannot join bin/{name}"))?;
        let bytes = resolver.read_file(src)?;
        resolver.write_file(&dest, &bytes)?;
        #[cfg(unix)]
        resolver.set_permissions_mode_unix(&dest, 0o755)?;
    }
    let cargo = dirs
        .bin
        .join(&cargo_name)
        .context("TOOLCHAIN cannot join cargo")?;
    let rustc = dirs
        .bin
        .join(&rustc_name)
        .context("TOOLCHAIN cannot join rustc")?;
    Ok(ToolchainInfo {
        cargo: cargo.display().to_string(),
        rustc: rustc.display().to_string(),
        sysroot: dirs.dist.display().to_string(),
        triple: triple_of(&dirs.dist)?,
    })
}

/// Provisioned portable linker: the cached zig binary path.
pub struct LinkerInfo {
    pub zig: String,
}

/// Ensure the portable linker is provisioned: a pinned zig bundle for
/// the host platform under `<guard>/linker/`, verified and unpacked
/// with the same crash-safe flow as toolchains. Returns the zig binary
/// path. Bails explicitly on hosts without a zig bundle (Windows).
pub fn provision_linker(anchor: &GuardedPath) -> Result<LinkerInfo> {
    let (os, arch) = zig_bundle_platform()?;
    let version = zig_version();
    let resolver = toolchain_resolver(anchor)?;
    let home = resolver
        .root()
        .join("linker")
        .and_then(|linker| linker.join(&format!("zig-{version}-{os}-{arch}")))
        .context("TOOLCHAIN cannot join linker dir")?;
    resolver.create_dir_all(&home)?;
    let exe = host_exe_name("zig");
    if let Some(hit) = find_top_file(&resolver, &home, &exe, 3)? {
        return Ok(LinkerInfo {
            zig: hit.display().to_string(),
        });
    }
    let file_name = format!("zig-{os}-{arch}-{version}.tar.xz");
    let url = zig_dist_url(&version, &os, &arch);
    let (bundle, _) = ensure_pinned_file(
        &resolver,
        &home,
        &file_name,
        &url,
        &version,
        &format!("zig-{os}-{arch}"),
        "linker.json",
        ZIG_SHA256_ENV,
        DIST_TIMEOUT,
    )?;
    let image = home
        .join("image")
        .context("TOOLCHAIN cannot join linker image dir")?;
    ensure_unpacked(&resolver, &bundle, &image, ArchiveKind::Xz)?;
    let hit = find_top_file(&resolver, &image, &exe, 3)?.with_context(|| {
        format!("TOOLCHAIN linker image has no {exe} under {}", image.display())
    })?;
    #[cfg(unix)]
    resolver.set_permissions_mode_unix(&hit, 0o755)?;
    Ok(LinkerInfo {
        zig: hit.display().to_string(),
    })
}

/// Depth-capped walk for `file` directly under a `bin/` parent.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn find_binned(
    resolver: &PathResolver,
    dir: &GuardedPath,
    file: &str,
    depth: usize,
) -> Result<Option<GuardedPath>> {
    if depth == 0 {
        return Ok(None);
    }
    let mut entries = resolver.read_dir_entries(dir)?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let abs = entry.path();
        let Some(wrapped) = GuardedPath::new(dir.root(), &abs).ok() else {
            continue;
        };
        if resolver.exists(&wrapped) && matches!(resolver.entry_kind(&wrapped), Ok(oxdock_fs::EntryKind::Dir)) {
            if name == "bin" {
                let mut inner_entries = resolver.read_dir_entries(&wrapped)?;
                inner_entries.sort_by_key(|entry| entry.path());
                for inner in inner_entries {
                    let inner_name = inner.file_name();
                    if inner_name.to_str().is_some_and(|n| n == file) {
                        let abs = inner.path();
                        if let Ok(hit) = GuardedPath::new(dir.root(), &abs) {
                            return Ok(Some(hit));
                        }
                    }
                }
            } else if let Some(found) = find_binned(resolver, &wrapped, file, depth - 1)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

/// Depth-capped walk for a top-level `file` anywhere below `dir`
/// (zig ships its binary at the bundle root, not under `bin/`).
/// Sorted for deterministic discovery.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn find_top_file(
    resolver: &PathResolver,
    dir: &GuardedPath,
    file: &str,
    depth: usize,
) -> Result<Option<GuardedPath>> {
    if depth == 0 {
        return Ok(None);
    }
    let mut entries = resolver.read_dir_entries(dir)?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let abs = entry.path();
        let Some(wrapped) = GuardedPath::new(dir.root(), &abs).ok() else {
            continue;
        };
        if !resolver.exists(&wrapped) {
            continue;
        }
        if matches!(
            resolver.entry_kind(&wrapped),
            Ok(oxdock_fs::EntryKind::Dir)
        ) {
            if name == "bin" || name == "lib" || name == "doc" {
                continue;
            }
            if let Some(found) = find_top_file(resolver, &wrapped, file, depth - 1)? {
                return Ok(Some(found));
            }
        } else if name == file {
            return Ok(Some(wrapped));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an in-memory `tar.xz` fixture: top vendor dir with a fake
    /// zig binary, a lib file, a relative symlink, and one absolute
    /// symlink that must fail closed.
    fn fixture_bundle(absolute_link: bool) -> Vec<u8> {
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);
            let mut dir_header = tar::Header::new_gnu();
            dir_header.set_path("top-1.0").expect("dir path");
            dir_header.set_entry_type(tar::EntryType::Directory);
            dir_header.set_size(0);
            dir_header.set_mode(0o755);
            dir_header.set_cksum();
            builder
                .append(&dir_header, &[][..])
                .expect("append dir");
            let mut zig_header = tar::Header::new_gnu();
            zig_header.set_path("top-1.0/zig").expect("zig path");
            zig_header.set_size(4);
            zig_header.set_mode(0o755);
            zig_header.set_cksum();
            builder
                .append(&zig_header, &b"zig!"[..])
                .expect("append zig");
            let mut link_header = tar::Header::new_gnu();
            link_header.set_path("top-1.0/zig-link").expect("link path");
            link_header.set_entry_type(tar::EntryType::Symlink);
            link_header.set_size(0);
            if absolute_link {
                link_header
                    .set_link_name("/etc/hostname")
                    .expect("absolute link");
            } else {
                link_header.set_link_name("zig").expect("relative link");
            }
            link_header.set_cksum();
            builder.append(&link_header, &[][..]).expect("append link");
            builder.into_inner().expect("finish tar");
        }
        let mut encoder_output = Vec::new();
        lzma_rs::xz_compress(&mut &tar_data[..], &mut encoder_output).expect("xz encode");
        encoder_output
    }

    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn scratch_resolver() -> (GuardedPath, PathResolver, oxdock_fs::GuardedTempDir) {
        let temp = GuardedPath::tempdir().expect("tempdir");
        let root = temp.as_guarded_path().clone();
        let resolver =
            PathResolver::new(root.as_path(), root.as_path()).expect("resolver");
        (root, resolver, temp)
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "needs OS tempdirs and real compression; blocked under Miri isolation"
    )]
    fn xz_unpack_restores_files_and_relative_symlinks() {
        let (_root, resolver, _temp) = scratch_resolver();
        let bundle = resolver
            .root()
            .join("zig.tar.xz")
            .expect("bundle join");
        resolver
            .write_file(&bundle, &fixture_bundle(false))
            .expect("write bundle");
        let image = resolver.root().join("image").expect("image join");
        unpack_archive(&resolver, &bundle, &image, ArchiveKind::Xz)
            .expect("unpack");
        let zig = image.join("zig").expect("zig join");
        assert_eq!(resolver.read_file(&zig).expect("read zig"), b"zig!");
        // Guard re-joins canonicalize through just-created symlinks, so
        // symlink-ness asserts against the raw join instead of a
        // re-guarded path.
        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        let is_link = {
            let raw = image.as_path().join("zig-link");
            std::fs::symlink_metadata(&raw)
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(false)
        };
        assert!(is_link, "relative symlink recreates as a symlink");
        let marker = image.join(".unpacked").expect("marker join");
        assert!(
            !resolver.exists(&marker),
            "unpack_archive leaves marking to ensure_unpacked"
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "needs OS tempdirs and real compression; blocked under Miri isolation"
    )]
    fn xz_unpack_rejects_escaping_symlinks() {
        let (_root, resolver, _temp) = scratch_resolver();
        let bundle = resolver
            .root()
            .join("evil.tar.xz")
            .expect("bundle join");
        resolver
            .write_file(&bundle, &fixture_bundle(true))
            .expect("write bundle");
        let image = resolver.root().join("image").expect("image join");
        let err = unpack_archive(&resolver, &bundle, &image, ArchiveKind::Xz)
            .expect_err("absolute symlink must fail");
        assert!(err.to_string().contains("escapes the image"), "{err:#}");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "needs OS tempdirs; blocked under Miri isolation"
    )]
    fn ensure_unpacked_recovers_from_partial_image() {
        let (_root, resolver, _temp) = scratch_resolver();
        let bundle = resolver
            .root()
            .join("zig.tar.xz")
            .expect("bundle join");
        resolver
            .write_file(&bundle, &fixture_bundle(false))
            .expect("write bundle");
        let image = resolver.root().join("image").expect("image join");
        // Simulate a killed run: partial image, no marker.
        resolver.create_dir_all(&image).expect("partial image");
        let partial = image.join("half").expect("partial join");
        resolver
            .write_file(&partial, b"partial")
            .expect("write partial");
        ensure_unpacked(&resolver, &bundle, &image, ArchiveKind::Xz)
            .expect("recover");
        assert!(
            !resolver.exists(&partial),
            "partial image wipes before re-extract"
        );
        assert!(
            resolver.exists(&image.join(".unpacked").expect("marker join")),
            "marker lands only after a complete extraction"
        );
    }
}
