//! End-to-end toolchain plugin tests with seeded fake toolchains.
//!
//! No test touches the network: fake `cargo`/`rustc` scripts are seeded
//! into the pinned toolchain cache before any `TOOLCHAIN_*` call, so the
//! download path never triggers. `OXDOCK_CACHE_DIR` is pinned per test
//! under a process-wide lock because process env is global. Process
//! spawning tests run on Unix with `sh` fakes; Windows covers validation,
//! bundling, and ordering without spawning. Everything skips under Miri
//! with a reason: OS tempdirs plus process spawn are blocked there.

use std::sync::{Mutex, MutexGuard};

use indoc::indoc;
use oxdock_core::{Engine, EngineOutput};
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver};

/// Serializes `OXDOCK_CACHE_DIR` pinning: process env is global, so
/// parallel tests must not race each other's overrides.
static CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());

struct PinnedCache {
    _lock: MutexGuard<'static, ()>,
    _temp: GuardedTempDir,
    prev: Option<String>,
    cache_root: GuardedPath,
}

impl PinnedCache {
    fn pin() -> Self {
        let lock = CACHE_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = GuardedPath::tempdir().expect("tempdir");
        let cache_root = temp
            .as_guarded_path()
            .join("cache")
            .expect("cache join");
        let prev = std::env::var("OXDOCK_CACHE_DIR").ok();
        // SAFETY: serialized by `CACHE_ENV_LOCK`, restored on drop.
        unsafe {
            std::env::set_var("OXDOCK_CACHE_DIR", cache_root.display());
            std::env::remove_var("OXDOCK_TOOLCHAIN_SHA256");
        }
        Self {
            _lock: lock,
            _temp: temp,
            prev,
            cache_root,
        }
    }

    fn cache_root(&self) -> GuardedPath {
        self.cache_root.clone()
    }
}

impl Drop for PinnedCache {
    fn drop(&mut self) {
        unsafe {
            match &self.prev {
                Some(value) => std::env::set_var("OXDOCK_CACHE_DIR", value),
                None => std::env::remove_var("OXDOCK_CACHE_DIR"),
            }
        }
    }
}

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn run_script(root: &GuardedPath, script: &str) -> anyhow::Result<EngineOutput> {
    let mut engine = Engine::new();
    engine.register_module(oxdock_toolchain_plugin::module());
    engine.run_script(root, script)
}

fn resolver_for(root: &GuardedPath) -> PathResolver {
    PathResolver::new(root.as_path(), root.as_path()).expect("resolver")
}

/// Execute a built binary and return its trimmed stdout. Uses the
/// `oxdock-process` builder like production code, never raw spawn.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn run_binary(binary: &GuardedPath) -> String {
    let mut cmd = oxdock_process::CommandBuilder::new(binary.as_path());
    let out = cmd.output().expect("run built binary");
    assert!(out.success(), "built binary exits zero");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Seed fake `cargo`/`rustc` executables into the pinned toolchain
/// cache for the host triple. The fakes never compile: `cargo` mimics
/// `cargo build` argument handling well enough to prove orchestration,
/// `rustc --version` reports a canned version for fingerprints.
fn seed_fake_toolchain(cache: &PinnedCache) -> String {
    let triple = oxdock_toolchain_plugin::host_triple().expect("host triple");
    let cargo_name = oxdock_toolchain_plugin::host_exe_name("cargo");
    let rustc_name = oxdock_toolchain_plugin::host_exe_name("rustc");
    let cache_root = cache.cache_root();
    let resolver = resolver_for(&cache_root);
    let bin = cache_root
        .join(&format!("toolchain/dist/{triple}/bin"))
        .expect("bin join");
    resolver.create_dir_all(&bin).expect("create bin");
    #[cfg(unix)]
    {
        let cargo = bin.join(&cargo_name).expect("cargo join");
        let script = indoc! {r#"#!/bin/sh
            # Fake cargo: scan for --target-dir, --target, --release and
            # drop a marker binary named after the test package.
            target_dir=""
            target=""
            profile="dev"
            prev=""
            for arg in "$@"; do
              case "$prev" in
                --target-dir) target_dir="$arg";;
                --target) target="$arg";;
              esac
              case "$arg" in
                --release) profile="release";;
              esac
              prev="$arg"
            done
            out="$target_dir/$target/$profile"
            mkdir -p "$out"
            printf '#!/bin/sh\nprintf "demo-pkg 0.1.0\\n"\n' > "$out/demo-pkg"
            chmod +x "$out/demo-pkg"
            printf '%s' "${RUSTC:-}" > "$target_dir/rustc.env"
            printf '%s' "${CARGO_ENCODED_RUSTFLAGS:-}" > "$target_dir/rustflags.env"
            printf '%s' "${CARGO_HOME:-}" > "$target_dir/cargohome.env"
        "#};
        resolver
            .write_file(&cargo, script.as_bytes())
            .expect("write cargo");
        resolver
            .set_permissions_mode_unix(&cargo, 0o755)
            .expect("chmod cargo");
        let rustc = bin.join(&rustc_name).expect("rustc join");
        resolver
            .write_file(&rustc, b"#!/bin/sh\necho 'rustc 1.90.0 (fake toolchain)'\n")
            .expect("write rustc");
        resolver
            .set_permissions_mode_unix(&rustc, 0o755)
            .expect("chmod rustc");
    }
    #[cfg(windows)]
    {
        let cargo = bin.join(&cargo_name).expect("cargo join");
        resolver
            .write_file(&cargo, b"fake cargo placeholder")
            .expect("write cargo");
        let rustc = bin.join(&rustc_name).expect("rustc join");
        resolver
            .write_file(&rustc, b"fake rustc placeholder")
            .expect("write rustc");
    }
    triple
}

/// Stage a minimal source package under the workspace root.
fn stage_demo_pkg(root: &GuardedPath) {
    let resolver = resolver_for(root);
    let pkg = root.join("demo-src").expect("pkg join");
    resolver.create_dir_all(&pkg).expect("create pkg");
    let manifest = pkg.join("Cargo.toml").expect("manifest join");
    resolver
        .write_file(
            &manifest,
            b"[package]\nname = \"demo-pkg\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
    let src = pkg.join("src").expect("src join");
    resolver.create_dir_all(&src).expect("create src");
    let main = src.join("main.rs").expect("main join");
    resolver
        .write_file(&main, b"fn main() { println!(\"demo-pkg 0.1.0\"); }\n")
        .expect("write main");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs OS tempdirs plus pinned cache env; blocked under Miri isolation"
)]
fn ensure_returns_cached_paths() {
    let cache = PinnedCache::pin();
    let triple = seed_fake_toolchain(&cache);
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = format!(
        indoc! {r#"
            IMPORT [STD, TOOLCHAIN]
            LET $t: MAP = TOOLCHAIN_ENSURE("")
            ASSERT_EQ $t.triple "{triple}"
        "#},
        triple = triple
    );
    run_script(&root, &script).expect("ensure runs");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs OS tempdirs plus pinned cache env; blocked under Miri isolation"
)]
fn unknown_triple_is_a_hard_error() {
    let _cache = PinnedCache::pin();
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, TOOLCHAIN]
        LET $t: MAP = TOOLCHAIN_ENSURE("mips-unknown-linux-gnu")
    "#};
    let err = run_script(&root, script).expect_err("unknown triple must fail");
    assert!(err.to_string().contains("unknown target triple"), "{err:#}");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs OS tempdirs plus pinned cache env; blocked under Miri isolation"
)]
fn fetch_source_bundles_into_cache() {
    let cache = PinnedCache::pin();
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    stage_demo_pkg(&root);
    let script = indoc! {r#"
        IMPORT [STD, TOOLCHAIN]
        LET $s: MAP = TOOLCHAIN_FETCH_SOURCE("demo-src")
    "#};
    run_script(&root, script).expect("fetch source runs");
    let cache_root = cache.cache_root();
    let staged_src = cache_root.join("toolchain/src").expect("src join");
    let resolver = resolver_for(&cache_root);
    let entries = resolver
        .read_dir_entries(&staged_src)
        .expect("staged src lists");
    assert_eq!(entries.len(), 1, "one content-addressed stage");
    let only = entries
        .first()
        .expect("one stage")
        .path()
        .to_string_lossy()
        .into_owned();
    assert!(only.contains("local-"), "stage is content addressed, got {only:?}");
    // The local tree gains nothing outside the package we staged.
    let local = resolver_for(&root);
    let top = local.read_dir_entries(&root).expect("local lists");
    let names: Vec<String> = top
        .iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"demo-src".to_string()),
        "staged package stays, got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("local-")),
        "no stage leaks into the local tree, got {names:?}"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs OS tempdirs plus pinned cache env; blocked under Miri isolation"
)]
fn build_rejects_manifest_outside_cache() {
    let _cache = PinnedCache::pin();
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    stage_demo_pkg(&root);
    let script = indoc! {r#"
        IMPORT [STD, TOOLCHAIN]
        LET $b: MAP = TOOLCHAIN_BUILD("", "demo-src", [], {})
    "#};
    let err = run_script(&root, script).expect_err("outside manifest must fail");
    assert!(
        err.to_string().contains("inside the toolchain cache"),
        "{err:#}"
    );
}

#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "needs OS tempdirs plus process spawn; blocked under Miri isolation"
)]
fn build_release_then_no_rebuild_then_dev() {
    let cache = PinnedCache::pin();
    let triple = seed_fake_toolchain(&cache);
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    stage_demo_pkg(&root);
    let script = format!(
        indoc! {r#"
            IMPORT [STD, TOOLCHAIN]
            LET $s: MAP = TOOLCHAIN_FETCH_SOURCE("demo-src")
            LET $b: MAP = TOOLCHAIN_BUILD("", $s.manifest_dir, [], {{}})
            ASSERT_EQ $b.profile "release"
            ASSERT_EQ $b.releasable true
            ASSERT_EQ $b.metadata.triple "{triple}"
        "#},
        triple = triple
    );
    run_script(&root, &script).expect("release build runs");
    let cache_root = cache.cache_root();
    let resolver = resolver_for(&cache_root);
    let binary = cache_root
        .join(&format!("toolchain/target/{triple}/release/demo-pkg"))
        .expect("binary join");
    assert!(resolver.exists(&binary), "release binary lands in cache");
    // The artifact is runnable: execute it and check its version line.
    // A marker file would pass an existence check; execution proves the
    // handoff (path plus executable bit plus spawnability).
    let version = run_binary(&binary);
    assert_eq!(version, "demo-pkg 0.1.0", "built binary runs");
    // The build reaches only cached inputs: RUSTC names the cached
    // rustc and CARGO_HOME stays inside the cache group.
    let target_base = cache_root
        .join("toolchain/target")
        .expect("target base join");
    let used_rustc = resolver
        .read_to_string(
            &target_base
                .join("rustc.env")
                .expect("rustc env join"),
        )
        .expect("read rustc env");
    assert!(
        used_rustc.contains("toolchain/dist"),
        "cargo builds through the cached rustc, got {used_rustc:?}"
    );
    let cargo_home = resolver
        .read_to_string(
            &target_base
                .join("cargohome.env")
                .expect("cargo home join"),
        )
        .expect("read cargo home");
    assert!(
        cargo_home.contains("toolchain"),
        "cargo registries stay inside the cache, got {cargo_home:?}"
    );
    // Fresh rebuild short-circuits: poison the binary and rebuild.
    resolver
        .write_file(&binary, b"sentinel")
        .expect("poison binary");
    let script = indoc! {r#"
        IMPORT [STD, TOOLCHAIN]
        LET $s: MAP = TOOLCHAIN_FETCH_SOURCE("demo-src")
        LET $b: MAP = TOOLCHAIN_BUILD("", $s.manifest_dir, [], {})
        ASSERT_EQ $b.profile "release"
    "#};
    run_script(&root, script).expect("fresh rebuild runs");
    let kept = resolver.read_file(&binary).expect("read binary");
    assert_eq!(kept, b"sentinel", "fresh fingerprint skips the build");
    // Dev builds live apart and never read releasable.
    let script = indoc! {r#"
        IMPORT [STD, TOOLCHAIN]
        LET $s: MAP = TOOLCHAIN_FETCH_SOURCE("demo-src")
        LET $d: MAP = TOOLCHAIN_BUILD("", $s.manifest_dir, [], {profile: "dev"})
        ASSERT_EQ $d.profile "dev"
        ASSERT_EQ $d.releasable false
    "#};
    run_script(&root, script).expect("dev build runs");
    let dev_binary = cache_root
        .join(&format!("toolchain/target/{triple}/dev/demo-pkg"))
        .expect("dev join");
    assert!(resolver.exists(&dev_binary), "dev binary lands apart");
    assert_eq!(run_binary(&dev_binary), "demo-pkg 0.1.0", "dev binary runs");
}

#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "needs OS tempdirs plus process spawn; blocked under Miri isolation"
)]
fn build_real_project_with_host_toolchain() {
    // No fakes: the toolchain running these tests stands in for a
    // provisioned toolchain (seeded by symlink, download bypassed),
    // and a real dependency-free package compiles through the DSL.
    // This proves the orchestration drives a genuine compiler to a
    // runnable binary. Provisioning downloads stay unverified here.
    let cache = PinnedCache::pin();
    let triple = oxdock_toolchain_plugin::host_triple().expect("host triple");
    let cargo_bin = std::env::var("CARGO").expect("cargo runs these tests");
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    let rustc_bin = std::path::Path::new(&cargo_bin)
        .parent()
        .expect("cargo has a parent dir")
        .join("rustc")
        .to_string_lossy()
        .into_owned();
    let cache_root = cache.cache_root();
    let resolver = resolver_for(&cache_root);
    let bin = cache_root
        .join(&format!("toolchain/dist/{triple}/bin"))
        .expect("bin join");
    resolver.create_dir_all(&bin).expect("create bin");
    // Copies, never symlinks: guard joins canonicalize through links,
    // so a linked binary would read as escaped. This mirrors
    // `wire_binaries`, which always publishes regular files.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    {
        use oxdock_fs::UnguardedPath;
        for (name, source) in [("cargo", &cargo_bin), ("rustc", &rustc_bin)] {
            let dest = bin.join(name).expect("tool join");
            let src = UnguardedPath::external(std::path::PathBuf::from(source));
            resolver
                .copy_file_from_unguarded(&src, &dest)
                .expect("seed tool");
        }
    }
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    stage_demo_pkg(&root);
    let script = indoc! {r#"
        IMPORT [STD, TOOLCHAIN]
        LET $s: MAP = TOOLCHAIN_FETCH_SOURCE("demo-src")
        LET $b: MAP = TOOLCHAIN_BUILD("", $s.manifest_dir, [], {})
        ASSERT_EQ $b.profile "release"
        ASSERT_EQ $b.releasable true
    "#};
    run_script(&root, script).expect("real build runs");
    let binary = cache_root
        .join(&format!("toolchain/target/{triple}/release/demo-pkg"))
        .expect("binary join");
    assert!(resolver.exists(&binary), "real binary lands in cache");
    assert_eq!(run_binary(&binary), "demo-pkg 0.1.0", "real binary runs");
}

#[test]
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs OS tempdirs plus pinned cache env; blocked under Miri isolation"
)]
fn build_orders_provision_before_manifest_checks() {
    // Windows cannot spawn the `sh` fake, so this asserts ordering
    // without spawning: a missing toolchain plus an outside-cache
    // manifest still fails on the manifest guard, proving validation
    // runs in the same order on every platform.
    let _cache = PinnedCache::pin();
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    stage_demo_pkg(&root);
    let script = indoc! {r#"
        IMPORT [STD, TOOLCHAIN]
        LET $b: MAP = TOOLCHAIN_BUILD("", "demo-src", [], {})
    "#};
    let err = run_script(&root, script).expect_err("outside manifest must fail");
    assert!(
        err.to_string().contains("inside the toolchain cache"),
        "{err:#}"
    );
}
