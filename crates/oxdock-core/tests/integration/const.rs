//! `CONST` bindings: deeply immutable values that read exactly like
//! `LET` but refuse every write. `SET`, `LIST_APPEND`, `MAP_SET`,
//! and `MERGE_MAPS` on a `CONST` (or an alias of one) fail with one
//! message shape; redeclaring across `LET`/`CONST` fails in the
//! shared namespace. Only `CONST` crosses the module boundary, read
//! as `$ALIAS::NAME`.

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

#[test]
fn const_reads_every_value_type() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Every declaration shape LET accepts, CONST accepts too.
        IMPORT [STD]
        CONST $s: STRING = "hi"
        CONST $i: INT = 7
        CONST $f: FLOAT = 2.5
        CONST $b: BOOL = true
        CONST $l: LIST<STRING> = ["a", "b"]
        CONST $m: MAP<ANY> = {x: 1, nested: {y: [1, 2]}}
        CONST $d: DURATION = 10s
        ASSERT_EQ $s "hi"
        ASSERT_EQ $i 7
        ASSERT_EQ $f 2.5
        ASSERT_EQ $b true
        ASSERT_CONTAINS $l "a"
        ASSERT_EQ $m.x 1
        ASSERT_EQ $m.nested.y.1 2
    "#};
    Engine::new()
        .run_script(&root, script)
        .expect("const reads every type");
}

#[test]
fn const_set_fails_with_one_message() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $n: INT = 1
                $n = 2
            "#},
        )
        .expect_err("SET on a CONST must fail");
    assert!(err.to_string().contains("cannot mutate CONST $n"), "{err}");
}

#[test]
fn const_list_append_fails_with_one_message() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $items: LIST<STRING> = ["a"]
                LIST_APPEND $items "b"
            "#},
        )
        .expect_err("LIST_APPEND on a CONST must fail");
    assert!(
        err.to_string().contains("cannot mutate CONST $items"),
        "{err}"
    );
}

#[test]
fn const_functional_rebuilds_fail_with_one_message() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                CONST $m: MAP<ANY> = {a: 1}
                LET $n: MAP<ANY> = MAP_SET($m, "b", 2)
            "#},
        )
        .expect_err("MAP_SET on a CONST must fail");
    assert!(
        format!("{err:#}").contains("cannot mutate CONST"),
        "{err:#}"
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                CONST $m: MAP<ANY> = {a: 1}
                LET $n: MAP<ANY> = MERGE_MAPS([$m], "overwrite")
            "#},
        )
        .expect_err("MERGE_MAPS on a CONST must fail");
    assert!(
        format!("{err:#}").contains("cannot mutate CONST"),
        "{err:#}"
    );
}

#[test]
fn const_freeze_is_deep_and_detached() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Freezing detaches: the LET source stays mutable while the
        # CONST copy and everything reachable through it freezes.
        IMPORT [STD]
        LET $inner: LIST<STRING> = ["a"]
        CONST $c: LIST<ANY> = [$inner]
        LIST_APPEND $inner "b"
        ASSERT_CONTAINS $inner "b"
        ASSERT_EQ $c.0.0 "a"
    "#};
    Engine::new()
        .run_script(&root, script)
        .expect("freeze detaches from the source");
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $c: LIST<STRING> = ["a"]
                LET $alias: LIST<STRING> = $c
                LIST_APPEND $alias "b"
            "#},
        )
        .expect_err("alias of a frozen buffer must fail");
    assert!(
        format!("{err:#}").contains("cannot mutate CONST"),
        "{err:#}"
    );
}

#[test]
fn const_and_let_share_one_namespace() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for (first, second) in [
        ("CONST $a: INT = 1", "CONST $a: INT = 2"),
        ("LET $a: INT = 1", "CONST $a: INT = 2"),
        ("CONST $a: INT = 1", "LET $a: INT = 2"),
    ] {
        let err = Engine::new()
            .run_script(&root, &format!("{first}\n{second}\n"))
            .expect_err("redeclaring across LET/CONST must fail");
        assert!(err.to_string().contains("redeclaration error"), "{err}");
        assert!(err.to_string().contains("CONST"), "{err}");
    }
}

#[test]
fn const_inner_let_shadow_stays_mutable() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Shadowing is lexical: an inner LET with the same name
        # mutates the inner binding, and the outer CONST survives.
        CONST $x: INT = 1
        [bool:true] {
            LET $x: INT = 2
            $x = 3
            WRITE inner.txt "{{ $x }}"
        }
        LET $back: STRING = READ inner.txt
        ASSERT_EQ $back "3"
        ASSERT_EQ $x 1
    "#};
    Engine::new()
        .run_script(&root, script)
        .expect("inner LET shadows outer CONST");
}

#[test]
fn const_capture_async_and_await_forms_bind() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        # Every LET right-hand side has a CONST twin.
        CONST $out: STRING = ECHO hi
        ASSERT_EQ $out "hi\n"
        CONST $t: HANDLE<INT> = ASYNC { RETURN 41 }
        CONST $n: INT = AWAIT $t
        ASSERT_EQ $n 41
        CONST $blk: STRING = {
            RETURN "made"
        }
        ASSERT_EQ $blk "made"
    "#};
    Engine::new()
        .run_script(&root, script)
        .expect("CONST capture/async/await/block forms bind");
}

#[test]
fn const_exports_read_through_alias() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            CONST $VERSION: STRING = "1.2.3"
            CONST $LIMITS: MAP<ANY> = {max: 10}
            LET $secret: STRING = "nope"
            EXPORT CONST VERSION
            EXPORT [CONST LIMITS]
        "#},
    );
    let script = indoc! {r#"
        # Exported CONSTs read as $ALIAS::NAME: bare, in math,
        # and inside templates. Nested access copies to a local
        # first (key paths never ride qualified names).
        IMPORT ["lib.oxdock" AS LIB]
        LET $v: STRING = $LIB::VERSION
        ASSERT_EQ $v "1.2.3"
        LET $lim: MAP<ANY> = $LIB::LIMITS
        ASSERT_EQ $lim.max 10
        LET $w: STRING = "v{{ $LIB::VERSION }}"
        ASSERT_EQ $w "v1.2.3"
        ECHO "export-ok {{ $LIB::VERSION }}"
    "#};
    Engine::new()
        .run_script(&root, script)
        .expect("exported CONST reads through its alias");
}

#[test]
fn const_let_never_exports() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            LET $secret: STRING = "nope"
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                LET $s: STRING = $LIB::secret
            "#},
        )
        .expect_err("LET bindings never export");
    assert!(err.to_string().contains("cannot be exported"), "{err}");
}

#[test]
fn const_export_of_let_fails_at_parse() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            LET $NAME: STRING = "x"
            EXPORT CONST NAME
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                ECHO hi
            "#},
        )
        .expect_err("EXPORT CONST over a LET must fail");
    assert!(
        err.to_string()
            .contains("does not match any top-level CONST"),
        "{err}"
    );
}

#[test]
fn const_qualified_reexport_fails_at_parse() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            CONST $A: INT = 1
            EXPORT CONST LIB::A
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                ECHO hi
            "#},
        )
        .expect_err("qualified CONST re-exports are not supported");
    assert!(
        err.to_string().contains("must name a bare top-level CONST"),
        "{err}"
    );
}

#[test]
fn const_keypath_on_alias_fails_with_copy_hint() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            CONST $LIMITS: MAP<ANY> = {max: 10}
            EXPORT CONST LIMITS
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                ASSERT_EQ $LIB::LIMITS.max 10
            "#},
        )
        .expect_err("key paths on $ALIAS::NAME must fail");
    assert!(err.to_string().contains("copy to a local"), "{err}");
}

#[test]
fn const_nested_freeze_refuses_inner_mutation() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // Freezing is deep: inner maps and lists freeze with the outer
    // binding, so deriving through a copied inner handle fails.
    for script in [
        indoc! {r#"
            IMPORT [STD]
            CONST $m: MAP<ANY> = {nested: {x: 1}}
            LET $inner: MAP<ANY> = $m.nested
            LET $z: MAP<ANY> = MAP_SET($inner, "y", 2)
        "#},
        indoc! {r#"
            IMPORT [STD]
            CONST $m: MAP<ANY> = {items: ["a"]}
            LET $items: LIST<STRING> = $m.items
            LIST_APPEND $items "b"
        "#},
        indoc! {r#"
            IMPORT [STD]
            CONST $c: LIST<ANY> = [{x: 1}]
            LET $first: MAP<ANY> = $c.0
            LET $z: MAP<ANY> = MERGE_MAPS([$first], "overwrite")
        "#},
    ] {
        let err = Engine::new()
            .run_script(&root, script)
            .expect_err("nested CONST contents must refuse mutation");
        assert!(
            format!("{err:#}").contains("cannot mutate CONST"),
            "{err:#}"
        );
    }
}

#[test]
fn const_imported_value_stays_immutable() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            CONST $VERSION: STRING = "1.2.3"
            CONST $LIMITS: MAP<ANY> = {max: 10}
            CONST $ITEMS: LIST<STRING> = ["a"]
            EXPORT CONST VERSION
            EXPORT [CONST LIMITS, CONST ITEMS]
        "#},
    );
    // Scalar copies rebind like any LET; frozen-ness only guards
    // container buffers, never rebinding.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                LET $v: STRING = $LIB::VERSION
                $v = "other"
                ASSERT_EQ $v "other"
            "#},
        )
        .expect("scalar copy of an imported CONST rebinds");
    // Container copies keep the frozen flag across the import, so
    // every write shape fails with the one message.
    for script in [
        indoc! {r#"
            IMPORT ["lib.oxdock" AS LIB]
            IMPORT [STD]
            LET $lim: MAP<ANY> = $LIB::LIMITS
            LET $z: MAP<ANY> = MAP_SET($lim, "x", 1)
        "#},
        indoc! {r#"
            IMPORT ["lib.oxdock" AS LIB]
            LET $items: LIST<STRING> = $LIB::ITEMS
            LIST_APPEND $items "b"
        "#},
    ] {
        let err = Engine::new()
            .run_script(&root, script)
            .expect_err("imported CONST contents must refuse mutation");
        assert!(
            format!("{err:#}").contains("cannot mutate CONST"),
            "{err:#}"
        );
    }
}

#[test]
fn const_export_names_share_one_registry() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // FUNC, TYPE, and CONST exports share one registry: the same
    // name cannot ride two export kinds.
    for lib in [
        indoc! {r#"
            FUNC FOO(): INT {
                RETURN 1
            }
            CONST $FOO: INT = 2
            EXPORT FOO
            EXPORT CONST FOO
        "#},
        indoc! {r#"
            TYPE FOO = MAP<x: INT>
            CONST $FOO: INT = 2
            EXPORT TYPE FOO
            EXPORT CONST FOO
        "#},
    ] {
        write_lib(&root, "lib.oxdock", lib);
        let err = Engine::new()
            .run_script(
                &root,
                indoc! {r#"
                    IMPORT ["lib.oxdock" AS LIB]
                    ECHO hi
                "#},
            )
            .expect_err("duplicate EXPORT across kinds must fail");
        assert!(err.to_string().contains("duplicate EXPORT `FOO`"), "{err}");
    }
    // Same-name FUNC and CONST coexist when only one is exported;
    // each reads through its own namespace.
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            FUNC FOO(): INT {
                RETURN 1
            }
            CONST $FOO: INT = 2
            EXPORT CONST FOO
        "#},
    );
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                LET $x: INT = $LIB::FOO
                ASSERT_EQ $x 2
            "#},
        )
        .expect("CONST reads when the same-name FUNC stays private");
}

#[test]
fn const_module_local_shadow_stays_mutable() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            CONST $X: INT = 1
            FUNC SHADOWED(): INT {
                LET $X: INT = 2
                $X = 3
                RETURN $X
            }
            EXPORT SHADOWED
        "#},
    );
    let script = indoc! {r#"
        # A module function that shadows a module-level CONST with
        # a local LET mutates the local: static and runtime agree.
        IMPORT ["lib.oxdock" AS LIB]
        LET $r: INT = LIB::SHADOWED()
        ASSERT_EQ $r 3
    "#};
    Engine::new()
        .run_script(&root, script)
        .expect("module-local LET shadows module CONST");
}
