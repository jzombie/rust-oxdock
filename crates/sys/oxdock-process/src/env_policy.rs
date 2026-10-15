//! Central host-injection policy: the single place deciding what may flow
//! from the host OS into scripts and spawned children.
//!
//! Mirror-or-absent world, stated once: session start copies host values
//! for the declared names directly into the script env map
//! ([`seed_defaults`]); a host lacking a name leaves it absent, never
//! synthesized. Explicit script `ENV`/`INHERIT_ENV` overwrite entries
//! uniformly; `ISOLATED` resets the map to the same seed. Missing values
//! surface at spawn time through [`check_spawn_anchors`], which names the
//! `INHERIT_ENV` fix: a missing host value is a host problem, reported,
//! never papered over.
//!
//! Names are declared once in the `define_env_contract!` invocation
//! below with their platforms. Doc tables, seed maps, and tests all derive
//! from that declaration, so there is no second list to drift.
//!
//! Every spawn site stages through here instead of hand-rolling its
//! own overlay and diagnostics, so the rule cannot drift per call site.
//!
//! Rendered reference: `ENV_CONTRACTS.md` at the repository root
//! (generated from the declaration below by `cargo run -p docs-gen`;
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

    Session start copies host values for the declared names directly
    into the script environment map. A host lacking a name leaves it
    absent: nothing is synthesized, nothing is immutable. Explicit
    script `ENV`/`INHERIT_ENV` overwrite entries uniformly; `ISOLATED`
    resets the map to the same seed. A missing value surfaces at spawn
    time, with the exact `INHERIT_ENV` declaration to add.
"};

/// Baseline-defaults chapter preamble, sourced here with the table.
pub const BASELINE_INTRO_MD: &str = indoc::indoc! {"
    ### Host-mirrored defaults

    Declared names mirrored from the host at session start and on every
    `ISOLATED` entry, wherever the host defines them. Absent stays
    absent; explicit script values win.
"};

/// Explicit-routing chapter, sourced here so the rule lives with the
/// names it constrains.
pub const ROUTING_MD: &str = indoc::indoc! {"
    ### Explicit routing

    The defaults in the table above mirror the host. Everything else
    enters scripts only when listed in `INHERIT_ENV` (inside `ISOLATED`,
    only when listed in the block allowlist, which copies from the
    enclosing scope). Absent stays absent.
"};

/// Precedence and enforcement chapter, sourced here so the guarantee
/// the doc states is reviewed alongside the declaration that keeps it.
pub const PRECEDENCE_MD: &str = indoc::indoc! {"
    ### Precedence and enforcement

    Uniform script-wins: the seed populates mirrors, and explicit
    script `ENV`/`INHERIT_ENV` overwrite them like any other variable.
    No name is immutable. `ISOLATED` resets to the identical seed, so
    inner and outer scopes start from the same defaults.
    `check_spawn_anchors` validates the final staged map with no
    caller-specific flags: present anchors pass, absent ones bail with
    the exact `INHERIT_ENV` declaration to add.

    Source of truth: the `define_env_contract!` invocation in
    `crates/sys/oxdock-process/src/env_policy.rs` (names live in
    `crates/sys/oxdock-fs/src/env.rs`). This file is generated; do not
    edit by hand.
"};

/// Host-reads chapter preamble, sourced here with the inventory it
/// introduces.
pub const HOST_READS_INTRO_MD: &str = indoc::indoc! {"
    ### Host reads

    Variables the engine process itself touches.
"};

/// Engine-process reads are declared in the `define_env_contract!`
/// invocation below.
/// One declared seed row: name plus the platforms seeding it, rendered
/// into the contract table. Tests derive expectations from these rows
/// instead of hardcoding names, so adding a seed row needs no test edits.
pub struct BaselineDefaultEntry {
    /// Variable name (an `oxdock_fs::env` constant value).
    pub name: &'static str,
    /// Platforms seeding the name, rendered verbatim (`unix`,
    /// `windows`, or `unix, windows`).
    pub platforms: &'static str,
}

/// One host interaction: a variable the engine process itself reads
/// (not injects). Names are `env` constants where one exists;
/// `*-suffix` entries are prefix patterns, not single variables. The
/// reader names the code performing the read; there is no prose to rot.
pub struct HostRead {
    /// Variable name, or `PREFIX_*` pattern.
    pub name: &'static str,
    /// Engine function performing the read.
    pub reader: &'static str,
}

/// Declare the whole host contract in one place: seeded names with
/// their owning families, plus engine-process reads. The invocation
/// below expands to the declared name list, the contract entries, and
/// the host reads, so behavior, docs, and tests share one declaration.
/// The interpreter mirrors every declared name the host defines, on
/// every platform: the family lists document ownership for the contract
/// table, they never gate mirroring. Scope shapes are exactly `[unix]`,
/// `[windows]`, `[unix, windows]`; anything else is a compile error at
/// the declaration site.
macro_rules! define_env_contract {
    (
        seeds { $($name:expr, [$($platform:ident),*];)* }
        reads { $($rname:expr, $rreader:expr;)* }
    ) => {
        /// Every declared seed name, on every platform: the seeder
        /// mirrors the ones the host defines (git-bash on Windows
        /// exports `HOME`/`TERM` itself, so unix names resolve there
        /// with no bridge and no detection).
        fn platform_seed_names() -> Vec<&'static str> {
            vec![ $($name),* ]
        }
        /// Every declared seed row on every platform, for the contract
        /// table.
        pub fn describe_baseline_defaults() -> Vec<BaselineDefaultEntry> {
            vec![ $( define_env_contract!(@entry $name, $($platform),*), )* ]
        }
        /// Seed entries for the current platform: identical to the
        /// declaration, since mirroring is platform-blind. What the
        /// child env actually contains, for tests that assert exact dump
        /// key sets without hardcoding names.
        pub fn current_platform_seed() -> Vec<BaselineDefaultEntry> {
            describe_baseline_defaults()
        }
        /// Every host read in the execution path, declared here so the
        /// contract documents interaction as well as seeding.
        pub fn engine_host_reads() -> Vec<HostRead> {
            vec![ $( HostRead { name: $rname, reader: $rreader } ),* ]
        }
    };
    (@entry $name:expr, unix, windows) => {
        BaselineDefaultEntry { name: $name, platforms: "unix, windows" }
    };
    (@entry $name:expr, unix) => {
        BaselineDefaultEntry { name: $name, platforms: "unix" }
    };
    (@entry $name:expr, windows) => {
        BaselineDefaultEntry { name: $name, platforms: "windows" }
    };
}

// The whole host contract: seeded names with their owning families,
// plus engine-process reads. Everything derives from this invocation:
// the declared name list, the contract table, the host-reads table.
// Family lists document ownership; mirroring itself is platform-blind
// (a declared name seeds wherever the host defines it).
define_env_contract! {
    seeds {
        oxdock_env::os::PATH, [unix, windows];
        oxdock_env::os::unix::HOME, [unix];
        oxdock_env::os::unix::TMPDIR, [unix];
        oxdock_env::os::unix::TERM, [unix];
        oxdock_env::os::unix::LOGNAME, [unix];
        oxdock_env::os::unix::USER, [unix];
        oxdock_env::os::windows::TEMP, [windows];
        oxdock_env::os::windows::TMP, [windows];
        oxdock_env::os::windows::USERPROFILE, [windows];
        oxdock_env::os::windows::LOCALAPPDATA, [windows];
        oxdock_env::os::windows::APPDATA, [windows];
        oxdock_env::os::windows::SYSTEM_ROOT, [windows];
        oxdock_env::os::windows::WINDIR, [windows];
        oxdock_env::os::windows::PATHEXT, [windows];
        oxdock_env::os::windows::PROGRAM_FILES, [windows];
        oxdock_env::os::windows::PROGRAM_FILES_X86, [windows];
        oxdock_env::os::windows::PROGRAM_W6432, [windows];
        oxdock_env::os::windows::COMSPEC, [windows];
        oxdock_env::os::windows::SYSTEM_DRIVE, [windows];
        oxdock_env::os::windows::PROCESSOR_ARCHITECTURE, [windows];
        // Engine-owned, not a host mirror: the interpreter binds the
        // session cargo scratch instead of reading the host.
        oxdock_env::cargo::TARGET_DIR, [unix, windows];
    }
    reads {
        oxdock_env::os::unix::SHELL, "oxdock_process::shell::shell_program";
        oxdock_env::os::windows::COMSPEC, "oxdock_process::shell::shell_program";
        oxdock_env::os::unix::TMPDIR, "LazyGuardedTempDir::new (via std::env::temp_dir)";
        "CARGO_FEATURE_*", "BuiltinEnv::collect";
        "CARGO_CFG_*", "BuiltinEnv::collect";
    }
}

/// Session seed, written directly into the script env map at
/// session start and reused verbatim for every `ISOLATED` reset: the
/// current platform's declared names mirrored from the host, nothing
/// else. A host lacking a name leaves it absent: no fallbacks, no
/// synthesis. Explicit script `ENV`/`INHERIT_ENV` overwrite these like
/// any other variable. `scratch_target` binds the single engine-owned
/// row (`CARGO_TARGET_DIR`); `scratch_tmp` is unused and retained only
/// for the call signature. Narrow by name, never wholesale: only the
/// declared rows read the host, so ambient secrets and harness
/// internals still stay out unless explicitly routed.
pub fn seed_defaults(scratch_tmp: &str, scratch_target: &str) -> HashMap<String, String> {
    let _ = scratch_tmp;
    interpret_seed_table(&platform_seed_names(), scratch_target, &host_var)
}

/// Interpret one name list into a map. `host` supplies host values so
/// tests can stub the host hermetically; production passes
/// [`host_var`]. A name the host lacks is omitted, never fatal. No
/// variable enters the map undefined.
fn interpret_seed_table(
    names: &[&str],
    scratch_target: &str,
    host: &dyn Fn(&str) -> Option<String>,
) -> HashMap<String, String> {
    let mut seed = HashMap::with_capacity(names.len() + 1);
    for name in names {
        // The single engine-owned row: session cargo scratch, not a
        // host mirror. Everything else mirrors or omits.
        if *name == oxdock_env::cargo::TARGET_DIR {
            seed.insert((*name).to_string(), scratch_target.to_string());
            continue;
        }
        if let Some(mirrored) = host(name) {
            seed.insert((*name).to_string(), mirrored);
        }
    }
    seed
}

/// Transport seed for infrastructure spawns (`ssh`, local binaries):
/// the host `PATH`, loader invariants, terminal default, and profile
/// locations, layered under the sanitized request envs. Mirror or omit,
/// like the session seed: no scratch and no synthesis. An explicitly
/// routed script value always wins.
pub fn seed_transport() -> HashMap<String, String> {
    transport_seed(&host_var)
}

/// Transport seed with a stubbable host lookup (see
/// [`interpret_seed_table`]): production passes [`host_var`].
fn transport_seed(host: &dyn Fn(&str) -> Option<String>) -> HashMap<String, String> {
    let mut seed = HashMap::new();
    for name in transport_names() {
        if let Some(value) = host(name) {
            seed.insert(name.to_string(), value);
        }
    }
    seed
}

/// Host names infrastructure spawns mirror. A subset of the declared
/// seed rows: what it takes to resolve and launch a transport, nothing
/// identifying.
fn transport_names() -> Vec<&'static str> {
    use oxdock_env::os::{unix, windows};
    #[allow(clippy::disallowed_macros)]
    if cfg!(windows) {
        vec![
            oxdock_env::os::PATH,
            windows::SYSTEM_ROOT,
            windows::WINDIR,
            windows::PATHEXT,
            unix::HOME,
            windows::USERPROFILE,
            windows::LOCALAPPDATA,
            windows::APPDATA,
        ]
    } else {
        vec![oxdock_env::os::PATH, unix::TERM, unix::HOME]
    }
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
        // Every declared name mirrors the stub host; the engine-owned
        // row binds scratch. Derived from the declaration, so adding a
        // row needs no edit here.
        let stub: HashMap<String, String> = current_platform_seed()
            .iter()
            .map(|entry| (entry.name.to_string(), format!("host-{}", entry.name)))
            .collect();
        let lookup = |name: &str| stub.get(name).cloned();
        let seed = interpret_seed_table(&platform_seed_names(), "/scratch/target", &lookup);
        for entry in current_platform_seed() {
            if entry.name == oxdock_env::cargo::TARGET_DIR {
                assert_eq!(
                    seed.get(entry.name).map(String::as_str),
                    Some("/scratch/target"),
                    "engine-owned row must bind scratch"
                );
            } else {
                assert_eq!(
                    seed.get(entry.name).map(String::as_str),
                    stub.get(entry.name).map(String::as_str),
                    "{} must mirror the host",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn seed_omits_all_without_host() {
        // An empty stub host (Miri, bare `env -i`): every mirror row
        // stays absent; only the engine-owned row binds scratch.
        let seed = interpret_seed_table(&platform_seed_names(), "/scratch/target", &|_: &str| {
            None::<String>
        });
        assert_eq!(
            seed.len(),
            1,
            "empty host must seed only the engine-owned row: {seed:?}"
        );
        assert_eq!(
            seed.get(oxdock_env::cargo::TARGET_DIR).map(String::as_str),
            Some("/scratch/target")
        );
    }

    #[test]
    fn seed_admits_only_declared_names() {
        // Closedness: a stub host carrying an unlisted name seeds no
        // trace of it, and every unlisted declared name stays absent.
        let stub = stub_host(&[
            (oxdock_env::os::PATH, "/host/bin"),
            ("HOST_HOME_PROBE", "/nowhere"),
        ]);
        let lookup = |name: &str| stub.get(name).cloned();
        let seed = interpret_seed_table(&platform_seed_names(), "/scratch/target", &lookup);
        assert!(
            !seed.contains_key("HOST_HOME_PROBE"),
            "unlisted names never seed: {seed:?}"
        );
        assert_eq!(
            seed.get(oxdock_env::os::PATH).map(String::as_str),
            Some("/host/bin")
        );
        for entry in current_platform_seed() {
            if entry.name == oxdock_env::os::PATH || entry.name == oxdock_env::cargo::TARGET_DIR {
                continue;
            }
            assert!(
                !seed.contains_key(entry.name),
                "{} must stay absent when the host lacks it: {seed:?}",
                entry.name
            );
        }
    }

    #[test]
    fn seed_differs_spots_seed_equality() {
        // Hermetic: the live process env is not an oracle here (under
        // Miri there is no host to read at all).
        let stub = stub_host(&[(oxdock_env::os::PATH, "/host/bin")]);
        let lookup = |name: &str| stub.get(name).cloned();
        let seed = interpret_seed_table(&platform_seed_names(), "/scratch/target", &lookup);
        assert!(!seed_differs(&seed, oxdock_env::os::PATH, "/host/bin"));
        assert!(seed_differs(&seed, oxdock_env::os::PATH, "/custom/bin"));
        assert!(seed_differs(&seed, "OXDOCK_CUSTOM_UNSEEDED", "1"));
    }

    #[test]
    fn transport_seed_mirrors_or_omits() {
        // Transports mirror the host names they need; an empty host
        // stages nothing, never a fallback.
        let stub = stub_host(&[(oxdock_env::os::PATH, "/host/bin")]);
        let staged = transport_seed(&|name: &str| stub.get(name).cloned());
        assert_eq!(
            staged.get(oxdock_env::os::PATH).map(String::as_str),
            Some("/host/bin")
        );
        let bare = transport_seed(&|_: &str| None::<String>);
        assert!(bare.is_empty(), "empty host must stage nothing: {bare:?}");
    }

    #[test]
    fn transport_seed_holds_no_scratch() {
        // Transports never bind session scratch: only the engine-owned
        // session row does, and transports do not carry it.
        let stub: HashMap<String, String> = transport_names()
            .iter()
            .map(|name| (name.to_string(), format!("host-{name}")))
            .collect();
        let lookup = |name: &str| stub.get(name).cloned();
        let staged = transport_seed(&lookup);
        assert!(
            !staged.contains_key(oxdock_env::cargo::TARGET_DIR),
            "transports must not bind session scratch: {staged:?}"
        );
        for name in transport_names() {
            assert_eq!(
                staged.get(name).map(String::as_str),
                stub.get(name).map(String::as_str),
                "{name} must mirror the host"
            );
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
                oxdock_env::os::unix::TMPDIR,
            ];
            assert!(
                known.contains(&read.name) || read.name.ends_with("_*"),
                "host read {} is neither const nor pattern",
                read.name
            );
            assert!(
                !read.reader.is_empty(),
                "host read {} names no reader",
                read.name
            );
        }
    }

    #[test]
    fn seed_tables_are_consistent() {
        // The declaration pins everything: names unique, every name a
        // defined `env` constant, platforms from the closed set, the
        // interpreter seeding exactly the declared names against a
        // complete stub host, and both entry views agreeing with the
        // name lists.
        let described = describe_baseline_defaults();
        let mut seen = std::collections::HashSet::new();
        for entry in &described {
            assert!(
                seen.insert(entry.name),
                "declaration duplicates {}",
                entry.name
            );
            assert!(
                ["unix", "windows", "unix, windows"].contains(&entry.platforms),
                "{} declares platforms outside the closed set: {}",
                entry.name,
                entry.platforms
            );
        }
        let defined: std::collections::HashSet<&str> = oxdock_env::os::unix::ALL
            .iter()
            .chain(oxdock_env::os::windows::ALL.iter())
            .chain(oxdock_env::cargo::ALL.iter())
            .copied()
            .chain([oxdock_env::os::PATH])
            .collect();
        for entry in &described {
            assert!(
                defined.contains(entry.name),
                "declared {} is not a defined env constant",
                entry.name
            );
        }
        // A complete stub host resolves every declared name; the stub
        // (not the live process env) keeps the proof hermetic on every
        // machine, including Miri.
        let stub: HashMap<String, String> = described
            .iter()
            .map(|entry| (entry.name.to_string(), format!("host-{}", entry.name)))
            .collect();
        let lookup = |name: &str| stub.get(name).cloned();
        let seed = interpret_seed_table(&platform_seed_names(), "/scratch/target", &lookup);
        for name in platform_seed_names() {
            assert!(seed.contains_key(name), "declared {name} never seeds");
        }
        // Both entry views agree with the name lists they render.
        let mut current: Vec<&str> = current_platform_seed()
            .iter()
            .map(|entry| entry.name)
            .collect();
        let mut names = platform_seed_names();
        current.sort_unstable();
        names.sort_unstable();
        assert_eq!(
            current, names,
            "current-platform entries disagree with seed names"
        );
        for name in platform_seed_names() {
            assert!(
                described.iter().any(|entry| entry.name == name),
                "platform name {name} missing from the contract table"
            );
        }
    }
}
