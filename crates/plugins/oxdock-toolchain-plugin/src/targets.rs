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
}
