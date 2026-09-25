//! OxDock-driven compile orchestration isolated to cache (issue #179).
//!
//! Source snapshots, target dirs, and binaries all live under the
//! toolchain guard. `RUN` equivalents invoke only cached `cargo` and
//! `rustc` binaries through `oxdock-process`, never host tools.
//! `TOOLCHAIN_BUILD` auto-provisions a missing toolchain instead of
//! failing on step ordering.

use anyhow::{Context, Result, bail};
use oxdock_fs::{EntryKind, GuardedPath, PathResolver};

use crate::fingerprint::{FingerprintInputs, Profile, compute_fingerprint};
use crate::provision::{ToolchainInfo, provision_toolchain, toolchain_dirs, toolchain_resolver};
use crate::targets::{host_exe_name, target_exe_name};

/// Compiled artifact description for MAP returns.
pub struct BuildOutput {
    pub binary: String,
    pub profile: Profile,
    pub releasable: bool,
    pub metadata: Vec<(String, String)>,
}

/// Snapshot the local source `hint` (workspace-anchored like READ) into
/// `<guard>/src/local-<hash>/` and return the staged manifest dir. Never
/// writes into the local tree. Idempotent per content hash.
pub fn fetch_source(anchor: &GuardedPath, hint: &str) -> Result<String> {
    let workspace =
        PathResolver::new_guarded(anchor.clone(), anchor.clone())
            .context("TOOLCHAIN_FETCH_SOURCE cannot open the workspace resolver")?;
    let src = anchor_fetch_source(&workspace, anchor, hint)?;
    let hash = hash_source_dir(&workspace, &src)?;
    let short = hash.get(..16).unwrap_or(&hash).to_string();
    let resolver = toolchain_resolver(anchor)?;
    let staged = resolver
        .root()
        .join(&format!("src/local-{short}"))
        .context("TOOLCHAIN_FETCH_SOURCE cannot join stage dir")?;
    if !resolver.exists(&staged) {
        copy_source_tree(&workspace, &resolver, &src, &staged)?;
    }
    Ok(staged.display().to_string())
}

/// Anchor the hint like READ: leading `/` means the workspace root.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn anchor_fetch_source(
    workspace: &PathResolver,
    anchor: &GuardedPath,
    raw: &str,
) -> Result<GuardedPath> {
    use oxdock_fs::to_forward_slashes;
    let _ = workspace;
    let normalized = to_forward_slashes(raw);
    let rel = std::path::Path::new(&normalized);
    if PathResolver::is_absolute_or_rooted(rel) {
        if let Ok(guarded) = GuardedPath::new(anchor.root(), rel) {
            return Ok(guarded);
        }
        let rel = PathResolver::root_relative_path(rel);
        let Some(rel_str) = rel.to_str().filter(|s| !s.is_empty()) else {
            bail!("TOOLCHAIN_FETCH_SOURCE source escapes the workspace: {raw}");
        };
        return anchor
            .join(rel_str)
            .with_context(|| format!("TOOLCHAIN_FETCH_SOURCE source escapes the workspace: {raw}"));
    }
    anchor
        .join(&normalized)
        .with_context(|| format!("TOOLCHAIN_FETCH_SOURCE source escapes the workspace: {raw}"))
}

/// Hash every file under `dir` (sorted, skipping `target/` and `.git`)
/// into one digest covering paths plus contents.
pub fn hash_source_dir(resolver: &PathResolver, dir: &GuardedPath) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut files = Vec::new();
    collect_source_files(resolver, dir, dir, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for file in &files {
        hasher.update(file.as_bytes());
        hasher.update([0]);
        hasher.update(resolver.read_file(&dir.join(file)?)?);
        hasher.update([0]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Relative file list under `top`, skipping build outputs and VCS state.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn collect_source_files(
    resolver: &PathResolver,
    top: &GuardedPath,
    dir: &GuardedPath,
    out: &mut Vec<String>,
) -> Result<()> {
    for entry in resolver.read_dir_entries(dir)? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == "target" || name == ".git" {
            continue;
        }
        let abs = entry.path();
        let Some(wrapped) = GuardedPath::new(dir.root(), &abs).ok() else {
            continue;
        };
        if matches!(resolver.entry_kind(&wrapped), Ok(EntryKind::Dir)) {
            collect_source_files(resolver, top, &wrapped, out)?;
        } else {
            let rel = abs
                .strip_prefix(top.as_path())
                .ok()
                .and_then(|p| p.to_str())
                .context("TOOLCHAIN source file escapes its root")?;
            out.push(oxdock_fs::to_forward_slashes(rel));
        }
    }
    Ok(())
}

/// Recursive guarded copy from the workspace resolver into the toolchain
/// resolver. Both sides stay inside their own guards the whole way.
fn copy_source_tree(
    workspace: &PathResolver,
    toolchain: &PathResolver,
    src: &GuardedPath,
    dest: &GuardedPath,
) -> Result<()> {
    toolchain.create_dir_all(dest)?;
    for entry in workspace.read_dir_entries(src)? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == "target" || name == ".git" {
            continue;
        }
        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        let abs = entry.path();
        let Some(wrapped) = GuardedPath::new(src.root(), &abs).ok() else {
            continue;
        };
        let target = dest.join(name)?;
        if matches!(
            workspace.entry_kind(&wrapped),
            Ok(EntryKind::Dir)
        ) {
            copy_source_tree(workspace, toolchain, &wrapped, &target)?;
        } else {
            toolchain.ensure_parent_dir(&target)?;
            let bytes = workspace.read_file(&wrapped)?;
            toolchain.write_file(&target, &bytes)?;
        }
    }
    Ok(())
}

/// Build the staged source for `triple` with `features` and `profile`.
/// Manifests must live inside the toolchain guard, never the local tree
/// or `SYSTEM`. Missing toolchain binaries provision automatically.
/// Rebuilds only when the fingerprint is stale.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
pub fn build_cached(
    anchor: &GuardedPath,
    triple: &str,
    manifest_dir: &str,
    features: &[String],
    profile: Profile,
) -> Result<BuildOutput> {
    let resolver = toolchain_resolver(anchor)?;
    let guard = resolver.root().clone();
    let manifest_candidate = std::path::Path::new(manifest_dir);
    let manifest = GuardedPath::new(guard.root(), manifest_candidate).with_context(|| {
        format!("TOOLCHAIN_BUILD manifest_dir must live inside the toolchain cache, got {manifest_dir:?}")
    })?;
    if manifest.as_path() != manifest_candidate {
        bail!(
            "TOOLCHAIN_BUILD manifest_dir must live inside the toolchain cache, got {manifest_dir:?}"
        );
    }
    let info = ensure_binaries(anchor, &resolver, triple)?;
    let source_hash = hash_source_dir(&resolver, &manifest)?;
    let toolchain_versions = vec![rustc_version(&info)?];
    let fingerprint = compute_fingerprint(&FingerprintInputs {
        source_hash: &source_hash,
        features,
        profile,
        flags: &[],
        toolchain_versions: &toolchain_versions,
    });
    let dirs = toolchain_dirs(&resolver, triple)?;
    let out_dir = dirs
        .target
        .join(profile.dir_name())
        .context("TOOLCHAIN_BUILD cannot join profile dir")?;
    let marker = out_dir
        .join("fingerprint.txt")
        .context("TOOLCHAIN_BUILD cannot join fingerprint marker")?;
    let bin_name = target_exe_name(&package_name(&resolver, &manifest)?, triple);
    let binary = out_dir.join(&bin_name).with_context(|| {
        format!("TOOLCHAIN_BUILD cannot join binary {bin_name:?}")
    })?;
    if resolver.exists(&marker) && resolver.exists(&binary) {
        let recorded = resolver.read_file(&marker)?;
        if recorded == fingerprint.as_bytes() {
            return Ok(output_of(&binary, profile, triple, features, &source_hash, &toolchain_versions, &fingerprint));
        }
    }
    resolver.create_dir_all(&out_dir)?;
    let target_base = dirs
        .target
        .parent()
        .context("TOOLCHAIN_BUILD target dir has no parent")?;
    cargo_build(&info, &manifest, &target_base, triple, features, profile)?;
    if !resolver.exists(&binary) {
        bail!(
            "TOOLCHAIN_BUILD finished but {} is missing: the manifest package name must match the binary name",
            binary.display()
        );
    }
    resolver.write_file(&marker, fingerprint.as_bytes())?;
    Ok(output_of(&binary, profile, triple, features, &source_hash, &toolchain_versions, &fingerprint))
}

/// Present binaries win; otherwise provision on the spot so callers
/// never fail purely because `TOOLCHAIN_ENSURE` ran out of order.
fn ensure_binaries(
    anchor: &GuardedPath,
    resolver: &PathResolver,
    triple: &str,
) -> Result<ToolchainInfo> {
    let dirs = toolchain_dirs(resolver, triple)?;
    let cargo = dirs.bin.join(&host_exe_name("cargo"))?;
    let rustc = dirs.bin.join(&host_exe_name("rustc"))?;
    if resolver.exists(&cargo) && resolver.exists(&rustc) {
        return Ok(ToolchainInfo {
            cargo: cargo.display().to_string(),
            rustc: rustc.display().to_string(),
            sysroot: dirs.dist.display().to_string(),
            triple: triple.to_string(),
        });
    }
    provision_toolchain(anchor, triple)
}

/// `rustc --version` from the cached binary only, never the host PATH.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn rustc_version(info: &ToolchainInfo) -> Result<String> {
    let mut cmd = oxdock_process::CommandBuilder::new(std::ffi::OsString::from(&info.rustc));
    cmd.arg("--version");
    let out = cmd.output().context("TOOLCHAIN cannot run cached rustc --version")?;
    if !out.success() {
        bail!("TOOLCHAIN cached rustc --version failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Package name from the staged `Cargo.toml`.
fn package_name(resolver: &PathResolver, manifest: &GuardedPath) -> Result<String> {
    let cargo_toml = manifest.join("Cargo.toml")?;
    let bytes = resolver.read_file(&cargo_toml)?;
    let text = String::from_utf8(bytes).context("TOOLCHAIN manifest Cargo.toml is not UTF-8")?;
    let table: toml::Value =
        toml::from_str(&text).context("TOOLCHAIN cannot parse manifest Cargo.toml")?;
    table
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .context("TOOLCHAIN manifest Cargo.toml has no package.name")
}

/// Drive the cached cargo with manifest and target dir pinned inside
/// the cache. The base target dir plus `--target` reproduces real cargo
/// layout (`<base>/<triple>/<profile>`). No host cargo, no ambient
/// target dir.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn cargo_build(
    info: &ToolchainInfo,
    manifest: &GuardedPath,
    target_base: &GuardedPath,
    triple: &str,
    features: &[String],
    profile: Profile,
) -> Result<()> {
    let manifest_toml = manifest.join("Cargo.toml")?;
    let mut cmd = oxdock_process::CommandBuilder::new(std::ffi::OsString::from(&info.cargo));
    cmd.arg("build");
    cmd.arg("--manifest-path");
    cmd.arg(std::ffi::OsString::from(manifest_toml.as_path()));
    cmd.arg("--target-dir");
    cmd.arg(std::ffi::OsString::from(target_base.as_path()));
    cmd.arg("--target");
    cmd.arg(triple);
    match profile {
        Profile::Release => {
            cmd.arg("--release");
        }
        Profile::Dev => {}
    }
    if !features.is_empty() {
        cmd.arg("--features");
        cmd.arg(features.join(","));
    }
    cmd.env("CARGO_NET_OFFLINE", "false");
    let out = cmd.output().context("TOOLCHAIN cannot run cached cargo build")?;
    if !out.success() {
        bail!(
            "TOOLCHAIN cargo build failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Assemble the return value with artifact metadata.
fn output_of(
    binary: &GuardedPath,
    profile: Profile,
    triple: &str,
    features: &[String],
    source_hash: &str,
    toolchain_versions: &[String],
    fingerprint: &str,
) -> BuildOutput {
    BuildOutput {
        binary: binary.display().to_string(),
        profile,
        releasable: profile.releasable(),
        metadata: vec![
            ("triple".to_string(), triple.to_string()),
            (
                "profile".to_string(),
                match profile {
                    Profile::Release => "release".to_string(),
                    Profile::Dev => "dev".to_string(),
                },
            ),
            ("features".to_string(), features.join(",")),
            ("source".to_string(), source_hash.to_string()),
            ("toolchains".to_string(), toolchain_versions.join(",")),
            ("fingerprint".to_string(), fingerprint.to_string()),
        ],
    }
}
