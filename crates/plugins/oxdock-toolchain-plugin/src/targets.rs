//! Target triple table for on-demand portable builds (issue #179).
//!
//! Unknown architectures are a hard error, never a guess. MSVC targets
//! report their weight and licensing explicitly instead of silently
//! falling back.

use anyhow::{Result, bail};

/// Pinned Rust version provisioned on demand. Bumped deliberately with
/// its hashes, never floating.
pub const PINNED_RUST_VERSION: &str = "1.90.0";

/// Triples the toolchain plugin provisions and builds for.
pub const SUPPORTED_TRIPLES: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-gnu",
    "aarch64-pc-windows-gnu",
];

/// MSVC triples: recognized so the error names the weight instead of
/// guessing or silently falling back.
const MSVC_TRIPLES: &[&str] = &[
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

/// Host triple of the running process, mapped from `std::env::consts`.
/// Unknown host combinations bail instead of guessing.
pub fn host_triple() -> Result<String> {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let triple = match (arch, os) {
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "windows") => "x86_64-pc-windows-gnu",
        ("aarch64", "windows") => "aarch64-pc-windows-gnu",
        _ => bail!("TOOLCHAIN: unsupported host arch/os combination: {arch}/{os}"),
    };
    Ok(triple.to_string())
}

/// Resolve an optional triple argument: empty or blank means the host.
/// Anything else must name a supported triple exactly.
pub fn resolve_triple(raw: Option<&str>) -> Result<String> {
    let Some(text) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return host_triple();
    };
    validate_triple(text)?;
    Ok(text.to_string())
}

/// Accept supported triples; MSVC triples fail with an explicit weight
/// error; everything else fails as unknown.
pub fn validate_triple(triple: &str) -> Result<()> {
    if SUPPORTED_TRIPLES.contains(&triple) {
        return Ok(());
    }
    if MSVC_TRIPLES.contains(&triple) {
        bail!(
            "TOOLCHAIN: {triple} needs the MSVC toolchain (multi-GB Visual Studio install with its own license): not provisioned by this plugin; use the -gnu triple or build on the target instead"
        );
    }
    bail!(
        "TOOLCHAIN: unknown target triple {triple:?}: expected one of {}",
        SUPPORTED_TRIPLES.join(", ")
    )
}

/// Download URL for the standalone Rust tarball of a pinned version and
/// triple. Hashes pin per release in the provision metadata, not here.
pub fn rust_dist_url(version: &str, triple: &str) -> String {
    format!("https://static.rust-lang.org/dist/rust-standalone-{version}-{triple}.tar.gz")
}

/// Pinned zig version provisioned as the portable linker. Bumped
/// deliberately with its hashes, never floating. Override with
/// `OXDOCK_ZIG_VERSION` when dogfooding a newer release.
pub const PINNED_ZIG_VERSION: &str = "0.15.1";

/// Version override for the zig linker bundle.
pub const ZIG_VERSION_ENV: &str = "OXDOCK_ZIG_VERSION";

/// Exact-digest override for the zig bundle. Mirrors the toolchain pin.
pub const ZIG_SHA256_ENV: &str = "OXDOCK_ZIG_SHA256";

/// Effective zig version: exact env override wins, otherwise the pin.
pub fn zig_version() -> String {
    std::env::var(ZIG_VERSION_ENV)
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| PINNED_ZIG_VERSION.to_string())
}

/// How a target triple links. `rustc` always shells out to a linker,
/// and bare hosts have no `cc`, so every triple that zig covers links
/// through the cached zig binary. Apple targets need the proprietary
/// macOS SDK, which no cache can provide: native macOS builds use the
/// host compiler explicitly, and cross-Apple builds from other hosts
/// fail closed instead of guessing.
pub enum Linker {
    /// Link through cached zig: `zig cc -target <zig_target>`.
    Zig { zig_target: String },
    /// Link through the host `cc`. Apple targets only: the proprietary
    /// macOS SDK ships with the Xcode command line tools and no cache
    /// can provide it.
    HostCc,
}

/// Linker policy for `triple`. Validates the triple first so unknown
/// and MSVC targets keep their existing errors.
pub fn linker_for_triple(triple: &str) -> Result<Linker> {
    validate_triple(triple)?;
    if triple.contains("apple") || triple.contains("darwin") {
        if std::env::consts::OS == "macos" {
            return Ok(Linker::HostCc);
        }
        bail!(
            "TOOLCHAIN: {triple} needs the macOS SDK, which is only available on a macOS host: cross-Apple builds from other hosts are not provisioned"
        );
    }
    Ok(Linker::Zig {
        zig_target: zig_target_for_triple(triple)?,
    })
}

/// zig `-target` string for a Rust triple. Only Linux and Windows GNU
/// targets qualify: zig bundles their libc headers, so no host files
/// are involved.
pub fn zig_target_for_triple(triple: &str) -> Result<String> {
    if let Some(arch) = triple.split('-').next() {
        let target = match (arch, triple) {
            ("x86_64", t) if t.contains("linux-gnu") => "x86_64-linux-gnu",
            ("aarch64", t) if t.contains("linux-gnu") => "aarch64-linux-gnu",
            ("x86_64", t) if t.contains("linux-musl") => "x86_64-linux-musl",
            ("aarch64", t) if t.contains("linux-musl") => "aarch64-linux-musl",
            ("x86_64", t) if t.contains("windows-gnu") => "x86_64-windows-gnu",
            ("aarch64", t) if t.contains("windows-gnu") => "aarch64-windows-gnu",
            _ => bail!("TOOLCHAIN: no zig target mapping for triple {triple:?}"),
        };
        return Ok(target.to_string());
    }
    bail!("TOOLCHAIN: no zig target mapping for triple {triple:?}")
}

/// Host platform of the running process as a zig bundle `(os, arch)`
/// pair. Unknown hosts bail instead of guessing.
pub fn zig_bundle_platform() -> Result<(String, String)> {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let pair = match (arch, os) {
        ("x86_64", "linux") => ("linux", "x86_64"),
        ("aarch64", "linux") => ("linux", "aarch64"),
        ("x86_64", "macos") => ("macos", "x86_64"),
        ("aarch64", "macos") => ("macos", "aarch64"),
        _ => bail!("TOOLCHAIN: no zig bundle for host arch/os combination: {arch}/{os} (Windows hosts are not provisioned yet)"),
    };
    Ok((pair.0.to_string(), pair.1.to_string()))
}

/// Download URL for the zig bundle of a version and host platform.
pub fn zig_dist_url(version: &str, os: &str, arch: &str) -> String {
    format!("https://ziglang.org/download/{version}/zig-{os}-{arch}-{version}.tar.xz")
}

/// Executable file name for a binary this host runs (provisioned
/// `cargo`/`rustc` execute locally, so the host platform decides).
#[cfg(windows)]
pub fn host_exe_name(stem: &str) -> String {
    format!("{stem}.exe")
}

/// Executable file name for a binary this host runs (provisioned
/// `cargo`/`rustc` execute locally, so the host platform decides).
#[cfg(not(windows))]
pub fn host_exe_name(stem: &str) -> String {
    stem.to_string()
}

/// Executable file name for a build artifact targeting `triple`.
/// Cargo appends `.exe` for Windows targets regardless of the host, so
/// the triple decides here, never the host platform.
pub fn target_exe_name(stem: &str, triple: &str) -> String {
    if triple.contains("windows") {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_triple_resolves_on_supported_hosts() {
        let triple = host_triple().expect("host triple");
        assert!(
            SUPPORTED_TRIPLES.contains(&triple.as_str()),
            "host triple must be supported, got {triple:?}"
        );
    }

    #[test]
    fn blank_means_host() {
        let host = host_triple().expect("host");
        assert_eq!(resolve_triple(None).expect("none"), host);
        assert_eq!(resolve_triple(Some("  ")).expect("blank"), host);
    }

    #[test]
    fn unknown_triple_is_a_hard_error() {
        let err = validate_triple("mips-unknown-linux-gnu").expect_err("unknown must fail");
        assert!(err.to_string().contains("unknown target triple"), "{err:#}");
    }

    #[test]
    fn msvc_names_its_weight() {
        let err =
            validate_triple("x86_64-pc-windows-msvc").expect_err("msvc must fail explicitly");
        assert!(err.to_string().contains("MSVC"), "{err:#}");
    }

    #[test]
    fn dist_url_names_version_and_triple() {
        let url = rust_dist_url("1.90.0", "x86_64-unknown-linux-musl");
        assert!(url.contains("1.90.0"), "{url:?}");
        assert!(url.contains("x86_64-unknown-linux-musl"), "{url:?}");
    }

    #[test]
    fn exe_names_follow_host_and_target() {
        assert_eq!(
            target_exe_name("demo-pkg", "x86_64-pc-windows-gnu"),
            "demo-pkg.exe"
        );
        assert_eq!(
            target_exe_name("demo-pkg", "x86_64-unknown-linux-musl"),
            "demo-pkg"
        );
        #[cfg(windows)]
        assert_eq!(host_exe_name("cargo"), "cargo.exe");
        #[cfg(not(windows))]
        assert_eq!(host_exe_name("cargo"), "cargo");
    }

    #[test]
    fn linux_triples_link_through_zig() {
        for (triple, ztarget) in [
            ("x86_64-unknown-linux-gnu", "x86_64-linux-gnu"),
            ("aarch64-unknown-linux-gnu", "aarch64-linux-gnu"),
            ("x86_64-unknown-linux-musl", "x86_64-linux-musl"),
            ("aarch64-unknown-linux-musl", "aarch64-linux-musl"),
            ("x86_64-pc-windows-gnu", "x86_64-windows-gnu"),
        ] {
            match linker_for_triple(triple).expect("supported triple") {
                Linker::Zig { zig_target } => assert_eq!(zig_target, ztarget),
                Linker::HostCc => {
                    panic!("{triple} must use zig, got host-cc")
                }
            }
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn apple_triples_use_host_cc_on_macos() {
        match linker_for_triple("aarch64-apple-darwin").expect("apple parses") {
            Linker::HostCc => {}
            Linker::Zig { .. } => panic!("apple targets must not use zig"),
        }
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn apple_triples_fail_closed_off_macos() {
        let err = linker_for_triple("aarch64-apple-darwin")
            .expect_err("cross-Apple from other hosts must fail");
        assert!(err.to_string().contains("macOS SDK"), "{err:#}");
    }

    #[test]
    fn zig_bundle_names_version_and_platform() {
        let url = zig_dist_url("0.15.1", "linux", "x86_64");
        assert_eq!(
            url,
            "https://ziglang.org/download/0.15.1/zig-linux-x86_64-0.15.1.tar.xz"
        );
    }
}
