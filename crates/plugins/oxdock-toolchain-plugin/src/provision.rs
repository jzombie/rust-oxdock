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

use crate::targets::{PINNED_RUST_VERSION, host_exe_name, rust_dist_url, validate_triple};

/// Exact-digest override for the toolchain tarball. When set, the
/// download verifies against it instead of the recorded pin.
pub const TOOLCHAIN_SHA256_ENV: &str = "OXDOCK_TOOLCHAIN_SHA256";

/// Download timeout for toolchain tarballs (hundreds of megabytes).
const DIST_TIMEOUT: Duration = Duration::from_secs(600);

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
    let tarball = dirs
        .dist
        .join(&format!(
            "rust-standalone-{PINNED_RUST_VERSION}-{triple}.tar.gz"
        ))
        .context("TOOLCHAIN cannot join tarball path")?;
    let fresh = !resolver.exists(&tarball);
    if fresh {
        download_dist(&resolver, &tarball, &url)?;
    }
    verify_dist(&resolver, &tarball, &url, triple)?;
    let image = dirs
        .dist
        .join("image")
        .context("TOOLCHAIN cannot join image dir")?;
    if fresh || !resolver.exists(&image) {
        unpack_dist(&resolver, &tarball, &image)?;
    }
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

/// Stream the standalone tarball into the dist dir.
fn download_dist(resolver: &PathResolver, tarball: &GuardedPath, url: &str) -> Result<()> {
    use std::io::{Read, Write};
    resolver.ensure_parent_dir(tarball)?;
    let mut handle = resolver.open_write(tarball)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(DIST_TIMEOUT))
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
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader
            .read(&mut chunk)
            .with_context(|| format!("TOOLCHAIN download of {url:?} stalled"))?;
        if n == 0 {
            break;
        }
        handle
            .write_all(&chunk[..n])
            .context("TOOLCHAIN cannot write the downloaded tarball")?;
    }
    handle.flush().ok();
    Ok(())
}

/// Verify the tarball digest: the exact env override wins, then the
/// recorded pin, otherwise the observed digest is recorded.
fn verify_dist(
    resolver: &PathResolver,
    tarball: &GuardedPath,
    url: &str,
    triple: &str,
) -> Result<()> {
    let bytes = resolver.read_file(tarball)?;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let observed = hex::encode(hasher.finalize());
    if let Ok(pinned) = std::env::var(TOOLCHAIN_SHA256_ENV) {
        let pinned = pinned.trim().to_lowercase();
        if observed != pinned {
            bail!("TOOLCHAIN tarball sha256 mismatch: expected {pinned}, got {observed}");
        }
        return Ok(());
    }
    let dist = tarball
        .parent()
        .context("TOOLCHAIN tarball has no parent")?;
    let pin_path = pin_path_for(&dist)?;
    if resolver.exists(&pin_path) {
        let recorded = resolver.read_file(&pin_path)?;
        let recorded = String::from_utf8(recorded).context("TOOLCHAIN pin record is not UTF-8")?;
        let digest = pin_digest(&recorded);
        if digest != observed {
            bail!(
                "TOOLCHAIN tarball changed since provisioning (recorded {digest}, observed {observed}): delete {} to re-pin",
                pin_path.display()
            );
        }
        return Ok(());
    }
    let record = format!(
        "{{\n  \"version\": \"{PINNED_RUST_VERSION}\",\n  \"triple\": \"{triple}\",\n  \"url\": \"{url}\",\n  \"sha256\": \"{observed}\"\n}}\n"
    );
    resolver.write_file(&pin_path, record.as_bytes())?;
    Ok(())
}

/// Pin record path for a dist dir.
fn pin_path_for(dist: &GuardedPath) -> Result<GuardedPath> {
    dist.join("provision.json")
        .context("TOOLCHAIN cannot join pin record path")
}

/// Pull the recorded digest out of a pin record without a JSON parser.
fn pin_digest(record: &str) -> String {
    record
        .split("\"sha256\"")
        .nth(1)
        .and_then(|rest| rest.split('"').nth(1))
        .unwrap_or_default()
        .to_string()
}

/// Unpack the tarball under `image/`, keeping the vendor tree intact so
/// the binaries keep discovering their sysroot relatively. The single
/// top-level vendor directory is skipped.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn unpack_dist(
    resolver: &PathResolver,
    tarball: &GuardedPath,
    image: &GuardedPath,
) -> Result<()> {
    resolver.create_dir_all(image)?;
    let tar_bytes = resolver.read_file(tarball)?;
    let gz = flate2::read::GzDecoder::new(tar_bytes.as_slice());
    let mut archive = tar::Archive::new(gz);
    let entries = archive
        .entries()
        .context("TOOLCHAIN cannot list the tarball entries")?;
    for entry in entries {
        let mut entry = entry.context("TOOLCHAIN cannot read a tarball entry")?;
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
        if entry.header().entry_type().is_dir() {
            resolver.create_dir_all(&dest)?;
        } else {
            resolver.ensure_parent_dir(&dest)?;
            let mut out = resolver.open_write(&dest)?;
            std::io::copy(&mut entry, &mut out)
                .context("TOOLCHAIN cannot extract a tarball entry")?;
        }
    }
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
    for entry in resolver.read_dir_entries(dir)? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let abs = entry.path();
        let Some(wrapped) = GuardedPath::new(dir.root(), &abs).ok() else {
            continue;
        };
        if resolver.exists(&wrapped) && matches!(resolver.entry_kind(&wrapped), Ok(oxdock_fs::EntryKind::Dir)) {
            if name == "bin" {
                for inner in resolver.read_dir_entries(&wrapped)? {
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
