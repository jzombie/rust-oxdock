//! Build fingerprints: agreement by construction (issue #179).
//!
//! The fingerprint covers source hash plus feature set plus profile plus
//! flags plus toolchain versions, so any of those changing rebuilds.
//! Dev and release profiles hash into distinct keys, so dev outputs can
//! never masquerade as release artifacts or poison the release entry.

use anyhow::{Result, bail};

/// Supported build profiles. Release is the default; dev is explicit
/// opt-in for iteration speed and never releasable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Release,
    Dev,
}

/// Parse a profile argument: empty or blank means release.
pub fn parse_profile(raw: Option<&str>) -> Result<Profile> {
    let Some(text) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Profile::Release);
    };
    match text {
        "release" => Ok(Profile::Release),
        "dev" => Ok(Profile::Dev),
        _ => bail!("TOOLCHAIN_BUILD profile must be \"release\" or \"dev\", got {text:?}"),
    }
}

impl Profile {
    /// Directory leaf under the target triple directory.
    pub fn dir_name(self) -> &'static str {
        match self {
            Profile::Release => "release",
            Profile::Dev => "dev",
        }
    }

    /// Only release outputs may ship.
    pub fn releasable(self) -> bool {
        match self {
            Profile::Release => true,
            Profile::Dev => false,
        }
    }
}

/// Fingerprint inputs. Every field that changes the artifact participates.
pub struct FingerprintInputs<'a> {
    pub source_hash: &'a str,
    pub features: &'a [String],
    pub profile: Profile,
    pub flags: &'a [String],
    pub toolchain_versions: &'a [String],
}

/// Canonical fingerprint hex: SHA-256 over newline-joined fields with a
/// versioned prefix, so the scheme itself can evolve.
pub fn compute_fingerprint(inputs: &FingerprintInputs<'_>) -> String {
    use sha2::{Digest, Sha256};
    let mut features = inputs.features.to_vec();
    features.sort();
    let mut flags = inputs.flags.to_vec();
    flags.sort();
    let mut versions = inputs.toolchain_versions.to_vec();
    versions.sort();
    let profile = match inputs.profile {
        Profile::Release => "release",
        Profile::Dev => "dev",
    };
    let canonical = format!(
        "oxdock-toolchain-fingerprint/v1\nsource={}\nfeatures={}\nprofile={profile}\nflags={}\ntoolchains={}\n",
        inputs.source_hash,
        features.join(","),
        flags.join(","),
        versions.join(","),
    );
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(source: &str, features: &[String], profile: Profile) -> String {
        compute_fingerprint(&FingerprintInputs {
            source_hash: source,
            features,
            profile,
            flags: &[],
            toolchain_versions: &["rustc 1.90.0".to_string()],
        })
    }

    #[test]
    fn blank_profile_is_release() {
        assert_eq!(parse_profile(None).expect("none"), Profile::Release);
        assert_eq!(parse_profile(Some(" ")).expect("blank"), Profile::Release);
        assert_eq!(parse_profile(Some("dev")).expect("dev"), Profile::Dev);
        assert!(parse_profile(Some("debug")).is_err());
    }

    #[test]
    fn any_input_change_rebuilds() {
        let base = fingerprint("abc", &[], Profile::Release);
        let other_source = fingerprint("abd", &[], Profile::Release);
        assert_ne!(base, other_source, "source hash participates");
        let feat = vec!["net".to_string()];
        let other_features = fingerprint("abc", &feat, Profile::Release);
        assert_ne!(base, other_features, "features participate");
        let other_profile = fingerprint("abc", &[], Profile::Dev);
        assert_ne!(base, other_profile, "profile participates");
        let other_toolchain = compute_fingerprint(&FingerprintInputs {
            source_hash: "abc",
            features: &[],
            profile: Profile::Release,
            flags: &[],
            toolchain_versions: &["rustc 1.91.0".to_string()],
        });
        assert_ne!(base, other_toolchain, "toolchain versions participate");
    }

    #[test]
    fn dev_never_releasable() {
        assert!(Profile::Release.releasable());
        assert!(!Profile::Dev.releasable());
        assert_ne!(Profile::Release.dir_name(), Profile::Dev.dir_name());
    }

    #[test]
    fn feature_order_does_not_rebuild() {
        let a = vec!["net".to_string(), "ssh".to_string()];
        let b = vec!["ssh".to_string(), "net".to_string()];
        assert_eq!(
            fingerprint("abc", &a, Profile::Release),
            fingerprint("abc", &b, Profile::Release),
        );
    }
}
