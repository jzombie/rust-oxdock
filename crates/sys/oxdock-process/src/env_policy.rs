//! Central host-injection policy: the single place deciding what may flow
//! from the host OS into scripts and spawned children.
//!
//! Seeded-map world, stated once: session start mirrors host
//! essentials directly into the script env map ([`seed_defaults`]);
//! explicit script `ENV`/`INHERIT_ENV` overwrite entries uniformly;
//! `ISOLATED` resets the map to the same seed. No read-through
//! proxies, no suppression branches: the map is the single source of
//! truth and every spawn site stages it verbatim after
//! [`check_spawn_anchors`].
//!
//! Names the engine reads from the host are narrow and listed: the
//! essential seed rows below plus loader/terminal fallbacks
//! (`SystemRoot`, `PATHEXT`, `TERM`), the transport profile bridge
//! (`HOME`, `USERPROFILE`), and shell resolution. Nothing else flows
//! ambiently.
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

    Session start mirrors host essentials directly into the script
    environment map; hardcoded fallbacks apply only when the host
    lacks a value, and identity rows stay absent instead of invented.
    Explicit script `ENV`/`INHERIT_ENV` overwrite entries uniformly;
    `ISOLATED` resets the map to the same seed. Nothing injects at
    spawn time and nothing is immutable: every default below is
    overridable like any other variable.
"};

/// Baseline-defaults chapter preamble, sourced here with the table.
pub const BASELINE_INTRO_MD: &str = "### Host-mirrored defaults\n\nMirrored from the host at session start and on every `ISOLATED` entry; fallbacks apply only when the host lacks a value. Explicit script values win.\n";

/// Explicit-routing chapter, sourced here so the rule lives with the
/// seed it constrains.
pub const ROUTING_MD: &str = indoc::indoc! {"
    ### Explicit routing

    Essential variables (`PATH`, `HOME`/`USERPROFILE`, temp bindings,
    loader anchors) mirror the host by default, with hardcoded
    fallbacks only when the host lacks them. Everything else enters
    scripts only when listed in `INHERIT_ENV` (inside `ISOLATED`, only
    when listed in the block allowlist, which copies from the enclosing
    scope). Absent stays absent.
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
            name: oxdock_env::os::PATH,
            reader: "seed_defaults",
            purpose: "mirrored default; platform baseline when the host sets nothing",
        },
        HostRead {
            name: unix::HOME,
            reader: "seed_defaults",
            purpose: "mirrored default; session scratch when the host sets nothing",
        },
        HostRead {
            name: windows::USERPROFILE,
            reader: "seed_defaults",
            purpose: "mirrored default; session scratch when the host sets nothing",
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
/// truth for host-mirrored defaults. [`seed_defaults`] interprets `value`;
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

/// How one seed row computes its value. Every variant reads the
/// host first: a hardcoded value applies only when the host lacks
/// the variable (Miri, bare `env -i`). Scripts declare what the
/// host cannot.
enum SeedValue {
    /// Host value when set, else session `<scratch>/tmp`.
    ScratchTmp,
    /// Host `HOME`, else host `USERPROFILE` (ported POSIX tools on
    /// Windows read `HOME` first), else session `<scratch>/tmp`.
    HomeDir,
    /// Session `<scratch>/target`.
    ScratchTarget,
    /// Host value when set; omitted when the host lacks it. No
    /// fiction: identity comes from the operator, never the engine.
    HostOnly,
    /// Host value when set, else the platform `DEFAULT_PATH` baseline.
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
            doc: "host value, else the platform baseline (`/usr/bin:/bin:/usr/sbin:/sbin`, `C:\\Windows\\System32;C:\\Windows`)",
        },
        SeedRow {
            name: unix::TMPDIR,
            value: SeedValue::ScratchTmp,
            doc: "host value, else session `<scratch>/tmp`",
        },
        SeedRow {
            name: unix::HOME,
            value: SeedValue::HomeDir,
            doc: "host `HOME`, else host `USERPROFILE`, else session `<scratch>/tmp`",
        },
        SeedRow {
            name: unix::LOGNAME,
            value: SeedValue::HostOnly,
            doc: "host value; omitted when unset",
        },
        SeedRow {
            name: unix::USER,
            value: SeedValue::HostOnly,
            doc: "host value; omitted when unset",
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
/// `LOGNAME`/`USER`, not their `TEMP`/native counterparts, so both
/// spellings resolve on Windows. The install-location rows ride along
/// because native toolchain discovery reads them from the environment:
/// without install roots and the interpreter in the child env, MSVC
/// linker lookup falls back to `PATH` and finds Git's `link.exe`.
fn windows_extra_seed_table() -> Vec<SeedRow> {
    use oxdock_env::os::windows;
    vec![
        SeedRow {
            name: windows::TEMP,
            value: SeedValue::ScratchTmp,
            doc: "host value, else session `<scratch>/tmp`",
        },
        SeedRow {
            name: windows::TMP,
            value: SeedValue::ScratchTmp,
            doc: "host value, else session `<scratch>/tmp`",
        },
        SeedRow {
            name: windows::USERPROFILE,
            value: SeedValue::ScratchTmp,
            doc: "host value, else session `<scratch>/tmp` (native profile anchor)",
        },
        SeedRow {
            name: windows::USERNAME,
            value: SeedValue::HostOnly,
            doc: "host value; omitted when unset",
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
        SeedRow {
            name: windows::PROGRAM_FILES,
            value: SeedValue::HostOnly,
            doc: "host value; omitted when unset",
        },
        SeedRow {
            name: windows::PROGRAM_FILES_X86,
            value: SeedValue::HostOnly,
            doc: "host value; omitted when unset",
        },
        SeedRow {
            name: windows::PROGRAM_W6432,
            value: SeedValue::HostOnly,
            doc: "host value; omitted when unset",
        },
        SeedRow {
            name: windows::COMSPEC,
            value: SeedValue::HostOnly,
            doc: "host value; omitted when unset",
        },
        SeedRow {
            name: windows::SYSTEM_DRIVE,
            value: SeedValue::HostOnly,
            doc: "host value; omitted when unset",
        },
        SeedRow {
            name: windows::PROCESSOR_ARCHITECTURE,
            value: SeedValue::HostOnly,
            doc: "host value; omitted when unset",
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

/// Session seed, written directly into the script env map at
/// session start and reused verbatim for every `ISOLATED` reset: the
/// current platform's seed table interpreted row by row, no per-name
/// code. Every row mirrors the host when the host defines the
/// variable; hardcoded values are fallbacks for a missing host value
/// only. Explicit script `ENV`/`INHERIT_ENV` overwrite these like any
/// other variable. `scratch_tmp` is the session `<scratch>/tmp`
/// display path; `scratch_target` is the `target` subpath for
/// `CARGO_TARGET_DIR`. Narrow by name, never wholesale: only the
/// listed rows read the host, so ambient secrets and harness
/// internals still stay out unless explicitly routed.
pub fn seed_defaults(scratch_tmp: &str, scratch_target: &str) -> HashMap<String, String> {
    let mut rows = platform_seed_table();
    rows.extend(common_seed_table());
    interpret_seed_table(&rows, scratch_tmp, scratch_target, &host_var)
}

/// Interpret one row list into a map. `host` supplies host values so
/// tests can stub the host hermetically; production passes
/// [`host_var`]. A row whose value cannot be determined is omitted,
/// never fatal: an `AliasOf` target missing from the same table, or a
/// `HostOnly` name the host lacks, simply leaves the name out (a test
/// proves every table fully resolves against a complete stub host, so
/// a dangling alias fails in CI, not in production). No variable
/// enters the map undefined.
fn interpret_seed_table(
    rows: &[SeedRow],
    scratch_tmp: &str,
    scratch_target: &str,
    host: &dyn Fn(&str) -> Option<String>,
) -> HashMap<String, String> {
    let mut seed = HashMap::with_capacity(rows.len());
    for row in rows {
        let value = match row.value {
            SeedValue::ScratchTmp => host(row.name).unwrap_or_else(|| scratch_tmp.to_string()),
            SeedValue::HomeDir => host(row.name)
                .or_else(|| host(oxdock_env::os::windows::USERPROFILE))
                .unwrap_or_else(|| scratch_tmp.to_string()),
            SeedValue::ScratchTarget => scratch_target.to_string(),
            SeedValue::HostOnly => {
                let Some(mirrored) = host(row.name) else {
                    continue;
                };
                mirrored
            }
            SeedValue::BaselinePath => {
                if let Some(mirrored) = host(row.name) {
                    mirrored
                } else {
                    #[allow(clippy::disallowed_macros)]
                    if cfg!(windows) {
                        oxdock_env::os::windows::DEFAULT_PATH.to_string()
                    } else {
                        oxdock_env::os::unix::DEFAULT_PATH.to_string()
                    }
                }
            }
            SeedValue::HostOrFallback(fallback) => {
                host(row.name).unwrap_or_else(|| fallback.to_string())
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
/// loader invariants plus extension/terminal defaults plus the host
/// `PATH`, layered under the sanitized request envs. No scratch and
/// no identity rows: transports authenticate as the invoking user, so
/// when the sanitized request lacks a profile binding the seed
/// bridges the host's actual `HOME` (unix) / `USERPROFILE` (windows)
/// for key discovery. An explicitly routed script value always wins.
pub fn seed_transport() -> HashMap<String, String> {
    transport_seed(&host_var)
}

/// Transport seed with a stubbable host lookup (see
/// [`interpret_seed_table`]): production passes [`host_var`].
fn transport_seed(host: &dyn Fn(&str) -> Option<String>) -> HashMap<String, String> {
    use oxdock_env::os::{unix, windows};
    let mut seed = HashMap::new();
    #[allow(clippy::disallowed_macros)]
    if cfg!(windows) {
        seed.insert(
            oxdock_env::os::PATH.to_string(),
            host(oxdock_env::os::PATH).unwrap_or_else(|| windows::DEFAULT_PATH.to_string()),
        );
        let root =
            host(windows::SYSTEM_ROOT).unwrap_or_else(|| windows::DEFAULT_SYSTEM_ROOT.to_string());
        seed.insert(windows::SYSTEM_ROOT.to_string(), root.clone());
        seed.insert(windows::WINDIR.to_string(), root);
        seed.insert(
            windows::PATHEXT.to_string(),
            host(windows::PATHEXT).unwrap_or_else(|| windows::DEFAULT_PATHEXT.to_string()),
        );
        if let Some(home) = host(windows::USERPROFILE) {
            seed.insert(windows::USERPROFILE.to_string(), home.clone());
            // Ported OpenSSH binaries inspect `$HOME` first: bridge the
            // host home when set, else the profile just bridged.
            if !has_var(&seed, unix::HOME) {
                seed.insert(unix::HOME.to_string(), host(unix::HOME).unwrap_or(home));
            }
        }
    } else {
        seed.insert(
            oxdock_env::os::PATH.to_string(),
            host(oxdock_env::os::PATH).unwrap_or_else(|| unix::DEFAULT_PATH.to_string()),
        );
        seed.insert(
            unix::TERM.to_string(),
            host(unix::TERM).unwrap_or_else(|| unix::DEFAULT_TERM.to_string()),
        );
        if let Some(home) = host(unix::HOME) {
            seed.insert(unix::HOME.to_string(), home);
        }
    }
    seed
}

/// Whether a request entry survives transport sanitize: true when the
/// session seed lacks the name or holds a different value. Key
/// comparison uses [`names_eq`], so Windows casing differences cannot
/// bypass the filter. Seeded scratch values drop (the transport must
/// not bind session scratch); explicitly routed values that differ
/// survive.
pub fn seed_differs(seed: &HashMap<String, String>, key: &str, value: &str) -> bool {
    match seed.iter().find(|(k, _)| names_eq(k, key)) {
        Some((_, seeded)) => seeded != value,
        None => true,
    }
}

/// One row of the host-mirrored defaults table: rendered from the seed
/// table, so the documented contract cannot drift from the engine.
/// Tests also derive expectations from these rows instead of
/// hardcoding names, so adding a seed row needs no test edits.
pub struct BaselineDefaultEntry {
    /// Variable name (an `oxdock_fs::env` constant value).
    pub name: &'static str,
    /// Where the seeded value comes from when the script is silent.
    pub default_source: &'static str,
    /// What a script declaration does (uniform: [`SEED_RULE`]).
    pub rule: &'static str,
    /// Whether the row omits the name when the host lacks it
    /// (`HostOnly` rows): tests expect such names only when the host
    /// defines them.
    pub omits_when_unset: bool,
    /// Hardcoded fallback for rows that always seed
    /// (`BaselinePath`/`HostOrFallback`); `None` for scratch-shaped,
    /// alias, and omitting rows, whose tests assert shape, target
    /// equality, or absence instead.
    pub fallback: Option<&'static str>,
}

/// Map one seed row to its contract entry: the single place turning
/// `SeedValue` semantics into documented and testable facts.
fn entry_of(row: &SeedRow) -> BaselineDefaultEntry {
    let (omits_when_unset, fallback) = match row.value {
        SeedValue::HostOnly => (true, None),
        SeedValue::BaselinePath =>
        {
            #[allow(clippy::disallowed_macros)]
            if cfg!(windows) {
                (false, Some(oxdock_env::os::windows::DEFAULT_PATH))
            } else {
                (false, Some(oxdock_env::os::unix::DEFAULT_PATH))
            }
        }
        SeedValue::HostOrFallback(fallback) => (false, Some(fallback)),
        SeedValue::ScratchTmp
        | SeedValue::HomeDir
        | SeedValue::ScratchTarget
        | SeedValue::AliasOf(_) => (false, None),
    };
    BaselineDefaultEntry {
        name: row.name,
        default_source: row.doc,
        rule: SEED_RULE,
        omits_when_unset,
        fallback,
    }
}

/// The host-mirrored defaults contract as static data, in render order: the
/// union of every seed table mapped to rows. There is exactly one row
/// list because there is exactly one seed list.
pub fn describe_baseline_defaults() -> Vec<BaselineDefaultEntry> {
    let mut rows = unix_seed_table();
    rows.extend(windows_extra_seed_table());
    rows.extend(common_seed_table());
    rows.iter().map(entry_of).collect()
}

/// Seed entries for the current platform (platform table plus the
/// common rows): what the child env actually contains, for tests that
/// assert exact dump key sets without hardcoding names.
pub fn current_platform_seed() -> Vec<BaselineDefaultEntry> {
    let mut rows = platform_seed_table();
    rows.extend(common_seed_table());
    rows.iter().map(entry_of).collect()
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

    /// Stub host for hermetic proofs: parallel tests cannot skew a
    /// private map, and no test mutates the real process env (a live
    /// `PATH`/`HOME` guard would race concurrent spawns).
    fn stub_host(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn seed_mirrors_host_essentials() {
        // Host-first: every essential mirrors the stub host. Scratch
        // and fallback constants apply only to names the stub lacks
        // (proven by `seed_falls_back_without_host`).
        use oxdock_env::os::{unix, windows};
        let stub = stub_host(&[
            (oxdock_env::os::PATH, "/host/bin"),
            (unix::HOME, "/host/home"),
            (unix::TMPDIR, "/host/tmp"),
            (unix::USER, "host-user"),
            (unix::LOGNAME, "host-user"),
            (unix::TERM, "host-term"),
            (windows::SYSTEM_ROOT, "C:\\HostWindows"),
            (windows::PATHEXT, ".HOST"),
            (windows::TEMP, "/host/temp"),
            (windows::TMP, "/host/temp"),
            (windows::USERPROFILE, "/host/profile"),
            (windows::USERNAME, "host-user"),
            (windows::PROGRAM_FILES, "C:\\HostProgramFiles"),
            (windows::PROGRAM_FILES_X86, "C:\\HostProgramFilesX86"),
            (windows::PROGRAM_W6432, "C:\\HostProgramW6432"),
            (windows::COMSPEC, "C:\\HostWindows\\system32\\cmd.exe"),
            (windows::SYSTEM_DRIVE, "C:"),
            (windows::PROCESSOR_ARCHITECTURE, "AMD64"),
        ]);
        let lookup = |name: &str| stub.get(name).cloned();
        let mut rows = platform_seed_table();
        rows.extend(common_seed_table());
        let seed = interpret_seed_table(&rows, "/scratch/tmp", "/scratch/target", &lookup);
        assert_eq!(
            seed.get(oxdock_env::os::PATH).map(String::as_str),
            Some("/host/bin")
        );
        assert_eq!(seed.get(unix::HOME).map(String::as_str), Some("/host/home"));
        assert_eq!(
            seed.get(unix::TMPDIR).map(String::as_str),
            Some("/host/tmp")
        );
        assert_eq!(seed.get(unix::USER).map(String::as_str), Some("host-user"));
        assert_eq!(
            seed.get(unix::LOGNAME).map(String::as_str),
            Some("host-user")
        );
        assert_eq!(seed.get(unix::TERM).map(String::as_str), Some("host-term"));
        assert_eq!(
            seed.get(oxdock_env::cargo::TARGET_DIR).map(String::as_str),
            Some("/scratch/target")
        );
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert_eq!(
                seed.get(windows::SYSTEM_ROOT).map(String::as_str),
                Some("C:\\HostWindows")
            );
            assert_eq!(
                seed.get(windows::WINDIR).map(String::as_str),
                Some("C:\\HostWindows"),
                "WINDIR aliases the mirrored SystemRoot"
            );
            assert_eq!(
                seed.get(windows::PATHEXT).map(String::as_str),
                Some(".HOST")
            );
            assert_eq!(
                seed.get(windows::TEMP).map(String::as_str),
                Some("/host/temp")
            );
            assert_eq!(
                seed.get(windows::TMP).map(String::as_str),
                Some("/host/temp")
            );
            assert_eq!(
                seed.get(windows::USERPROFILE).map(String::as_str),
                Some("/host/profile")
            );
            assert_eq!(
                seed.get(windows::USERNAME).map(String::as_str),
                Some("host-user")
            );
            assert_eq!(
                seed.get(windows::PROGRAM_FILES).map(String::as_str),
                Some("C:\\HostProgramFiles")
            );
            assert_eq!(
                seed.get(windows::PROGRAM_FILES_X86).map(String::as_str),
                Some("C:\\HostProgramFilesX86")
            );
            assert_eq!(
                seed.get(windows::PROGRAM_W6432).map(String::as_str),
                Some("C:\\HostProgramW6432")
            );
            assert_eq!(
                seed.get(windows::COMSPEC).map(String::as_str),
                Some("C:\\HostWindows\\system32\\cmd.exe")
            );
            assert_eq!(
                seed.get(windows::SYSTEM_DRIVE).map(String::as_str),
                Some("C:")
            );
            assert_eq!(
                seed.get(windows::PROCESSOR_ARCHITECTURE).map(String::as_str),
                Some("AMD64")
            );
        }
    }

    #[test]
    fn seed_home_bridges_userprofile() {
        // Ported POSIX tools on Windows read `HOME` first: with no
        // host `HOME`, the row bridges the host profile instead of
        // falling to scratch.
        let stub = stub_host(&[(oxdock_env::os::windows::USERPROFILE, "/host/profile")]);
        let lookup = |name: &str| stub.get(name).cloned();
        let mut rows = platform_seed_table();
        rows.extend(common_seed_table());
        let seed = interpret_seed_table(&rows, "/scratch/tmp", "/scratch/target", &lookup);
        assert_eq!(
            seed.get(oxdock_env::os::unix::HOME).map(String::as_str),
            Some("/host/profile")
        );
    }

    #[test]
    fn seed_falls_back_without_host() {
        // An empty stub host (Miri, bare `env -i`): baselines and
        // scratch apply, identity rows stay absent instead of invented.
        use oxdock_env::os::{unix, windows};
        let mut rows = platform_seed_table();
        rows.extend(common_seed_table());
        let seed = interpret_seed_table(&rows, "/scratch/tmp", "/scratch/target", &|_: &str| {
            None::<String>
        });
        #[allow(clippy::disallowed_macros)]
        let expected_path = if cfg!(windows) {
            windows::DEFAULT_PATH
        } else {
            unix::DEFAULT_PATH
        };
        assert_eq!(
            seed.get(oxdock_env::os::PATH).map(String::as_str),
            Some(expected_path)
        );
        assert_eq!(
            seed.get(unix::HOME).map(String::as_str),
            Some("/scratch/tmp")
        );
        assert_eq!(
            seed.get(unix::TMPDIR).map(String::as_str),
            Some("/scratch/tmp")
        );
        assert!(
            !seed.contains_key(unix::USER) && !seed.contains_key(unix::LOGNAME),
            "absent identity must stay absent: {seed:?}"
        );
        assert_eq!(
            seed.get(unix::TERM).map(String::as_str),
            Some(unix::DEFAULT_TERM)
        );
        assert_eq!(
            seed.get(oxdock_env::cargo::TARGET_DIR).map(String::as_str),
            Some("/scratch/target")
        );
        #[allow(clippy::disallowed_macros)]
        if cfg!(windows) {
            assert_eq!(
                seed.get(windows::SYSTEM_ROOT).map(String::as_str),
                Some(windows::DEFAULT_SYSTEM_ROOT)
            );
            assert_eq!(
                seed.get(windows::WINDIR).map(String::as_str),
                Some(windows::DEFAULT_SYSTEM_ROOT),
                "WINDIR aliases the fallback SystemRoot"
            );
            assert_eq!(
                seed.get(windows::PATHEXT).map(String::as_str),
                Some(windows::DEFAULT_PATHEXT)
            );
            assert_eq!(
                seed.get(windows::TEMP).map(String::as_str),
                Some("/scratch/tmp")
            );
            assert_eq!(
                seed.get(windows::TMP).map(String::as_str),
                Some("/scratch/tmp")
            );
            assert_eq!(
                seed.get(windows::USERPROFILE).map(String::as_str),
                Some("/scratch/tmp")
            );
            assert!(
                !seed.contains_key(windows::USERNAME),
                "absent identity must stay absent: {seed:?}"
            );
            for name in [
                windows::PROGRAM_FILES,
                windows::PROGRAM_FILES_X86,
                windows::PROGRAM_W6432,
                windows::COMSPEC,
                windows::SYSTEM_DRIVE,
                windows::PROCESSOR_ARCHITECTURE,
            ] {
                assert!(
                    !seed.contains_key(name),
                    "absent install-location rows must stay absent: {seed:?}"
                );
            }
        }
    }

    #[test]
    fn seed_omits_absent_identity() {
        // Identity is host-or-absent: a stub host without login names
        // seeds no identity rows, and unrelated probes never appear.
        use oxdock_env::os::unix;
        let stub = stub_host(&[
            (oxdock_env::os::PATH, "/host/bin"),
            ("HOST_HOME_PROBE", "/nowhere"),
        ]);
        let lookup = |name: &str| stub.get(name).cloned();
        let mut rows = platform_seed_table();
        rows.extend(common_seed_table());
        let seed = interpret_seed_table(&rows, "/scratch/tmp", "/scratch/target", &lookup);
        assert!(
            !seed.contains_key(unix::USER) && !seed.contains_key(unix::LOGNAME),
            "absent identity must stay absent: {seed:?}"
        );
        assert!(
            !seed.contains_key("HOST_HOME_PROBE"),
            "unlisted names never seed: {seed:?}"
        );
        assert_eq!(
            seed.get(oxdock_env::os::PATH).map(String::as_str),
            Some("/host/bin")
        );
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
    fn transport_seed_mirrors_host_path() {
        // Transports resolve through the host PATH, with the platform
        // baseline only as fallback.
        let stub = stub_host(&[(oxdock_env::os::PATH, "/host/bin")]);
        let staged = transport_seed(&|name: &str| stub.get(name).cloned());
        assert_eq!(
            staged.get(oxdock_env::os::PATH).map(String::as_str),
            Some("/host/bin")
        );
        let bare = transport_seed(&|_: &str| None::<String>);
        #[allow(clippy::disallowed_macros)]
        let expected = if cfg!(windows) {
            oxdock_env::os::windows::DEFAULT_PATH
        } else {
            oxdock_env::os::unix::DEFAULT_PATH
        };
        assert_eq!(
            bare.get(oxdock_env::os::PATH).map(String::as_str),
            Some(expected)
        );
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
                oxdock_env::os::PATH,
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
        // Every table fully resolves against a complete stub host:
        // unix rows, windows rows over unix rows (WINDIR aliases
        // SYSTEM_ROOT), and the common row. An alias pointing nowhere
        // panics here, not in production. The stub (not the live
        // process env) keeps the proof hermetic on every machine.
        let stub: HashMap<String, String> = rows
            .iter()
            .map(|row| (row.name.to_string(), format!("host-{}", row.name)))
            .collect();
        let lookup = |name: &str| stub.get(name).cloned();
        for table in [unix_seed_table(), {
            let mut windows = unix_seed_table();
            windows.extend(windows_extra_seed_table());
            windows
        }] {
            let mut with_common = table;
            with_common.extend(common_seed_table());
            let interpreted =
                interpret_seed_table(&with_common, "/scratch/tmp", "/scratch/target", &lookup);
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
        let mut platform = platform_seed_table();
        platform.extend(common_seed_table());
        let seed = interpret_seed_table(&platform, "/scratch/tmp", "/scratch/target", &lookup);
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
