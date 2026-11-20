//! Home-scope function resolution (#319): a callee body resolves
//! against its defining file's scope, so an `IMPORT` names only what
//! the importing file itself calls. Harvesting `STAGE` alone still
//! runs `STAGE`'s sibling calls (`OUT_DIR`, `WORK_ROOT`); private
//! helpers resolve from home too but never leak through the alias.

use indoc::indoc;
use oxdock_core::Engine;
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver};

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn write_lib(root: &GuardedPath, rel: &str, contents: &str) {
    let resolver = PathResolver::new(root.as_path(), root.as_path()).unwrap();
    let path = root.join(rel).unwrap();
    resolver.write_file(&path, contents.as_bytes()).unwrap();
}

fn shared_lib() -> &'static str {
    indoc! {r#"
        IMPORT [STD]
        EXPORT [FUNC STAGE, FUNC OUT_DIR, FUNC WORK_ROOT]
        FUNC WORK_ROOT(): STRING {
            WORKSPACE SYSTEM
            RETURN "/work"
        }
        FUNC OUT_DIR(): STRING {
            WORKSPACE SYSTEM
            LET $w: STRING|PATH = WORK_ROOT()
            RETURN STD::PATH_JOIN($w, ".oxdock-codebook")
        }
        FUNC STAGE(): STRING {
            WORKSPACE SYSTEM
            LET $dir: STRING = OUT_DIR()
            RETURN $dir
        }
    "#}
}

#[test]
fn home_scope_harvested_func_resolves_siblings() {
    // The issue repro: the entry harvests only `STAGE`, and `STAGE`'s
    // calls to `OUT_DIR` (transitively `WORK_ROOT`) resolve against
    // the defining file instead of failing as `SCRIPT::OUT_DIR`.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(&root, "shared.oxfile", shared_lib());
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, [FUNC STAGE FROM "shared.oxfile"]]
                WORKSPACE SYSTEM
                LET $snap: STRING = STAGE()
                ECHO $snap
            "#},
        )
        .expect("harvested STAGE resolves home siblings");
    assert_eq!(
        out.bindings.get("snap").and_then(|v| v.as_str()),
        Some(oxdock_fs::join_native("/work", ".oxdock-codebook").as_str())
    );
}

#[test]
fn home_scope_file_import_resolves_siblings() {
    // Same closure through a full file import: `LIB::STAGE` reaches
    // its siblings without the entry naming them.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(&root, "shared.oxfile", shared_lib());
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "shared.oxfile" AS LIB]
                WORKSPACE SYSTEM
                LET $snap: STRING = LIB::STAGE()
                ECHO $snap
            "#},
        )
        .expect("file-imported STAGE resolves home siblings");
    assert_eq!(
        out.bindings.get("snap").and_then(|v| v.as_str()),
        Some(oxdock_fs::join_native("/work", ".oxdock-codebook").as_str())
    );
}

#[test]
fn home_scope_harvest_with_alias_resolves_siblings() {
    // Harvested-under-alias spells the same home scope through the
    // canonical key instead of the bare one.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(&root, "shared.oxfile", shared_lib());
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, [FUNC STAGE FROM "shared.oxfile" AS LIB2]]
                WORKSPACE SYSTEM
                LET $snap: STRING = LIB2::STAGE()
                ECHO $snap
            "#},
        )
        .expect("aliased harvest resolves home siblings");
    assert_eq!(
        out.bindings.get("snap").and_then(|v| v.as_str()),
        Some(oxdock_fs::join_native("/work", ".oxdock-codebook").as_str())
    );
}

#[test]
fn home_scope_unknown_alias_call_suggests_exporting_file() {
    // #319 diagnostic: `LIB2::OUT_DIR` names the file that exports
    // the name plus the working spelling, instead of dead-ending at
    // `unknown function`.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(&root, "shared.oxfile", shared_lib());
    write_lib(
        &root,
        "other.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC Q]
            FUNC Q(): STRING {
                WORKSPACE SYSTEM
                RETURN "q"
            }
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "shared.oxfile" AS LIB, "other.oxfile" AS LIB2]
                WORKSPACE SYSTEM
                LET $x: STRING = LIB2::OUT_DIR()
            "#},
        )
        .expect_err("wrong-alias call must fail");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("unknown function `LIB2::OUT_DIR`"),
        "{rendered}"
    );
    assert!(
        rendered.contains("did you mean `LIB::OUT_DIR`?") && rendered.contains("exports `OUT_DIR`"),
        "{rendered}"
    );
}

#[test]
fn home_scope_unknown_call_names_unimported_exporter() {
    // Nested-only exports are not alias-imported by the entry, so
    // the hint suggests the harvest spelling with the file it names.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "nested.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC NESTED]
            FUNC NESTED(): STRING {
                WORKSPACE SYSTEM
                RETURN "nested"
            }
        "#},
    );
    write_lib(
        &root,
        "mid.oxfile",
        indoc! {r#"
            IMPORT [STD, [FUNC NESTED FROM "nested.oxfile"]]
            EXPORT [FUNC STAGE]
            FUNC STAGE(): STRING {
                WORKSPACE SYSTEM
                RETURN NESTED()
            }
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "mid.oxfile" AS LIB]
                WORKSPACE SYSTEM
                LET $x: STRING = LIB::NESTED()
            "#},
        )
        .expect_err("nested-only call must fail");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("unknown function `LIB::NESTED`"),
        "{rendered}"
    );
    assert!(
        rendered.contains("exports `NESTED`") && rendered.contains("FROM"),
        "{rendered}"
    );
}

#[test]
fn home_scope_unknown_call_without_exporter_stays_bare() {
    // Nothing exports the name: the error keeps its bare form with
    // no speculative suffix.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(&root, "shared.oxfile", shared_lib());
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "shared.oxfile" AS LIB]
                WORKSPACE SYSTEM
                LET $x: STRING = LIB::NOPE()
            "#},
        )
        .expect_err("unexported call must fail");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("unknown function `LIB::NOPE`"),
        "{rendered}"
    );
    assert!(!rendered.contains("exports"), "{rendered}");
}

#[test]
fn home_scope_private_helper_resolves_but_stays_private() {
    // A private helper (never exported) still serves home bodies,
    // but the alias never exposes it: `LIB::HELPER` fails while
    // `LIB::STAGE` (which calls it) succeeds.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "shared2.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC STAGE]
            FUNC HELPER(): STRING {
                WORKSPACE SYSTEM
                RETURN "help"
            }
            FUNC STAGE(): STRING {
                WORKSPACE SYSTEM
                LET $h: STRING = HELPER()
                RETURN $h
            }
        "#},
    );
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "shared2.oxfile" AS LIB]
                WORKSPACE SYSTEM
                LET $s: STRING = LIB::STAGE()
            "#},
        )
        .expect("private helper serves home caller");
    assert_eq!(out.bindings.get("s").and_then(|v| v.as_str()), Some("help"));
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "shared2.oxfile" AS LIB]
                WORKSPACE SYSTEM
                LET $s: STRING = LIB::HELPER()
            "#},
        )
        .expect_err("private helper must not leak through the alias");
    assert!(
        format!("{err:#}").contains("unknown function `LIB::HELPER`"),
        "{err:#}"
    );
}

#[test]
fn home_scope_private_return_flows_into_union_param() {
    // The SEV-3 proof: a private sibling's declared `-> STRING`
    // return flows into a union-typed builtin param through a home
    // call. Before the home index this inferred `ANY` and failed
    // static check with the untyped-ANY gate.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "shared3.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC STAGE]
            FUNC LEAF(): STRING {
                WORKSPACE SYSTEM
                RETURN "leaf"
            }
            FUNC STAGE(): STRING {
                WORKSPACE SYSTEM
                LET $d: STRING = STD::PATH_JOIN(LEAF(), "child")
                RETURN $d
            }
        "#},
    );
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "shared3.oxfile" AS LIB]
                WORKSPACE SYSTEM
                LET $s: STRING = LIB::STAGE()
            "#},
        )
        .expect("private return flows into union param");
    assert!(
        out.bindings
            .get("s")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.ends_with("child")),
        "joined path carries the private return: {:?}",
        out.bindings.get("s")
    );
}

#[test]
fn home_scope_guarded_sibling_resolves_live_variant() {
    // A helper defined under a platform guard resolves at home only
    // where its defining step would run: the lookup evaluates the
    // step guard exactly like execution. A caller on a non-matching
    // host fails with unknown function, mirroring that the `FUNC`
    // step never executed there (same as any guarded definition).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "shared4.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC STAGE]
            [family:windows] FUNC WHICH(): STRING {
                WORKSPACE SYSTEM
                RETURN "win"
            }
            FUNC STAGE(): STRING {
                WORKSPACE SYSTEM
                RETURN WHICH()
            }
        "#},
    );
    let script = indoc! {r#"
        IMPORT [STD, "shared4.oxfile" AS LIB]
        WORKSPACE SYSTEM
        LET $w: STRING = LIB::STAGE()
    "#};
    if std::env::consts::FAMILY == "windows" {
        let out = Engine::new()
            .run_script(&root, script)
            .expect("live-guarded home sibling resolves");
        assert_eq!(out.bindings.get("w").and_then(|v| v.as_str()), Some("win"));
    } else {
        let err = Engine::new()
            .run_script(&root, script)
            .expect_err("dead-guarded home sibling must not resolve");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("unknown function") && rendered.contains("SCRIPT::WHICH"),
            "{rendered}"
        );
    }
}
