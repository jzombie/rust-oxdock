//! Central host-injection policy: the single place deciding what may flow
//! from the host OS into scripts and spawned children.
//!
//! Seeded-map world, stated once: session start writes sandbox defaults
//! directly into the script env map ([`seed_defaults`]); explicit script
//! `ENV`/`INHERIT_ENV` overwrite entries uniformly; `ISOLATED` resets the
//! map to the same seed. No read-through proxies, no suppression
//! branches: the map is the single source of truth and every spawn site
//! stages it verbatim after [`check_spawn_anchors`].
//!
//! Names the engine reads from the host are narrow and listed:
//! loader/terminal fallbacks (`SystemRoot`, `PATHEXT`, `TERM`) with
//! synthetic defaults, the transport profile bridge (`HOME`,
//! `USERPROFILE`), and shell resolution. Nothing else flows ambiently.
//!
//! Every spawn site stages through here instead of hand-rolling its
//! own overlay and diagnostics, so the rule cannot drift per call site.
//!
//! Rendered reference: `ENV_CONTRACTS.md` at the repository root
//! (generated from the seed tables below by `cargo run -p docs-gen`;
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
/// instead of an undiagnosable OS error. Validates the FINAL staged
/// map with no caller-specific flags: the seed has already populated
/// it, so `PATH` is required for bare program names (and always when
/// `require_path` is set), and `SystemRoot` is required on Windows
/// always. This stays a fail-safe against spawn sites that forget
/// the seed.
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
pub fn names_eq(a: &str, b: &str) -> bool {
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

/// Opening chapter of the contract reference, sourced here with the
/// rest of the prose.
pub const CONTRACT_INTRO_MD: &str = indoc::indoc! {"
    ## Seeded environment

    Session start writes sandbox defaults directly into the script
    environment map; explicit script `ENV`/`INHERIT_ENV` overwrite
    entries uniformly; `ISOLATED` resets the map to the same seed.
    Nothing injects at spawn time and nothing is immutable: every
    default below is overridable like any other variable.
"};

/// Baseline-defaults chapter preamble, sourced here with the table.
pub const BASELINE_INTRO_MD: &str = "### Sandbox baseline defaults\n\nSeeded at session start and on every `ISOLATED` entry. Explicit script values win.\n";

/// Explicit-routing chapter, sourced here so the rule lives with the
/// seed it constrains.
pub const ROUTING_MD: &str = indoc::indoc! {"
    ### Explicit routing

    Host variables enter scripts only when listed in `INHERIT_ENV`
    (inside `ISOLATED`, only when listed in the block allowlist, which
    copies from the enclosing scope). Absent stays absent.
"};

/// Precedence and enforcement chapter, sourced here so the guarantee
/// the doc states is reviewed alongside the seed that keeps it.
pub const PRECEDENCE_MD: &str = indoc::indoc! {"
    ### Precedence and enforcement

    Uniform script-wins: the seed populates defaults, and explicit
    script `ENV`/`INHERIT_ENV` overwrite them like any other variable.
    No name is immutable. `ISOLATED` resets to the identical seed, so
    inner and outer scopes start from the same defaults.
    `check_spawn_anchors` validates the final staged map with no
    caller-specific flags: present anchors pass, absent ones bail
    naming the `INHERIT_ENV` fix.

    Source of truth: `crates/sys/oxdock-fs/src/env.rs` (names) and
    `crates/sys/oxdock-process/src/env_policy.rs` (seeding). This
    file is generated; do not edit by hand.
"};

/// Host-reads chapter preamble, sourced here with the inventory it
/// introduces.
pub const HOST_READS_INTRO_MD: &str =
    "### Host reads\n\nVariables the engine process itself touches (not seeds into scripts).\n";

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
/// contract documents interaction as well as seeding. The baseline
/// table says what scripts start with; this says what the engine
/// process itself touches. `oxdock_fs::env::oxdock` / `cargo`
/// plumbing names are explicit call-site reads (cache roots,
/// manifest dirs), never ambient inheritance; they are listed by
/// module, not per variable.
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
            name: unix::TERM,
            reader: "seed_defaults",
            purpose: "seeded default; synthetic `dumb` when the host sets nothing",
        },
        HostRead {
            name: windows::SYSTEM_ROOT,
            reader: "seed_defaults",
            purpose: "seeded default; fallback is the `C:\\Windows` constant",
        },
        HostRead {
            name: windows::PATHEXT,
            reader: "seed_defaults",
            purpose: "seeded default; synthetic list when the host sets nothing",
        },
        HostRead {
            name: unix::HOME,
            reader: "seed_transport",
            purpose: "transport profile bridge only: the host value reaches `ssh` key discovery, never scripts",
        },
        HostRead {
            name: windows::USERPROFILE,
            reader: "seed_transport",
            purpose: "transport profile bridge only: the host value reaches `ssh.exe` key discovery, never scripts",
        },
        HostRead {
            name: unix::TMPDIR,
            reader: "LazyGuardedTempDir::new (via std::env::temp_dir)",
            purpose: "scratch root placement only; the value is never seeded",
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
/// One declarative row per seeded variable: the single source of
/// truth for sandbox defaults. [`seed_defaults`] interprets `value`;
/// [`describe_baseline_defaults`] renders `doc`. Adding a default
/// means adding one row here: a name seeded but undescribed (or vice
/// versa) is unrepresentable, so no parity test polices the mapping.
struct SeedRow {
    /// Variable name (an `oxdock_fs::env` constant value).
    name: &'static str,
    /// How the seeded value is computed.
    value: SeedValue,
    /// Documented default source, rendered verbatim into the contract.
    doc: &'static str,
}

/// How one seed row computes its value.
enum SeedValue {
    /// Session `<scratch>/tmp`.
    ScratchTmp,
    /// Session `<scratch>/target`.
    ScratchTarget,
    /// Fixed sandbox name (`os::SANDBOX_IDENTITY`).
    Identity,
    /// Platform `DEFAULT_PATH` baseline.
    BaselinePath,
    /// Host value when set, else the fallback constant.
    HostOrFallback(&'static str),
    /// Copy of an already-seeded row (resolved in a second pass, so
    /// row order is irrelevant).
    AliasOf(&'static str),
}

/// Uniform rule for every seeded row: scripts win.
pub const SEED_RULE: &str = "seeded default: explicit script values win";

/// Unix seed rows: names, value sources, and documented defaults.
fn unix_seed_table() -> Vec<SeedRow> {
    use oxdock_env::os::unix;
    vec![
        SeedRow {
            name: oxdock_env::os::PATH,
            value: SeedValue::BaselinePath,
            doc: "sandbox baseline (`/usr/bin:/bin:/usr/sbin:/sbin`, `C:\\Windows\\System32;C:\\Windows`)",
        },
        SeedRow {
            name: unix::TMPDIR,
            value: SeedValue::ScratchTmp,
            doc: "session `<scratch>/tmp`",
        },
        SeedRow {
            name: unix::HOME,
            value: SeedValue::ScratchTmp,
            doc: "session `<scratch>/tmp`, so tools cannot discover host configs",
        },
        SeedRow {
            name: unix::LOGNAME,
            value: SeedValue::Identity,
            doc: "fixed sandbox name `oxdock` (shells would otherwise synthesize host identity from `getpwuid`)",
        },
        SeedRow {
            name: unix::USER,
            value: SeedValue::Identity,
            doc: "fixed sandbox name `oxdock`",
        },
        SeedRow {
            name: unix::TERM,
            value: SeedValue::HostOrFallback(unix::DEFAULT_TERM),
            doc: "host value, else `dumb`",
        },
    ]
}

/// Windows-only seed rows. The Windows seed is these plus the full
/// unix table: cross-compiled unix utilities and unix shells running
/// on Windows (`git-bash`, MSYS2, Cygwin) read `TMPDIR`/`TERM`/
/// `LOGNAME`/`USER`, not their `TEMP`/native counterparts, so the
/// sandbox binds both spellings to the same scratch.
fn windows_extra_seed_table() -> Vec<SeedRow> {
    use oxdock_env::os::windows;
    vec![
        SeedRow {
            name: windows::TEMP,
            value: SeedValue::ScratchTmp,
            doc: "session `<scratch>/tmp`",
        },
        SeedRow {
            name: windows::TMP,
            value: SeedValue::ScratchTmp,
            doc: "session `<scratch>/tmp`",
        },
        SeedRow {
            name: windows::USERPROFILE,
            value: SeedValue::ScratchTmp,
            doc: "session `<scratch>/tmp` (native profile anchor; the Win32 API would otherwise resolve the host profile)",
        },
        SeedRow {
            name: windows::USERNAME,
            value: SeedValue::Identity,
            doc: "fixed sandbox name `oxdock`",
        },
        SeedRow {
            name: windows::SYSTEM_ROOT,
            value: SeedValue::HostOrFallback(windows::DEFAULT_SYSTEM_ROOT),
            doc: "host value, else the `C:\\Windows` fallback",
        },
        SeedRow {
            name: windows::WINDIR,
            value: SeedValue::AliasOf(windows::SYSTEM_ROOT),
            doc: "same root as `SystemRoot`",
        },
        SeedRow {
            name: windows::PATHEXT,
            value: SeedValue::HostOrFallback(windows::DEFAULT_PATHEXT),
            doc: "host value, else `.COM;.EXE;.BAT;.CMD`",
        },
    ]
}

/// Seed rows for the current platform: the unix table everywhere,
// plus the Windows extras on Windows.
fn platform_seed_table() -> Vec<SeedRow> {
    let mut rows = unix_seed_table();
    #[allow(clippy::disallowed_macros)]
    if cfg!(windows) {
        rows.extend(windows_extra_seed_table());
    }
    rows
}

/// Platform-independent seed rows, appended on every platform.
fn common_seed_table() -> Vec<SeedRow> {
    vec![SeedRow {
        name: oxdock_env::cargo::TARGET_DIR,
        value: SeedValue::ScratchTarget,
        doc: "session `<scratch>/target`, owned by cargo alone",
    }]
}

/// Sandbox defaults for one session, written directly into the script
/// env map at session start and reused verbatim for every `ISOLATED`
/// reset: the current platform's seed table interpreted row by row, no
/// per-name code. Explicit script `ENV`/`INHERIT_ENV` overwrite these
/// like any other variable. `scratch_tmp` is the session
/// `<scratch>/tmp` display path; `scratch_target` is the `target`
/// subpath for `CARGO_TARGET_DIR`. Never host-read wholesale: only
/// the narrow `HostOrFallback` rows read the host (with synthetic
/// defaults); anything wider reintroduces dirty `PATH`, credential
/// leaks, and CI drift.
pub fn seed_defaults(scratch_tmp: &str, scratch_target: &str) -> HashMap<String, String> {
    let mut rows = platform_seed_table();
    rows.extend(common_seed_table());
    interpret_seed_table(&rows, scratch_tmp, scratch_target)
}

/// Interpret one row list into a map. A row whose value cannot be
/// determined is omitted, never fatal: an `AliasOf` target missing
/// from the same table simply leaves the alias out (a test proves
/// every table fully resolves, so a dangling alias fails in CI, not
/// in production). No variable enters the map undefined.
fn interpret_seed_table(
    rows: &[SeedRow],
    scratch_tmp: &str,
    scratch_target: &str,
) -> HashMap<String, String> {
    let mut seed = HashMap::with_capacity(rows.len());
    for row in rows {
        let value = match row.value {
            SeedValue::ScratchTmp => scratch_tmp.to_string(),
            SeedValue::ScratchTarget => scratch_target.to_string(),
            SeedValue::Identity => oxdock_env::os::SANDBOX_IDENTITY.to_string(),
            SeedValue::BaselinePath =>
            {
                #[allow(clippy::disallowed_macros)]
                if cfg!(windows) {
                    oxdock_env::os::windows::DEFAULT_PATH.to_string()
                } else {
                    oxdock_env::os::unix::DEFAULT_PATH.to_string()
                }
            }
            SeedValue::HostOrFallback(fallback) => {
                host_var(row.name).unwrap_or_else(|| fallback.to_string())
            }
            SeedValue::AliasOf(_) => continue,
        };
        seed.insert(row.name.to_string(), value);
    }
    for row in rows {
        if let SeedValue::AliasOf(other) = row.value
            && let Some(value) = seed.get(other).cloned()
        {
            seed.insert(row.name.to_string(), value);
        }
    }
    seed
}

/// Transport seed for infrastructure spawns (`ssh`, local binaries):
/// loader invariants plus extension/terminal defaults plus the `PATH`
/// baseline, layered under the sanitized request envs. No scratch, no
/// sandbox identity: transports authenticate as the invoking user, so
/// when the sanitized request lacks a profile binding the seed
/// bridges the host's actual `HOME` (unix) / `USERPROFILE` (windows)
/// for key discovery. An explicitly routed script value always wins.
pub fn seed_transport() -> HashMap<String, String> {
    use oxdock_env::os::{unix, windows};
    let mut seed = HashMap::new();
    #[allow(clippy::disallowed_macros)]
    if cfg!(windows) {
        seed.insert(
            oxdock_env::os::PATH.to_string(),
            windows::DEFAULT_PATH.to_string(),
        );
        let root = host_var(windows::SYSTEM_ROOT)
            .unwrap_or_else(|| windows::DEFAULT_SYSTEM_ROOT.to_string());
        seed.insert(windows::SYSTEM_ROOT.to_string(), root.clone());
        seed.insert(windows::WINDIR.to_string(), root);
        seed.insert(
            windows::PATHEXT.to_string(),
            host_var(windows::PATHEXT).unwrap_or_else(|| windows::DEFAULT_PATHEXT.to_string()),
        );
        if let Some(home) = host_var(windows::USERPROFILE) {
            seed.insert(windows::USERPROFILE.to_string(), home.clone());
            // Ported OpenSSH binaries inspect `$HOME` first: bridge the
            // host home when set, else the profile just bridged.
            if !has_var(&seed, unix::HOME) {
                seed.insert(unix::HOME.to_string(), host_var(unix::HOME).unwrap_or(home));
            }
        }
    } else {
        seed.insert(
            oxdock_env::os::PATH.to_string(),
            unix::DEFAULT_PATH.to_string(),
        );
        seed.insert(
            unix::TERM.to_string(),
            host_var(unix::TERM).unwrap_or_else(|| unix::DEFAULT_TERM.to_string()),
        );
        if let Some(home) = host_var(unix::HOME) {
            seed.insert(unix::HOME.to_string(), home);
        }
    }
    seed
}

/// Whether a request entry survives transport sanitize: true when the
/// session seed lacks the name or holds a different value. Key
/// comparison uses [`names_eq`], so Windows casing differences cannot
/// bypass the filter. Seeded scratch/identity values drop (the
/// transport must not bind session scratch); explicitly routed values
/// that differ survive.
pub fn seed_differs(seed: &HashMap<String, String>, key: &str, value: &str) -> bool {
    match seed.iter().find(|(k, _)| names_eq(k, key)) {
        Some((_, seeded)) => seeded != value,
        None => true,
    }
}

/// One row of the sandbox baseline table: rendered from the seed
/// table, so the documented contract cannot drift from the engine.
pub struct BaselineDefaultEntry {
    /// Variable name (an `oxdock_fs::env` constant value).
    pub name: &'static str,
    /// Where the seeded value comes from when the script is silent.
    pub default_source: &'static str,
    /// What a script declaration does (uniform: [`SEED_RULE`]).
    pub rule: &'static str,
}

/// The sandbox baseline contract as static data, in render order: the
/// union of every seed table mapped to rows. There is exactly one row
/// list because there is exactly one seed list.
pub fn describe_baseline_defaults() -> Vec<BaselineDefaultEntry> {
    let mut rows = unix_seed_table();
    rows.extend(windows_extra_seed_table());
    rows.extend(common_seed_table());
    rows.into_iter()
        .map(|row| BaselineDefaultEntry {
            name: row.name,
            default_source: row.doc,
            rule: SEED_RULE,
        })
        .collect()
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

    fn seed_for_tests() -> HashMap<String, String> {
        seed_defaults("/scratch/tmp", "/scratch/target")
    }

    #[test]
    fn seed_writes_sandbox_defaults() {
        let seed = seed_for_tests();
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert_eq!(
                seed.get(oxdock_env::os::PATH),
                Some(&oxdock_env::os::windows::DEFAULT_PATH.to_string())
            );
            assert_eq!(
                seed.get(oxdock_env::os::windows::SYSTEM_ROOT),
                Some(
                    &std::env::var(oxdock_env::os::windows::SYSTEM_ROOT).unwrap_or_else(|_| {
                        oxdock_env::os::windows::DEFAULT_SYSTEM_ROOT.to_string()
                    })
                )
            );
            assert_eq!(
                seed.get(oxdock_env::os::windows::TEMP),
                Some(&"/scratch/tmp".to_string())
            );
            assert_eq!(
                seed.get(oxdock_env::os::windows::USERPROFILE),
                Some(&"/scratch/tmp".to_string())
            );
            assert_eq!(
                seed.get(oxdock_env::os::windows::USERNAME),
                Some(&oxdock_env::os::SANDBOX_IDENTITY.to_string())
            );
            // Windows inherits the unix rows for cross-compiled tools
            // and unix shells running on Windows.
            assert_eq!(
                seed.get(oxdock_env::os::unix::TMPDIR),
                Some(&"/scratch/tmp".to_string())
            );
            assert_eq!(
                seed.get(oxdock_env::os::unix::LOGNAME),
                Some(&oxdock_env::os::SANDBOX_IDENTITY.to_string())
            );
            assert_eq!(
                seed.get(oxdock_env::os::unix::USER),
                Some(&oxdock_env::os::SANDBOX_IDENTITY.to_string())
            );
            assert!(seed.contains_key(oxdock_env::os::unix::TERM));
        } else {
            assert_eq!(
                seed.get(oxdock_env::os::PATH),
                Some(&oxdock_env::os::unix::DEFAULT_PATH.to_string())
            );
            assert_eq!(
                seed.get(oxdock_env::os::unix::TMPDIR),
                Some(&"/scratch/tmp".to_string())
            );
            assert_eq!(
                seed.get(oxdock_env::os::unix::HOME),
                Some(&"/scratch/tmp".to_string())
            );
            assert_eq!(
                seed.get(oxdock_env::os::unix::LOGNAME),
                Some(&oxdock_env::os::SANDBOX_IDENTITY.to_string())
            );
            assert_eq!(
                seed.get(oxdock_env::os::unix::USER),
                Some(&oxdock_env::os::SANDBOX_IDENTITY.to_string())
            );
            assert!(seed.contains_key(oxdock_env::os::unix::TERM));
        }
        assert_eq!(
            seed.get(oxdock_env::cargo::TARGET_DIR),
            Some(&"/scratch/target".to_string())
        );
    }

    #[test]
    fn seed_reads_no_host_identity() {
        // The seed binds scratch and fixed identity; host login names,
        // homes, and profiles never flow, even when the host sets them.
        let seed = seed_for_tests();
        for probe in ["HOST_HOME_PROBE", "HOST_LOGNAME_PROBE"] {
            assert!(!seed.contains_key(probe), "unexpected key: {seed:?}");
        }
        #[allow(clippy::disallowed_macros)]
        if cfg!(not(windows)) {
            let host_user = std::env::var("USER").unwrap_or_default();
            if !host_user.is_empty() && host_user != oxdock_env::os::SANDBOX_IDENTITY {
                assert_ne!(seed.get(oxdock_env::os::unix::USER), Some(&host_user));
            }
        }
    }

    #[test]
    fn seed_differs_spots_seed_equality() {
        let seed = seed_for_tests();
        assert!(!seed_differs(
            &seed,
            oxdock_env::os::PATH,
            seed.get(oxdock_env::os::PATH).unwrap()
        ));
        assert!(seed_differs(&seed, oxdock_env::os::PATH, "/custom/bin"));
        assert!(seed_differs(&seed, "OXDOCK_CUSTOM_UNSEEDED", "1"));
    }

    #[test]
    fn transport_seed_has_no_scratch_or_identity() {
        let staged = seed_transport();
        assert!(
            !staged.values().any(|v| v == "/scratch/tmp"),
            "transports must not bind session scratch: {staged:?}"
        );
        assert!(staged.contains_key(oxdock_env::os::PATH));
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert!(staged.contains_key(oxdock_env::os::windows::SYSTEM_ROOT));
            assert!(!staged.contains_key(oxdock_env::os::windows::USERNAME));
        } else {
            assert!(!staged.contains_key(oxdock_env::os::unix::LOGNAME));
            assert!(!staged.contains_key(oxdock_env::os::unix::USER));
        }
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
                oxdock_env::os::unix::TERM,
                oxdock_env::os::windows::SYSTEM_ROOT,
                oxdock_env::os::windows::PATHEXT,
                oxdock_env::os::unix::HOME,
                oxdock_env::os::windows::USERPROFILE,
                oxdock_env::os::unix::TMPDIR,
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
    fn seed_tables_are_consistent() {
        // Seed and doc render from the same row lists, so mapping drift
        // is unrepresentable. What remains to pin is structure: row
        // names unique, every row name a defined `env` constant, the
        // interpreter seeding exactly the current platform's rows, and
        // fallback prose containing the live const it names.
        use oxdock_env::os::{unix, windows};
        let mut rows = unix_seed_table();
        rows.extend(windows_extra_seed_table());
        rows.extend(common_seed_table());
        let mut seen = std::collections::HashSet::new();
        for row in &rows {
            assert!(seen.insert(row.name), "seed table duplicates {}", row.name);
        }
        let defined: std::collections::HashSet<&str> = oxdock_env::os::unix::ALL
            .iter()
            .chain(oxdock_env::os::windows::ALL.iter())
            .chain(oxdock_env::cargo::ALL.iter())
            .copied()
            .chain([oxdock_env::os::PATH])
            .collect();
        for row in &rows {
            assert!(
                defined.contains(row.name),
                "seed row {} is not a defined env constant",
                row.name
            );
        }
        // Every table fully resolves: unix rows, windows rows over unix
        // rows (WINDIR aliases SYSTEM_ROOT), and the common row. An
        // alias pointing nowhere panics here, not in production.
        for table in [unix_seed_table(), {
            let mut windows = unix_seed_table();
            windows.extend(windows_extra_seed_table());
            windows
        }] {
            let mut with_common = table;
            with_common.extend(common_seed_table());
            let interpreted = interpret_seed_table(&with_common, "/scratch/tmp", "/scratch/target");
            assert_eq!(
                interpreted.len(),
                with_common.len(),
                "interpreter dropped rows: {interpreted:?}"
            );
            for row in &with_common {
                assert!(
                    interpreted.contains_key(row.name),
                    "seed table row {} never resolves",
                    row.name
                );
            }
        }
        let mut expected: Vec<&str> = platform_seed_table().iter().map(|row| row.name).collect();
        expected.push(oxdock_env::cargo::TARGET_DIR);
        let seed = seed_for_tests();
        assert_eq!(
            seed.len(),
            expected.len(),
            "interpreter dropped rows: {seed:?}"
        );
        for name in expected {
            assert!(seed.contains_key(name), "seed table row {name} never seeds");
        }
        let doc_of = |name: &str| -> &str {
            rows.iter()
                .find(|row| row.name == name)
                .map(|row| row.doc)
                .expect("row missing")
        };
        assert!(doc_of(oxdock_env::os::PATH).contains(unix::DEFAULT_PATH));
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert!(doc_of(oxdock_env::os::PATH).contains(windows::DEFAULT_PATH));
            assert!(doc_of(windows::PATHEXT).contains(windows::DEFAULT_PATHEXT));
            assert!(doc_of(windows::SYSTEM_ROOT).contains(windows::DEFAULT_SYSTEM_ROOT));
        } else {
            assert!(doc_of(unix::TERM).contains(unix::DEFAULT_TERM));
        }
        for entry in describe_baseline_defaults() {
            assert_eq!(entry.rule, SEED_RULE, "row rule must be the shared const");
        }
    }
}
