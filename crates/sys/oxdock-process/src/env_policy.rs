//! Central host-injection policy: the single place deciding what may flow
//! from the host OS into scripts and spawned children.
//!
//! Tiered world, stated once: a spawned child receives the script
//! environment layered over engine-managed substrate, never ambient
//! host state.
//!
//! - Tier 1 (OS substrate) injects on every spawn, including `ISOLATED`.
//! - Tier 2 (managed temp) injects on every spawn, including `ISOLATED`.
//! - Tier 3 (sanctioned PATH baseline) injects everywhere except
//!   `ISOLATED` without an inner `PATH`.
//! - Tier 4 (identity, credentials) arrives only by explicit routing.
//!
//! Every spawn site stages through here instead of hand-rolling its
//! own overlay and diagnostics, so the rule cannot drift per call site.
//!
//! Rendered reference: `ENV_CONTRACTS.md` at the repository root
//! (generated from the tier tables below by `cargo run -p docs-gen`;
//! templates link it through the `env_contracts_url` global).

use anyhow::{Result, bail};
use oxdock_fs::env as oxdock_env;
use std::collections::HashMap;

/// Look up `name` in a script env map with OS comparison semantics:
/// case-insensitive on Windows (where `Path` and `PATH` name the same
/// variable), exact everywhere else.
pub fn has_var(envs: &HashMap<String, String>, name: &str) -> bool {
    #[allow(clippy::disallowed_macros)]
    if cfg!(windows) {
        envs.keys().any(|k| k.eq_ignore_ascii_case(name))
    } else {
        envs.contains_key(name)
    }
}

/// Whether `name` is a sanctioned ambient seed into initial script env:
/// the complete list of host names admitted without an explicit
/// `INHERIT_ENV` directive. Everything else arrives only by routing.
pub fn is_sanctioned_seed(name: &str) -> bool {
    name.starts_with("CARGO_FEATURE_") || name.starts_with("CARGO_CFG_")
}

/// Whether the OS consults `PATH` to resolve `program`: bare names do,
/// absolute paths and separator-carrying relative paths do not.
pub fn needs_path_lookup(program: &str) -> bool {
    !program.contains('/') && !program.contains('\\')
}

/// Pre-flight for a closed child spawn: fail with the fix spelled out
/// instead of an undiagnosable OS error. Validates the FINAL assembled
/// map with no caller-specific flags: `apply_tiers` runs first, so
/// `PATH` is required for bare program names (and always when
/// `require_path` is set), and `SystemRoot` is required on Windows
/// always. Inside `ISOLATED` without an inner `PATH` the omitted
/// baseline trips the bail with the fix spelled out. This stays a
/// fail-safe against spawn sites that forget tiers.
pub fn check_spawn_anchors(
    program: &str,
    envs: &HashMap<String, String>,
    require_path: bool,
) -> Result<()> {
    if (require_path || needs_path_lookup(program)) && !has_var(envs, oxdock_env::os::PATH) {
        bail!(
            "cannot resolve '{program}' without {} in the script environment; declare INHERIT_ENV [{}]",
            oxdock_env::os::PATH,
            oxdock_env::os::PATH,
        );
    }
    #[allow(clippy::disallowed_macros)]
    if cfg!(windows) && !has_var(envs, oxdock_env::os::windows::SYSTEM_ROOT) {
        bail!(
            "Windows process execution requires {} in the script environment; declare INHERIT_ENV [{}]",
            oxdock_env::os::windows::SYSTEM_ROOT,
            oxdock_env::os::windows::SYSTEM_ROOT,
        );
    }
    Ok(())
}

/// OS comparison for variable names: case-insensitive on Windows,
/// exact everywhere else. Mirrors [`has_var`].
fn names_eq(a: &str, b: &str) -> bool {
    #[allow(clippy::disallowed_macros)]
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// One host variable read. Under Miri isolation there is no host to
/// read: every name resolves absent and synthetic fallbacks apply.
#[allow(clippy::disallowed_methods)]
fn host_var(name: &str) -> Option<String> {
    #[allow(clippy::disallowed_macros)]
    if cfg!(miri) {
        None
    } else {
        std::env::var(name).ok()
    }
}

/// Tier labels in render order, derived from [`describe_tiers`] so a
/// new tier appears in output without renderer edits.
pub fn tier_order() -> Vec<&'static str> {
    let mut order = Vec::new();
    for entry in describe_tiers() {
        if !order.contains(&entry.tier) {
            order.push(entry.tier);
        }
    }
    order
}

/// Opening chapter of the contract reference, sourced here with the
/// rest of the prose.
pub const CONTRACT_INTRO_MD: &str = "## Tier contract\n\
 \n\
 Every spawned child receives the script environment layered over\n\
 engine-managed substrate, never ambient host state. Tiers 1 and 2\n\
 inject on every spawn including `ISOLATED`; Tier 3 injects\n\
 everywhere except `ISOLATED` without an inner `PATH`; Tier 4\n\
 arrives only by explicit routing.\n";

/// Full markdown heading for one tier's chapter, sourced here so
/// presentational exceptions (the identity tier reads cleaner without
/// its label) live with the chapters, not the renderer.
pub fn tier_heading(tier: &str) -> Option<String> {
    let (title, _) = tier_section(tier)?;
    if tier == "identity" {
        Some("### Managed identity".to_string())
    } else {
        Some(format!("### Tier {tier}: {title}"))
    }
}

/// Section title and body for one tier's contract chapter, sourced
/// here (not in the docs renderer) so prose and tables change together.
/// Returns `None` for unknown tiers.
pub fn tier_section(tier: &str) -> Option<(&'static str, &'static str)> {
    let section = match tier {
        "1A" => (
            "OS loader invariants",
            "Required for process initialization on Windows: the dynamic\n\
             linker and MSVC CRT cannot run without these. Injecting them\n\
             is not a leak.",
        ),
        "1B" => (
            "Substrate defaults",
            "Terminal identity and executable-extension resolution. Host\n\
             values apply when set, synthetic fallbacks otherwise; scripts\n\
             override freely.",
        ),
        "2" => (
            "Managed temp and home",
            "Scratch spaces and tool home, always bound to the session\n\
             `<scratch>/tmp` so children can neither read nor collide with\n\
             host temporary files and configs.",
        ),
        "identity" => (
            "Managed sandbox identity",
            "Shells synthesize login identity from `getpwuid` when these\n\
             are absent, which would reintroduce host identity into\n\
             scrubbed children. The engine pins the sandbox name instead.",
        ),
        "3" => (
            "Execution path",
            "Sanctioned system directories for core utilities. Host `PATH`\n\
             extensions (custom toolchains, user bins) still require\n\
             explicit `INHERIT_ENV [PATH]`.",
        ),
        "4" => (
            "Identity and credentials",
            "User data, configs, and secrets. Zero automatic injection:\n\
             toolchain legs bridge what they need explicitly.",
        ),
        _ => return None,
    };
    Some(section)
}

/// Precedence and enforcement chapter, sourced here so the guarantee
/// the doc states is reviewed alongside the assembly that keeps it.
pub const PRECEDENCE_MD: &str = "### Precedence and enforcement\n\
 \n\
 Tier 1A is immutable: the static pass rejects `ENV SystemRoot=...`\n\
 and assembly drops script values for those names. Every other\n\
 tier lets explicit script `ENV`/`INHERIT_ENV` win. `check_spawn_anchors`\n\
 validates the final assembled map with no caller-specific flags:\n\
 present anchors pass, and the absent baseline inside unpatched\n\
 `ISOLATED` blocks trips the bail naming `INHERIT_ENV [PATH]`.\n\
 \n\
 Source of truth: `crates/sys/oxdock-fs/src/env.rs` (names) and\n\
 `crates/sys/oxdock-process/src/env_policy.rs` (assembly). This\n\
 file is generated; do not edit by hand.\n";

/// Host-reads chapter preamble, sourced here with the inventory it
/// introduces.
pub const HOST_READS_INTRO_MD: &str =
    "### Host reads\n\nVariables the engine process itself touches (not injects).\n";

/// One host interaction: a variable the engine process itself reads
/// (not injects). Names are `env` constants where one exists;
/// `*-suffix` entries are prefix patterns, not single variables.
pub struct HostRead {
    /// Variable name, or `PREFIX_*` pattern.
    pub name: &'static str,
    /// Engine function performing the read.
    pub reader: &'static str,
    /// Why the read exists.
    pub purpose: &'static str,
}

/// Every host read in the execution path, declared here so the
/// contract documents interaction as well as injection. Tier tables
/// say what children receive; this says what the engine process
/// itself touches. `oxdock_fs::env::oxdock` / `cargo` plumbing names
/// are explicit call-site reads (cache roots, manifest dirs), never
/// ambient inheritance; they are listed by module, not per variable.
pub fn engine_host_reads() -> Vec<HostRead> {
    use oxdock_env::os::{unix, windows};
    vec![
        HostRead {
            name: unix::SHELL,
            reader: "oxdock_process::shell::shell_program",
            purpose: "interactive shell resolution (`oxdock --shell`); batch steps pin `sh`",
        },
        HostRead {
            name: windows::COMSPEC,
            reader: "oxdock_process::shell::shell_program",
            purpose: "interactive shell resolution on Windows; batch steps pin `cmd`",
        },
        HostRead {
            name: unix::substrate::TERM,
            reader: "SubstrateEnv::capture",
            purpose: "Tier 1B default; synthetic `dumb` when the host sets nothing",
        },
        HostRead {
            name: windows::substrate::SYSTEM_ROOT,
            reader: "SubstrateEnv::capture",
            purpose: "Tier 1A anchor; fallback is the `C:\\Windows` constant",
        },
        HostRead {
            name: windows::substrate::PATHEXT,
            reader: "SubstrateEnv::capture",
            purpose: "Tier 1B default; synthetic list when the host sets nothing",
        },
        HostRead {
            name: unix::managed::TMPDIR,
            reader: "GuardedPath::tempdir (via tempfile)",
            purpose: "scratch root placement only; the value is never injected",
        },
        HostRead {
            name: "CARGO_FEATURE_*",
            reader: "BuiltinEnv::collect",
            purpose: "sanctioned ambient seed: Cargo-namespaced feature flags",
        },
        HostRead {
            name: "CARGO_CFG_*",
            reader: "BuiltinEnv::collect",
            purpose: "sanctioned ambient seed: Cargo-namespaced cfg flags",
        },
    ]
}
/// Tier 1A immutable loader invariants (Windows): injected
/// unconditionally, script values dropped at assembly.
fn is_tier1a(name: &str) -> bool {
    names_eq(name, oxdock_env::os::windows::substrate::SYSTEM_ROOT)
        || names_eq(name, oxdock_env::os::windows::substrate::WINDIR)
}

/// One row of the tier contract: the same constants the assembly
/// reads, so the rendered `ENV_CONTRACTS.md` cannot drift from the
/// engine. `tier` is one of `"1A"`, `"1B"`, `"2"`, `"identity"`,
/// `"3"`, `"4"`.
pub struct TierEntry {
    /// Tier label (`"1A"`, `"1B"`, `"2"`, `"identity"`, `"3"`, `"4"`).
    pub tier: &'static str,
    /// Variable name (an `oxdock_fs::env` constant value).
    pub name: &'static str,
    /// Where the injected value comes from when the script is silent.
    pub default: &'static str,
    /// What a script declaration does.
    pub rule: &'static str,
}

/// The full tier contract as static data, in render order. Assembly
/// (`capture`) reads the same `env` constants; a parity test pins
/// that every assembled name appears here and vice versa, so adding
/// a tier name in one place without the other fails loudly.
pub fn describe_tiers() -> Vec<TierEntry> {
    use oxdock_env::os::{unix, windows};
    vec![
        TierEntry {
            tier: "1A",
            name: windows::substrate::SYSTEM_ROOT,
            default: "host value, else the `C:\\Windows` fallback",
            rule: "immutable: injected unconditionally; script values dropped at assembly and rejected by the static pass",
        },
        TierEntry {
            tier: "1A",
            name: windows::substrate::WINDIR,
            default: "same root as `SystemRoot`",
            rule: "immutable, same as `SystemRoot`",
        },
        TierEntry {
            tier: "1B",
            name: windows::substrate::PATHEXT,
            default: "host value, else `.COM;.EXE;.BAT;.CMD`",
            rule: "default: script values win",
        },
        TierEntry {
            tier: "1B",
            name: unix::substrate::TERM,
            default: "host value, else `dumb`",
            rule: "default: script values win",
        },
        TierEntry {
            tier: "2",
            name: windows::managed::TEMP,
            default: "session `<scratch>/tmp`",
            rule: "managed: script values win; never host-inherited",
        },
        TierEntry {
            tier: "2",
            name: windows::managed::TMP,
            default: "session `<scratch>/tmp`",
            rule: "managed: script values win; never host-inherited",
        },
        TierEntry {
            tier: "2",
            name: windows::managed::USERPROFILE,
            default: "session `<scratch>/tmp` (native profile anchor; Win32 API fallback would otherwise resolve the host profile)",
            rule: "managed: script values win; never host-inherited",
        },
        TierEntry {
            tier: "2",
            name: unix::managed::TMPDIR,
            default: "session `<scratch>/tmp`",
            rule: "managed: script values win; never host-inherited",
        },
        TierEntry {
            tier: "2",
            name: unix::managed::HOME,
            default: "session `<scratch>/tmp`, so tools cannot discover host configs",
            rule: "managed: script values win; never host-inherited",
        },
        TierEntry {
            tier: "identity",
            name: unix::LOGNAME,
            default: "fixed sandbox name `oxdock` (shells would otherwise synthesize host identity from `getpwuid`)",
            rule: "managed: script values win",
        },
        TierEntry {
            tier: "identity",
            name: unix::USER,
            default: "fixed sandbox name `oxdock`",
            rule: "managed: script values win",
        },
        TierEntry {
            tier: "identity",
            name: windows::USERNAME,
            default: "fixed sandbox name `oxdock`",
            rule: "managed: script values win",
        },
        TierEntry {
            tier: "3",
            name: oxdock_env::os::PATH,
            default: "sanctioned baseline (`/usr/bin:/bin:/usr/sbin:/sbin`, `C:\\Windows\\System32;C:\\Windows`)",
            rule: "baseline: script values win; suppressed inside `ISOLATED` without an inner `PATH`",
        },
        TierEntry {
            tier: "4",
            name: windows::USERPROFILE,
            default: "absent on Unix; managed scratch binding on Windows (Tier 2)",
            rule: "zero host injection everywhere: bridged explicitly on Unix; script values win",
        },
    ]
}

/// Engine-managed substrate for one session: Tier 1 (OS loader and
/// terminal defaults), Tier 2 (scratch-bound temp and home), managed
/// sandbox identity, Tier 3 (sanctioned PATH baseline). Owned by
/// `ExecState`, cloned into `CommandContext` for spawn sites. Never
/// written back into script state: DSL evaluation reads through
/// [`SubstrateEnv::get`], and `ISOLATED` snapshots stay clean.
#[derive(Clone, Debug)]
pub struct SubstrateEnv {
    tier1a: Vec<(String, String)>,
    tier1b: Vec<(String, String)>,
    tier2: Vec<(String, String)>,
    identity: Vec<(String, String)>,
    path_baseline: Option<(String, String)>,
}

impl SubstrateEnv {
    /// Capture host substrate once per session. `scratch_tmp` is the
    /// session `<scratch>/tmp` display path Tier 2 binds to.
    pub fn capture(scratch_tmp: &str) -> Self {
        let mut tier1a = Vec::new();
        let mut tier1b = Vec::new();
        let mut tier2 = Vec::new();
        let mut identity = Vec::new();
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            let root =
                host_var(oxdock_env::os::windows::substrate::SYSTEM_ROOT).unwrap_or_else(|| {
                    oxdock_env::os::windows::substrate::FALLBACK_SYSTEM_ROOT.to_string()
                });
            tier1a.push((
                oxdock_env::os::windows::substrate::SYSTEM_ROOT.to_string(),
                root.clone(),
            ));
            tier1a.push((oxdock_env::os::windows::substrate::WINDIR.to_string(), root));
            tier1b.push((
                oxdock_env::os::windows::substrate::PATHEXT.to_string(),
                host_var(oxdock_env::os::windows::substrate::PATHEXT).unwrap_or_else(|| {
                    oxdock_env::os::windows::substrate::DEFAULT_PATHEXT.to_string()
                }),
            ));
            tier2.push((
                oxdock_env::os::windows::managed::TEMP.to_string(),
                scratch_tmp.to_string(),
            ));
            tier2.push((
                oxdock_env::os::windows::managed::TMP.to_string(),
                scratch_tmp.to_string(),
            ));
            tier2.push((
                oxdock_env::os::windows::managed::HOME.to_string(),
                scratch_tmp.to_string(),
            ));
            // Native Windows toolchains resolve user directories through
            // USERPROFILE (Win32 profile-API fallback when absent), so the
            // primary profile anchor binds the scratch alongside HOME.
            tier2.push((
                oxdock_env::os::windows::managed::USERPROFILE.to_string(),
                scratch_tmp.to_string(),
            ));
            // Managed identity: shells must not synthesize host identity
            // into scrubbed children. Explicit script values win.
            identity.push((
                oxdock_env::os::windows::USERNAME.to_string(),
                oxdock_env::os::windows::substrate::MANAGED_IDENTITY.to_string(),
            ));
        } else {
            tier1b.push((
                oxdock_env::os::unix::substrate::TERM.to_string(),
                host_var(oxdock_env::os::unix::substrate::TERM)
                    .unwrap_or_else(|| oxdock_env::os::unix::substrate::FALLBACK_TERM.to_string()),
            ));
            tier2.push((
                oxdock_env::os::unix::managed::TMPDIR.to_string(),
                scratch_tmp.to_string(),
            ));
            tier2.push((
                oxdock_env::os::unix::managed::HOME.to_string(),
                scratch_tmp.to_string(),
            ));
            // Managed identity: shells synthesize these from `getpwuid`
            // when absent (observed live: scrubbed zsh children regaining
            // host `HOME`/`LOGNAME`). The engine pins the sandbox name
            // instead. Explicit script values win.
            identity.push((
                oxdock_env::os::unix::LOGNAME.to_string(),
                oxdock_env::os::unix::substrate::MANAGED_IDENTITY.to_string(),
            ));
            identity.push((
                oxdock_env::os::unix::USER.to_string(),
                oxdock_env::os::unix::substrate::MANAGED_IDENTITY.to_string(),
            ));
        }
        #[allow(clippy::disallowed_macros)]
        let baseline = if cfg!(windows) {
            oxdock_env::os::windows::substrate::SANCTIONED_PATH
        } else {
            oxdock_env::os::unix::substrate::SANCTIONED_PATH
        };
        Self {
            tier1a,
            tier1b,
            tier2,
            identity,
            path_baseline: Some((oxdock_env::os::PATH.to_string(), baseline.to_string())),
        }
    }

    /// Capture for a scratch root (tests, synthetic backends): Tier 2
    /// binds to the scratch tmp subpath (created on use, native only).
    pub fn capture_for_scratch(scratch: &oxdock_fs::CargoScratch) -> Self {
        Self::capture(&scratch.tmp_command_path().to_string_lossy())
    }

    /// Clone with the Tier 3 baseline removed: for `ISOLATED` scopes
    /// whose inner allowlist did not re-add `PATH`.
    pub fn without_baseline(mut self) -> Self {
        self.path_baseline = None;
        self
    }
    /// Read-through for DSL evaluation (`is_exec_name`, `Expr::Env`,
    /// expansions): Tier 1 defaults, Tier 2 bindings, and managed
    /// identity are visible, Tier 1A invariants are visible, the PATH
    /// baseline is visible.
    /// Callers suppress the baseline inside `ISOLATED` without an
    /// inner `PATH` (see `ExecState::get_env`).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.tier1a
            .iter()
            .chain(self.tier1b.iter())
            .chain(self.tier2.iter())
            .chain(self.identity.iter())
            .chain(self.path_baseline.iter())
            .find(|(k, _)| names_eq(k, key))
            .map(|(_, v)| v.as_str())
    }

    /// Whether `key` resolves through the substrate (any tier).
    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    fn has_script(script: &HashMap<String, String>, name: &str) -> bool {
        script.keys().any(|k| names_eq(k, name))
    }

    /// Assemble the final child map: Tier 1A (script values for these
    /// names dropped), Tier 1B/2 defaults and managed identity (script
    /// wins), PATH baseline when `include_baseline` and the script map
    /// lacks `PATH`, then the full script map last so explicit values win.
    pub fn assemble(
        &self,
        script: &HashMap<String, String>,
        include_baseline: bool,
    ) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for (k, v) in &self.tier1a {
            out.insert(k.clone(), v.clone());
        }
        for (k, v) in self
            .tier1b
            .iter()
            .chain(self.tier2.iter())
            .chain(self.identity.iter())
        {
            if !Self::has_script(script, k) {
                out.insert(k.clone(), v.clone());
            }
        }
        if include_baseline
            && let Some((k, v)) = &self.path_baseline
            && !Self::has_script(script, k)
        {
            out.insert(k.clone(), v.clone());
        }
        for (k, v) in script {
            if is_tier1a(k) {
                continue;
            }
            out.insert(k.clone(), v.clone());
        }
        out
    }

    /// Transport assembly for infrastructure spawns (`ssh`, `docker`):
    /// Tier 1 + Tier 3 baseline + envs. No Tier 2 scratch coupling, and
    /// no managed identity: transports authenticate as the invoking
    /// user (SSH key discovery reads the real home).
    pub fn assemble_transport(&self, script: &HashMap<String, String>) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for (k, v) in self.tier1a.iter().chain(self.tier1b.iter()) {
            out.insert(k.clone(), v.clone());
        }
        if let Some((k, v)) = &self.path_baseline
            && !Self::has_script(script, k)
        {
            out.insert(k.clone(), v.clone());
        }
        for (k, v) in script {
            if is_tier1a(k) {
                continue;
            }
            out.insert(k.clone(), v.clone());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathed_programs_skip_path_lookup() {
        assert!(!needs_path_lookup("/bin/prog"));
        assert!(!needs_path_lookup("sub/dir/prog"));
        assert!(!needs_path_lookup("sub\\dir\\prog.exe"));
        assert!(needs_path_lookup("prog"));
        assert!(needs_path_lookup("prog.exe"));
    }

    #[test]
    fn sanctioned_seeds_are_cargo_namespaced_only() {
        assert!(is_sanctioned_seed("CARGO_FEATURE_FOO"));
        assert!(is_sanctioned_seed("CARGO_CFG_FOO"));
        assert!(!is_sanctioned_seed(oxdock_env::os::PATH));
        assert!(!is_sanctioned_seed("GITHUB_TOKEN"));
        assert!(!is_sanctioned_seed(oxdock_env::oxdock::WORKSPACE_ROOT));
    }

    #[test]
    fn anchors_reject_bare_spawn_without_path() {
        let envs = HashMap::new();
        let err = check_spawn_anchors("prog", &envs, false).expect_err("must bail");
        assert!(
            err.to_string().contains("INHERIT_ENV [PATH]"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn anchors_require_path_for_shell_form() {
        // Shell binaries resolve by absolute path, but script text inside
        // still resolves through the child's PATH: always required.
        let envs = HashMap::new();
        let err = check_spawn_anchors("/bin/sh", &envs, true).expect_err("must bail");
        assert!(
            err.to_string().contains("INHERIT_ENV [PATH]"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn anchors_accept_pathed_spawn_without_path() {
        let envs = HashMap::new();
        // Absolute program: no PATH consulted. (On Windows this still
        // requires SystemRoot; the assertion below is unix-only.)
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            let err = check_spawn_anchors("/bin/prog", &envs, false).expect_err("must bail");
            assert!(
                err.to_string().contains("SystemRoot"),
                "unexpected error: {err}"
            );
        } else {
            check_spawn_anchors("/bin/prog", &envs, false).expect("pathed spawn needs no PATH");
        }
    }

    fn substrate_for_tests() -> SubstrateEnv {
        SubstrateEnv::capture("/scratch/tmp")
    }

    #[test]
    fn assemble_injects_tiers_under_empty_script() {
        let assembled = substrate_for_tests().assemble(&HashMap::new(), true);
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert_eq!(
                assembled.get(oxdock_env::os::windows::substrate::SYSTEM_ROOT),
                Some(&oxdock_env::os::windows::substrate::FALLBACK_SYSTEM_ROOT.to_string())
            );
            assert_eq!(
                assembled.get(oxdock_env::os::windows::managed::TEMP),
                Some(&"/scratch/tmp".to_string())
            );
            assert_eq!(
                assembled.get(oxdock_env::os::PATH),
                Some(&oxdock_env::os::windows::substrate::SANCTIONED_PATH.to_string())
            );
        } else {
            assert_eq!(
                assembled.get(oxdock_env::os::unix::managed::TMPDIR),
                Some(&"/scratch/tmp".to_string())
            );
            assert_eq!(
                assembled.get(oxdock_env::os::PATH),
                Some(&oxdock_env::os::unix::substrate::SANCTIONED_PATH.to_string())
            );
            assert!(assembled.contains_key(oxdock_env::os::unix::substrate::TERM));
        }
    }

    #[test]
    fn assemble_lets_script_win_tier1b_2_3() {
        let mut script = HashMap::new();
        script.insert(oxdock_env::os::PATH.to_string(), "/custom/bin".to_string());
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            script.insert(
                oxdock_env::os::windows::substrate::PATHEXT.to_string(),
                ".COM".to_string(),
            );
            script.insert(
                oxdock_env::os::windows::managed::TEMP.to_string(),
                "/custom/tmp".to_string(),
            );
        } else {
            script.insert(
                oxdock_env::os::unix::substrate::TERM.to_string(),
                "xterm-256color".to_string(),
            );
            script.insert(
                oxdock_env::os::unix::managed::TMPDIR.to_string(),
                "/custom/tmp".to_string(),
            );
        }
        let assembled = substrate_for_tests().assemble(&script, true);
        assert_eq!(
            assembled.get(oxdock_env::os::PATH),
            Some(&"/custom/bin".to_string())
        );
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert_eq!(
                assembled.get(oxdock_env::os::windows::substrate::PATHEXT),
                Some(&".COM".to_string())
            );
            assert_eq!(
                assembled.get(oxdock_env::os::windows::managed::TEMP),
                Some(&"/custom/tmp".to_string())
            );
        } else {
            assert_eq!(
                assembled.get(oxdock_env::os::unix::substrate::TERM),
                Some(&"xterm-256color".to_string())
            );
            assert_eq!(
                assembled.get(oxdock_env::os::unix::managed::TMPDIR),
                Some(&"/custom/tmp".to_string())
            );
        }
    }

    #[test]
    fn assemble_drops_tier1a_script_values() {
        let mut script = HashMap::new();
        script.insert(
            oxdock_env::os::windows::substrate::SYSTEM_ROOT.to_string(),
            "C:\\Invalid".to_string(),
        );
        script.insert(
            oxdock_env::os::windows::substrate::WINDIR.to_string(),
            "C:\\Invalid".to_string(),
        );
        let assembled = substrate_for_tests().assemble(&script, true);
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert_eq!(
                assembled.get(oxdock_env::os::windows::substrate::SYSTEM_ROOT),
                Some(&oxdock_env::os::windows::substrate::FALLBACK_SYSTEM_ROOT.to_string())
            );
        } else {
            assert!(
                !assembled.contains_key(oxdock_env::os::windows::substrate::SYSTEM_ROOT),
                "non-Windows builds never inject NT invariants: {assembled:?}"
            );
        }
    }

    #[test]
    fn without_baseline_skips_path_but_keeps_tiers() {
        let suppressed = substrate_for_tests().without_baseline();
        let assembled = suppressed.assemble(&HashMap::new(), true);
        assert!(
            !assembled.contains_key(oxdock_env::os::PATH),
            "suppressed baseline must stay absent: {assembled:?}"
        );
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert!(assembled.contains_key(oxdock_env::os::windows::substrate::SYSTEM_ROOT));
        } else {
            assert!(assembled.contains_key(oxdock_env::os::unix::managed::TMPDIR));
        }
    }

    #[test]
    fn transport_assembly_excludes_scratch() {
        let mut script = HashMap::new();
        script.insert("SSHrelay".to_string(), "1".to_string());
        let assembled = substrate_for_tests().assemble_transport(&script);
        assert_eq!(assembled.get("SSHrelay"), Some(&"1".to_string()));
        assert!(
            !assembled.values().any(|v| v == "/scratch/tmp"),
            "transports must not bind session scratch: {assembled:?}"
        );
        assert!(assembled.contains_key(oxdock_env::os::PATH));
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert!(assembled.contains_key(oxdock_env::os::windows::substrate::SYSTEM_ROOT));
        }
    }

    #[test]
    fn identity_defaults_to_sandbox_not_host() {
        let assembled = substrate_for_tests().assemble(&HashMap::new(), true);
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert_eq!(
                assembled.get(oxdock_env::os::windows::USERNAME),
                Some(&oxdock_env::os::windows::substrate::MANAGED_IDENTITY.to_string())
            );
            assert_eq!(
                assembled.get(oxdock_env::os::windows::managed::HOME),
                Some(&"/scratch/tmp".to_string())
            );
        } else {
            assert_eq!(
                assembled.get(oxdock_env::os::unix::LOGNAME),
                Some(&oxdock_env::os::unix::substrate::MANAGED_IDENTITY.to_string())
            );
            assert_eq!(
                assembled.get(oxdock_env::os::unix::USER),
                Some(&oxdock_env::os::unix::substrate::MANAGED_IDENTITY.to_string())
            );
            assert_eq!(
                assembled.get(oxdock_env::os::unix::managed::HOME),
                Some(&"/scratch/tmp".to_string())
            );
        }
        // Host identity must never flow, even when the host sets it:
        // capture() reads no Tier-4 names at all.
        for probe in ["HOST_HOME_PROBE", "HOST_LOGNAME_PROBE"] {
            assert!(
                !assembled.contains_key(probe),
                "unexpected key: {assembled:?}"
            );
        }
    }

    #[test]
    fn identity_explicit_script_values_win() {
        let mut script = HashMap::new();
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            script.insert(
                oxdock_env::os::windows::USERNAME.to_string(),
                "runner".to_string(),
            );
        } else {
            script.insert(oxdock_env::os::unix::USER.to_string(), "runner".to_string());
            script.insert(
                oxdock_env::os::unix::managed::HOME.to_string(),
                "/home/runner".to_string(),
            );
        }
        let assembled = substrate_for_tests().assemble(&script, true);
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert_eq!(
                assembled.get(oxdock_env::os::windows::USERNAME),
                Some(&"runner".to_string())
            );
        } else {
            assert_eq!(
                assembled.get(oxdock_env::os::unix::USER),
                Some(&"runner".to_string())
            );
            assert_eq!(
                assembled.get(oxdock_env::os::unix::managed::HOME),
                Some(&"/home/runner".to_string())
            );
        }
    }

    #[test]
    fn transport_assembly_excludes_identity() {
        let assembled = substrate_for_tests().assemble_transport(&HashMap::new());
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert!(!assembled.contains_key(oxdock_env::os::windows::USERNAME));
        } else {
            assert!(!assembled.contains_key(oxdock_env::os::unix::LOGNAME));
            assert!(!assembled.contains_key(oxdock_env::os::unix::USER));
            assert!(!assembled.contains_key(oxdock_env::os::unix::managed::HOME));
        }
    }

    #[test]
    fn tier_chapters_cover_every_tier() {
        // The renderer formats tier_section() verbatim: every tier in
        // describe_tiers() needs a chapter, or rendering bails.
        let mut tiers: Vec<&str> = describe_tiers().iter().map(|e| e.tier).collect();
        tiers.sort_unstable();
        tiers.dedup();
        for tier in tiers {
            assert!(
                tier_section(tier).is_some(),
                "tier {tier} has rows but no chapter"
            );
        }
        assert!(tier_section("nope").is_none());
        assert!(!PRECEDENCE_MD.is_empty());
    }

    #[test]
    fn engine_host_reads_names_are_consts_or_patterns() {
        // Names must be env constants (compile-pinned) or explicit
        // `PREFIX_*` patterns; freeform strings would drift silently.
        let reads = engine_host_reads();
        assert!(!reads.is_empty());
        for read in &reads {
            let known = [
                oxdock_env::os::unix::SHELL,
                oxdock_env::os::windows::COMSPEC,
                oxdock_env::os::unix::substrate::TERM,
                oxdock_env::os::windows::substrate::SYSTEM_ROOT,
                oxdock_env::os::windows::substrate::PATHEXT,
                oxdock_env::os::unix::managed::TMPDIR,
            ];
            assert!(
                known.contains(&read.name) || read.name.ends_with("_*"),
                "host read {} is neither const nor pattern",
                read.name
            );
            assert!(!read.reader.is_empty() && !read.purpose.is_empty());
        }
    }

    #[test]
    fn described_tiers_match_assembly() {
        // The rendered ENV_CONTRACTS.md reads describe_tiers(); the
        // engine reads capture()/assemble(). This pins them together:
        // a name added on one side without the other fails here.
        let env = substrate_for_tests();
        let assembled = env.assemble(&HashMap::new(), true);
        let described: Vec<&str> = describe_tiers().iter().map(|e| e.name).collect();
        for name in assembled.keys() {
            assert!(
                described.contains(&name.as_str()),
                "assembled {name} is undescribed"
            );
        }
        #[allow(clippy::disallowed_macros)]
        let windows_only = [
            oxdock_env::os::windows::substrate::SYSTEM_ROOT,
            oxdock_env::os::windows::substrate::WINDIR,
            oxdock_env::os::windows::substrate::PATHEXT,
            oxdock_env::os::windows::managed::TEMP,
            oxdock_env::os::windows::managed::TMP,
            oxdock_env::os::windows::managed::USERPROFILE,
            oxdock_env::os::windows::USERNAME,
        ];
        let applicable = |tier: &str, name: &str| {
            if tier == "4" {
                return false;
            }
            #[allow(clippy::disallowed_macros)]
            if cfg!(windows) {
                return true;
            }
            !windows_only.contains(&name)
        };
        for entry in describe_tiers() {
            if !applicable(entry.tier, entry.name) {
                continue;
            }
            assert!(
                assembled.contains_key(entry.name),
                "described tier-{} {} never assembles",
                entry.tier,
                entry.name
            );
        }
    }

    #[test]
    fn substrate_get_reads_all_tiers() {
        let env = substrate_for_tests();
        assert!(env.get(oxdock_env::os::PATH).is_some());
        assert!(env.get("OXDOCK_DEFINITELY_ABSENT_12345").is_none());
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert!(env.get(oxdock_env::os::windows::managed::TMP).is_some());
        } else {
            assert!(env.get(oxdock_env::os::unix::managed::TMPDIR).is_some());
            assert!(env.contains(oxdock_env::os::unix::substrate::TERM));
        }
    }
}
