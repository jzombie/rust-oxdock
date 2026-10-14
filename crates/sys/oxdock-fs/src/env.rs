//! Shared environment variable names, namespaced by owner so generated
//! documentation renders one section per source.
//!
//! Every runtime name the workspace reads or writes lives here, so call
//! sites never repeat a hardcoded string. OS names are keys, not values:
//! process spawn, executable lookup, and home/temp resolution read exactly
//! these spellings. (The `env!("...")` compile-time macros keep their
//! literals: that macro only accepts literal arguments. Type-system
//! spellings such as `TypeTag::Path`'s `"PATH"` label and invented
//! test-only probe names stay at their call sites: the former names a
//! type, the latter nothing.)
//!
//! Rendered reference: `ENV_CONTRACTS.md` at the repository root
//! (generated from the live seed tables by `cargo run -p docs-gen`).

//! Rendered reference: `ENV_CONTRACTS.md` at the repository root
//! (generated from the live seed tables by `cargo run -p docs-gen`).

/// Defines string constants and their module inventory in one place, so
/// contract rendering cannot miss a name: adding a constant without
/// routing it through this macro is a compile error at the use site
/// (`ALL` is the only inventory the renderer reads).
macro_rules! define_names {
    ($($(#[$meta:meta])* $name:ident = $value:expr),* $(,)?) => {
        $( $(#[$meta])* pub const $name: &str = $value; )*
        /// Inventory of every name in this module, for contract rendering.
        pub const ALL: &[&str] = &[ $( $name ),* ];
    };
}

/// Names owned by OxDock itself: engine plumbing, asset pipeline,
/// fixture runners, and test guards.
pub mod oxdock {
    define_names! {
        /// Exact-directory override for the persistent project cache root.
        CACHE_DIR = "OXDOCK_CACHE_DIR",
        /// Application identity override for the persistent project cache root.
        CACHE_APP = "OXDOCK_CACHE_APP",
        /// Fallback application identity when no other stage resolves one.
        FALLBACK_APP_NAME = "oxdock",
        /// Overrides workspace-root discovery; highest-priority input.
        WORKSPACE_ROOT = "OXDOCK_WORKSPACE_ROOT",
        /// Debug flag shared by the asset pipeline.
        EMBED_DEBUG = "OXDOCK_EMBED_DEBUG",
        /// Content-fingerprint salt for the asset pipeline.
        EMBED_FINGERPRINT_SALT = "OXDOCK_EMBED_FINGERPRINT_SALT",
        /// Cache-bust gate for the asset pipeline.
        EMBED_FORCE_REBUILD = "OXDOCK_EMBED_FORCE_REBUILD",
        /// Banner relayed to spawned shells.
        BANNER = "OXDOCK_BANNER",
        /// Forces stdout/stderr inheritance for spawned processes.
        INHERIT_STDOUT = "OXDOCK_INHERIT_STDOUT",
        /// Shell re-exec marker (Windows).
        SHELL_REEXEC = "OXDOCK_SHELL_REEXEC",
        /// Target-dir override for fixture cargo invocations.
        FIXTURE_TARGET_DIR = "OXDOCK_FIXTURE_TARGET_DIR",
        /// Case filter for the `ast_commands` fixture runner.
        AST_CASE = "OXDOCK_AST_CASE",
        /// Coverage-only mode for the `ast_commands` fixture runner.
        AST_ONLY_COVERAGE = "OXDOCK_AST_ONLY_COVERAGE",
        /// Test guards: multi-guard pass token.
        MULTI_GUARD_TEST_PASS = "OXDOCK_MULTI_GUARD_TEST_PASS",
        /// Test guards: multi-guard fail token.
        MULTI_GUARD_TEST_FAIL = "OXDOCK_MULTI_GUARD_TEST_FAIL",
        /// Test guards: unset-token probe.
        GUARD_TEST_TOKEN_UNSET = "OXDOCK_GUARD_TEST_TOKEN_UNSET",
    }
}

/// Names provided by Cargo to build scripts, proc macros, and test processes.
pub mod cargo {
    define_names! {
        /// Cargo-provided manifest directory of the current package.
        MANIFEST_DIR = "CARGO_MANIFEST_DIR",
        /// Cargo-provided package name of the current process.
        PKG_NAME = "CARGO_PKG_NAME",
        /// Cargo-provided primary-package marker.
        PRIMARY_PACKAGE = "CARGO_PRIMARY_PACKAGE",
        /// Cargo target-directory override.
        TARGET_DIR = "CARGO_TARGET_DIR",
        /// Cargo incremental-compilation toggle.
        INCREMENTAL = "CARGO_INCREMENTAL",
        /// Cargo-provided enabled-features list.
        CFG_FEATURE = "CARGO_CFG_FEATURE",
    }

    /// Names in the build-script protocol (set by Cargo for build scripts,
    /// read by asset materialization).
    pub mod build {
        define_names! {
            /// Build output directory for build scripts.
            OUT_DIR = "OUT_DIR",
            /// Cargo-provided build target triple.
            TARGET = "TARGET",
            /// Cargo-provided build profile.
            PROFILE = "PROFILE",
            /// Toolchain override for rustc probing.
            RUSTC = "RUSTC",
        }
    }
}

/// Names owned by the host OS, divided by family. `PATH` is shared:
/// every platform resolves bare executable names through it. Each
/// family module is flat: every name plus the fallback value constants
/// the seeder reads when the host lacks a variable. Name constants
/// route through `define_names!` (with the `ALL` inventory); fallback
/// values are plain constants beside them.
pub mod os {
    /// Executable lookup path, shared by all families.
    pub const PATH: &str = "PATH";

    /// Unix-owned names.
    pub mod unix {
        define_names! {
            /// Shell resolution.
            SHELL = "SHELL",
            /// Home directory.
            HOME = "HOME",
            /// Login name (synthesized by shells from `getpwuid` when absent).
            LOGNAME = "LOGNAME",
            /// User name (synthesized by shells from `getpwuid` when absent).
            USER = "USER",
            /// System temp-dir override honored by fixture runners.
            TMPDIR = "TMPDIR",
            /// Terminal identity. Host value when set, else
            /// [`DEFAULT_TERM`]; explicit script values win.
            TERM = "TERM",
        }

        /// Fallback `PATH`: core system directories, used only when
        /// the host sets no `PATH`.
        pub const DEFAULT_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
        /// Fallback when the host sets no `TERM`.
        pub const DEFAULT_TERM: &str = "dumb";
    }

    /// Windows-owned names.
    pub mod windows {
        define_names! {
            /// Shell resolution.
            COMSPEC = "COMSPEC",
            /// System root anchor (case-insensitive on Windows).
            SYSTEM_ROOT = "SystemRoot",
            /// Windows directory, companion to `SYSTEM_ROOT`.
            WINDIR = "windir",
            /// User profile directory.
            USERPROFILE = "USERPROFILE",
            /// Executable-extension list.
            PATHEXT = "PATHEXT",
            /// Temporary directory (primary).
            TEMP = "TEMP",
            /// Temporary directory (secondary).
            TMP = "TMP",
            /// User name (Windows shells read, not synthesize).
            USERNAME = "USERNAME",
            /// Program install root (toolchain discovery reads it).
            PROGRAM_FILES = "ProgramFiles",
            /// 32-bit program install root (vswhere lives under it).
            PROGRAM_FILES_X86 = "ProgramFiles(x86)",
            /// 64-bit program install root.
            PROGRAM_W6432 = "ProgramW6432",
            /// System drive root.
            SYSTEM_DRIVE = "SystemDrive",
            /// Processor architecture (installer arch detection reads it).
            PROCESSOR_ARCHITECTURE = "PROCESSOR_ARCHITECTURE",
        }

        /// Fallback `PATH`: core system directories, used only when
        /// the host sets no `PATH`.
        pub const DEFAULT_PATH: &str = r"C:\Windows\System32;C:\Windows";
        /// Fallback when the host sets no `PATHEXT`.
        pub const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";
        /// Fallback when the host sets no `SystemRoot`.
        pub const DEFAULT_SYSTEM_ROOT: &str = r"C:\Windows";
    }
}

/// One engine-plumbing name: an `oxdock::` or `cargo::` variable and
/// the rule routing it. Rendered into the contract's plumbing table.
pub struct PlumbingEntry {
    /// Owning module (`"oxdock"`, `"cargo"`, `"cargo::build"`).
    pub owner: &'static str,
    /// Variable name (an inventory constant value).
    pub name: &'static str,
    /// Routing rule for this owner's names.
    pub rule: &'static str,
}

/// Routing rule for `oxdock::` plumbing: explicit call-site reads from
/// the host process, never ambient inheritance into scripts or children.
pub const OXDOCK_PLUMBING_RULE: &str =
    "engine plumbing: read at explicit call sites; never inherited";
/// Routing rule for `cargo::` names: provided by Cargo to build scripts,
/// proc macros, and test processes; relayed into scripts only through
/// sanctioned seeds or explicit routing.
pub const CARGO_PLUMBING_RULE: &str =
    "Cargo-provided: relayed only through sanctioned seeds or explicit routing";

/// Engine-plumbing inventory: every `oxdock::` / `cargo::` name with
/// its routing rule. Built from the module `ALL` arrays the
/// `define_names!` macro emits, so a new constant appears here with
/// no extra registration.
pub fn plumbing() -> Vec<PlumbingEntry> {
    let mut out = Vec::new();
    for name in oxdock::ALL {
        out.push(PlumbingEntry {
            owner: "oxdock",
            name,
            rule: OXDOCK_PLUMBING_RULE,
        });
    }
    for name in cargo::ALL {
        out.push(PlumbingEntry {
            owner: "cargo",
            name,
            rule: CARGO_PLUMBING_RULE,
        });
    }
    for name in cargo::build::ALL {
        out.push(PlumbingEntry {
            owner: "cargo::build",
            name,
            rule: CARGO_PLUMBING_RULE,
        });
    }
    out
}

/// Plumbing-table chapter preamble, sourced here with the inventory.
pub const PLUMBING_INTRO_MD: &str = indoc::indoc! {"
    ### Engine plumbing

    Names the engine and Cargo provide to the host process itself.
    None of these inject into children; scripts see them only through
    explicit routing.
"};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventories_cover_every_defined_name() {
        // The macro emits ALL alongside the constants, so inventories
        // are complete by construction. This pins no duplicates and
        // names the load-bearing entries, so a hand-edited inventory
        // (bypassing the macro) fails loudly.
        for (label, all) in [
            ("oxdock", oxdock::ALL),
            ("cargo", cargo::ALL),
            ("cargo::build", cargo::build::ALL),
            ("os::unix", os::unix::ALL),
            ("os::windows", os::windows::ALL),
        ] {
            let mut seen = std::collections::HashSet::new();
            for name in all {
                assert!(seen.insert(name), "{label} inventory duplicates {name}");
            }
        }
        assert!(
            oxdock::ALL.contains(&oxdock::EMBED_FORCE_REBUILD),
            "plumbing must name EMBED_FORCE_REBUILD"
        );
        assert!(
            oxdock::ALL.contains(&oxdock::WORKSPACE_ROOT),
            "plumbing must name WORKSPACE_ROOT"
        );
        assert!(
            cargo::ALL.contains(&cargo::MANIFEST_DIR),
            "plumbing must name MANIFEST_DIR"
        );
        assert!(
            os::unix::ALL.contains(&os::unix::TERM),
            "unix inventory must name TERM"
        );
        assert!(
            os::windows::ALL.contains(&os::windows::WINDIR),
            "windows inventory must name WINDIR"
        );
        assert_eq!(
            plumbing().len(),
            oxdock::ALL.len() + cargo::ALL.len() + cargo::build::ALL.len()
        );
    }
}
