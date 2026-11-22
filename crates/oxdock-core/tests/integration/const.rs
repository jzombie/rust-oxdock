//! `CONST` bindings: deeply immutable values that read exactly like
//! `LET` but refuse every write. `$var = <expr>` reassignment, `LIST_APPEND`,
//! `MAP_SET`, and `MERGE_MAPS` on a `CONST` (or an alias of one) fail with one
//! message shape; redeclaring across `LET`/`CONST` fails in the
//! shared namespace. Only `CONST` crosses the module boundary, read
//! as `ALIAS::NAME`.

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
        CONST $S: STRING = "hi"
        CONST $I: INT = 7
        CONST $F: FLOAT = 2.5
        CONST $B: BOOL = true
        CONST $L: LIST<STRING> = ["a", "b"]
        CONST $M: MAP<ANY> = {x: 1, nested: {y: [1, 2]}}
        CONST $D: DURATION = 10s
        ASSERT_EQ {{ $S }} "hi"
        LET $i: INT = $I
        ASSERT_EQ $i 7
        LET $f: FLOAT = $F
        ASSERT_EQ $f 2.5
        LET $b: BOOL = $B
        ASSERT_EQ $b true
        ASSERT_CONTAINS {{ $L }} "a"
        LET $x: INT = $M.x
        ASSERT_EQ $x 1
        LET $y: INT = $M.nested.y.1
        ASSERT_EQ $y 2
        LET $d: DURATION = $D
        LET $e: DURATION = 10s
        ASSERT_EQ $d $e
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
                CONST $N: INT = 1
                $N = 2
            "#},
        )
        .expect_err("SET on a CONST must fail");
    assert!(err.to_string().contains("cannot mutate CONST N"), "{err}");
}

#[test]
fn const_list_append_fails_with_one_message() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // `LIST_APPEND` takes `$var` targets; naming a `CONST` list
    // fails at the mutation gate with one message.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $ITEMS: LIST<STRING> = ["a"]
                LIST_APPEND $ITEMS "b"
            "#},
        )
        .expect_err("LIST_APPEND on a CONST must fail");
    assert!(
        err.to_string().contains("cannot mutate CONST ITEMS"),
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
                CONST $M: MAP<ANY> = {a: 1}
                LET $n: MAP<ANY> = MAP_SET($M, "b", 2)
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
                CONST $M: MAP<ANY> = {a: 1}
                LET $n: MAP<ANY> = MERGE_MAPS([$M], "overwrite")
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
        CONST $C: LIST<ANY> = [$inner]
        LIST_APPEND $inner "b"
        ASSERT_CONTAINS $inner "b"
        ASSERT_EQ {{ $C.0.0 }} "a"
    "#};
    Engine::new()
        .run_script(&root, script)
        .expect("freeze detaches from the source");
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $C: LIST<STRING> = ["a"]
                LET $alias: LIST<STRING> = $C
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
        ("CONST $A: INT = 1", "CONST $A: INT = 2"),
        ("LET $A: INT = 1", "CONST $A: INT = 2"),
        ("CONST $A: INT = 1", "LET $A: INT = 2"),
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
        CONST $X: INT = 1
        [bool:true] {
            LET $X: INT = 2
            $X = 3
            WRITE inner.txt "{{ $X }}"
        }
        LET $back: STRING = READ inner.txt
        ASSERT_EQ $back "3"
        LET $held: INT = $X
        ASSERT_EQ $held 1
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
        CONST $OUT: STRING = ECHO hi
        ASSERT_EQ {{ $OUT }} "hi\n"
        CONST $T: HANDLE<INT> = ASYNC { RETURN 41 }
        CONST $N: INT = AWAIT $T
        LET $n: INT = $N
        ASSERT_EQ $n 41
        CONST $BLK: STRING = {
            RETURN "made"
        }
        ASSERT_EQ {{ $BLK }} "made"
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
            EXPORT CONST $VERSION
            EXPORT [CONST $LIMITS]
        "#},
    );
    let script = indoc! {r#"
        # Exported CONSTs read as $ALIAS::NAME: in math
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
            EXPORT CONST $NAME
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
fn const_qualified_reexport_unknown_alias_fails_at_parse() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            CONST $A: INT = 1
            EXPORT CONST $LIB::A
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
        .expect_err("qualified CONST re-export of an unknown alias must fail");
    assert!(
        err.to_string().contains("names unknown import alias `LIB`"),
        "{err}"
    );
}

#[test]
fn const_forwarded_reexport_reads() {
    // A qualified `EXPORT CONST $ALIAS::NAME` forwards the imported
    // file's binding: readers observe the defining file's value with
    // the frozen flag intact, exactly like a direct read.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            CONST $N: INT = 4
            CONST $LIMITS: MAP<ANY> = {max: 10, nested: {x: 1}}
            CONST $ITEMS: LIST<STRING> = ["a"]
            EXPORT CONST $N
            EXPORT [CONST $LIMITS, CONST $ITEMS]
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            IMPORT ["a.oxdock" AS A]
            EXPORT CONST $A::N
            EXPORT [CONST $A::LIMITS, CONST $A::ITEMS]
        "#},
    );
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["b.oxdock" AS B]
                IMPORT [STD]
                LET $n: INT = $B::N
                ASSERT_EQ $n 4
                LET $lim: MAP<ANY> = $B::LIMITS
                ASSERT_EQ $lim.max 10
                LET $items: LIST<STRING> = $B::ITEMS
                ASSERT_CONTAINS $items "a"
            "#},
        )
        .expect("forwarded CONSTs read through the re-export");
    // Freezing rides the forward: copies refuse every write shape.
    for script in [
        indoc! {r#"
            IMPORT ["b.oxdock" AS B]
            IMPORT [STD]
            LET $lim: MAP<ANY> = $B::LIMITS
            LET $z: MAP<ANY> = MAP_SET($lim, "x", 1)
        "#},
        indoc! {r#"
            IMPORT [STD]
            IMPORT ["b.oxdock" AS B]
            LET $z: MAP<ANY> = MAP_SET($B::LIMITS, "x", 1)
        "#},
        indoc! {r#"
            IMPORT ["b.oxdock" AS B]
            LET $items: LIST<STRING> = $B::ITEMS
            LIST_APPEND $items "b"
        "#},
    ] {
        let err = Engine::new()
            .run_script(&root, script)
            .expect_err("forwarded CONST contents must refuse mutation");
        assert!(
            format!("{err:#}").contains("cannot mutate CONST"),
            "{err:#}"
        );
    }
}

#[test]
fn const_forward_chain_resolves() {
    // Forwards compose: a two-hop chain reads the origin value.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            CONST $N: INT = 7
            EXPORT CONST $N
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            IMPORT ["a.oxdock" AS A]
            EXPORT CONST $A::N
        "#},
    );
    write_lib(
        &root,
        "c.oxdock",
        indoc! {r#"
            IMPORT ["b.oxdock" AS B]
            EXPORT CONST $B::N
        "#},
    );
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["c.oxdock" AS C]
                LET $n: INT = $C::N
                ASSERT_EQ $n 7
            "#},
        )
        .expect("chained CONST forwards resolve to the origin");
}

#[test]
fn const_forward_mutual_imports_rejected_as_cycle() {
    // A forward cycle cannot form: it would need a mutual import,
    // and the import cycle gate fires first. The chase cycle guard
    // stays as defense in depth for paths that never execute files.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            IMPORT ["b.oxdock" AS B]
            EXPORT CONST $B::N
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            IMPORT ["a.oxdock" AS A]
            EXPORT CONST $A::N
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["a.oxdock" AS A]
                LET $n: INT = $A::N
            "#},
        )
        .expect_err("mutual imports must fail");
    assert!(
        format!("{err:#}").contains("IMPORT cycle detected"),
        "{err:#}"
    );
}

#[test]
fn const_forward_missing_fails_loud() {
    // A forward names a real export: a dangling target fails at the
    // read naming the broken link.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            CONST $N: INT = 1
            EXPORT CONST $N
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            IMPORT ["a.oxdock" AS A]
            EXPORT CONST $A::MISSING
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["b.oxdock" AS B]
                LET $n: INT = $B::MISSING
            "#},
        )
        .expect_err("dangling CONST forward must fail");
    assert!(format!("{err:#}").contains("does not resolve"), "{err:#}");
}

#[test]
fn const_forward_to_let_fails_loud() {
    // Only `CONST` crosses: a forward landing on a `LET` names the
    // real error instead of an unknown-name miss.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            LET $PRIV: STRING = "shh"
            CONST $N: INT = 1
            EXPORT CONST $N
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            IMPORT ["a.oxdock" AS A]
            EXPORT CONST $A::PRIV
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["b.oxdock" AS B]
                LET $x: STRING = $B::PRIV
            "#},
        )
        .expect_err("forward to a LET must fail");
    assert!(format!("{err:#}").contains("cannot be exported"), "{err:#}");
}

#[test]
fn const_forward_same_base_rejected_at_parse() {
    // One name, one source: two forwards of the same base in one
    // file are one duplicate export, rejected at the `EXPORT` site
    // instead of waiting for a first read.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            CONST $N: INT = 1
            EXPORT CONST $N
        "#},
    );
    write_lib(
        &root,
        "c.oxdock",
        indoc! {r#"
            CONST $N: INT = 2
            EXPORT CONST $N
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            IMPORT ["a.oxdock" AS A]
            IMPORT ["c.oxdock" AS C]
            EXPORT CONST $A::N
            EXPORT CONST $C::N
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["b.oxdock" AS B]
                ECHO hi
            "#},
        )
        .expect_err("split CONST forwards must fail at parse");
    assert!(format!("{err:#}").contains("duplicate EXPORT"), "{err:#}");
}

#[test]
fn const_forward_local_and_forwarded_base_rejected_at_parse() {
    // A local export and a forward of the same base collide the same
    // way: the second `EXPORT` fails instead of shadowing.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            CONST $N: INT = 1
            EXPORT CONST $N
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            IMPORT ["a.oxdock" AS A]
            CONST $N: INT = 2
            EXPORT CONST $N
            EXPORT CONST $A::N
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["b.oxdock" AS B]
                ECHO hi
            "#},
        )
        .expect_err("local plus forwarded base must fail at parse");
    assert!(format!("{err:#}").contains("duplicate EXPORT"), "{err:#}");
}

#[test]
fn const_forward_disjoint_guards_resolve() {
    // Disjoint platform branches each forwarding their own chain
    // resolve per platform: same-frame guarded duplicates parse (an
    // unguarded duplicate still rejects as a probable typo), and the
    // shared walk chases every holder to its own terminal and unions
    // same tags instead of failing as ambiguous. Guard-gated
    // top-level imports persist without pushing a scope, so one read
    // serves every platform.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a-macos.oxdock",
        indoc! {r#"
            CONST $N: INT = 2
            EXPORT CONST $N
        "#},
    );
    write_lib(
        &root,
        "a-linux.oxdock",
        indoc! {r#"
            CONST $N: INT = 1
            EXPORT CONST $N
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            IMPORT ["a-macos.oxdock" AS A]
            EXPORT CONST $A::N
        "#},
    );
    write_lib(
        &root,
        "c.oxdock",
        indoc! {r#"
            IMPORT ["a-linux.oxdock" AS D]
            EXPORT CONST $D::N
        "#},
    );
    #[allow(clippy::disallowed_macros)]
    let expected = if cfg!(target_os = "macos") { 2 } else { 1 };
    let script = indoc! {r#"
        [os:macos] IMPORT ["b.oxdock" AS B]
        [os:linux] IMPORT ["c.oxdock" AS B]
        [os:windows] IMPORT ["c.oxdock" AS B]
        LET $n: INT = $B::N
        ASSERT_EQ $n @EXPECTED@
    "#}
    .replace("@EXPECTED@", &expected.to_string());
    Engine::new()
        .run_script(&root, &script)
        .expect("forward over disjoint guards resolves per platform");
}

#[test]
fn const_forward_script_qualified_rejected_at_parse() {
    // `SCRIPT::` is not a CONST namespace: local CONSTs read as
    // `$NAME`, so a qualified export spelling names nothing servable.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            CONST $N: INT = 1
            EXPORT CONST $SCRIPT::N
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
        .expect_err("SCRIPT-qualified CONST export must fail");
    assert!(err.to_string().contains("SCRIPT"), "{err}");
}

#[test]
fn const_forward_cross_kind_base_rejected_at_parse() {
    // One base name rides exactly one export kind: a forwarded
    // CONST base colliding with a FUNC or TYPE export (either
    // order) is the same duplicate as two identical names.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for (order, lib) in [
        (
            "func first",
            indoc! {r#"
                IMPORT ["a.oxdock" AS A]
                FUNC FOO(): INT {
                    RETURN 1
                }
                EXPORT FOO
                EXPORT CONST $A::FOO
            "#},
        ),
        (
            "forward first",
            indoc! {r#"
                IMPORT ["a.oxdock" AS A]
                FUNC FOO(): INT {
                    RETURN 1
                }
                EXPORT CONST $A::FOO
                EXPORT FOO
            "#},
        ),
        (
            "type first",
            indoc! {r#"
                IMPORT ["a.oxdock" AS A]
                TYPE FOO = INT
                EXPORT TYPE FOO
                EXPORT CONST $A::FOO
            "#},
        ),
        (
            "forward before type",
            indoc! {r#"
                IMPORT ["a.oxdock" AS A]
                TYPE FOO = INT
                EXPORT CONST $A::FOO
                EXPORT TYPE FOO
            "#},
        ),
    ] {
        write_lib(&root, "b.oxdock", lib);
        let err = Engine::new()
            .run_script(
                &root,
                indoc! {r#"
                    IMPORT ["b.oxdock" AS B]
                    ECHO hi
                "#},
            )
            .expect_err("cross-kind base collision must fail at parse");
        assert!(
            format!("{err:#}").contains("duplicate EXPORT"),
            "{order}: {err:#}"
        );
    }
}

#[test]
fn const_forward_unread_broken_fails_at_load() {
    // Eager closure validation: a dangling forward fails when its
    // module loads, even when no expression reads it yet.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            CONST $N: INT = 1
            EXPORT CONST $N
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            IMPORT ["a.oxdock" AS A]
            EXPORT CONST $A::MISSING
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["b.oxdock" AS B]
                ECHO hi
            "#},
        )
        .expect_err("unread broken forward must fail at load");
    assert!(format!("{err:#}").contains("does not resolve"), "{err:#}");
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
            EXPORT CONST $LIMITS
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                LET $v: INT = $LIB::LIMITS.max
            "#},
        )
        .expect_err("key paths on ALIAS::NAME must fail");
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
            CONST $M: MAP<ANY> = {nested: {x: 1}}
            LET $inner: MAP<ANY> = $M.nested
            LET $z: MAP<ANY> = MAP_SET($inner, "y", 2)
        "#},
        indoc! {r#"
            IMPORT [STD]
            CONST $M: MAP<ANY> = {items: ["a"]}
            LET $items: LIST<STRING> = $M.items
            LIST_APPEND $items "b"
        "#},
        indoc! {r#"
            IMPORT [STD]
            CONST $C: LIST<ANY> = [{x: 1}]
            LET $first: MAP<ANY> = $C.0
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
            EXPORT CONST $VERSION
            EXPORT [CONST $LIMITS, CONST $ITEMS]
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
            EXPORT CONST $FOO
        "#},
        indoc! {r#"
            TYPE FOO = MAP<x: INT>
            CONST $FOO: INT = 2
            EXPORT TYPE FOO
            EXPORT CONST $FOO
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
            EXPORT CONST $FOO
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
fn const_readline_and_prompt_refuse_const() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // `READ_LINE`/`PROMPT` declare when absent but mutate when
    // present: onto a `CONST` both must refuse, statically (new
    // `READ_LINE`/`PROMPT` only bind `$var`: a bare word never
    // names a target, so `$CONST` is the only way to attempt a
    // CONST write through them — and the gate refuses it.
    for script in [
        indoc! {r#"
            CONST $X: STRING = "a"
            READ_LINE $X
        "#},
        indoc! {r#"
            CONST $X: STRING = "a"
            PROMPT $X "name?"
        "#},
    ] {
        let err = Engine::new()
            .run_script(&root, script)
            .expect_err("READ_LINE/PROMPT onto a CONST must fail");
        assert!(
            format!("{err:#}").contains("cannot mutate CONST X"),
            "{err:#}"
        );
    }
}

#[test]
fn const_async_redeclare_fails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // `LET ... = ASYNC` after `CONST ... = ASYNC` (and every other
    // direction) overlaps instead of overwriting: the static pass
    // mirrors the runtime redeclare error.
    for (first, second) in [
        (
            "CONST $T: HANDLE<INT> = ASYNC { RETURN 1 }",
            "LET $T: HANDLE<INT> = ASYNC { RETURN 2 }",
        ),
        (
            "LET $T: HANDLE<INT> = ASYNC { RETURN 1 }",
            "CONST $T: HANDLE<INT> = ASYNC { RETURN 2 }",
        ),
        (
            "CONST $T: HANDLE<INT> = ASYNC { RETURN 1 }",
            "CONST $T: HANDLE<INT> = ASYNC { RETURN 2 }",
        ),
    ] {
        let err = Engine::new()
            .run_script(&root, &format!("{first}\n{second}\n"))
            .expect_err("ASYNC redeclare across LET/CONST must fail");
        assert!(err.to_string().contains("redeclaration error"), "{err}");
        assert!(err.to_string().contains("CONST"), "{err}");
    }
}

#[test]
fn const_guards_are_site_aware() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // Disjoint platform guards share one scope: mutating the
    // Windows `LET` under its own guard is legal even though a
    // Linux `CONST` of the same name exists (the block skips at
    // runtime on unix; static admits it on every host).
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                [family:unix] CONST $X: INT = 1
                [family:windows] LET $X: INT = 2
                [family:windows] $X = 3
            "#},
        )
        .expect("guard-disjoint LET mutation passes");
    // The same `CONST` mutated under its own guard still refuses.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                [family:unix] CONST $X: INT = 1
                [family:windows] LET $X: INT = 2
                [family:unix] $X = 3
            "#},
        )
        .expect_err("guard-matching CONST mutation must fail");
    assert!(
        format!("{err:#}").contains("cannot mutate CONST X"),
        "{err:#}"
    );
}

#[test]
fn const_qualified_reads_in_math_call_args_and_templates() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            CONST $N: INT = 4
            FUNC DOUBLE($x: INT): INT {
                RETURN $x * 2
            }
            EXPORT DOUBLE
            EXPORT CONST $N
        "#},
    );
    // `$ALIAS::NAME` rides every expression position a `$var`
    // does: math operands, call arguments, templates.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                LET $e: INT = $LIB::N * 2
                ASSERT_EQ $e 8
                LET $d: INT = LIB::DOUBLE($LIB::N)
                ASSERT_EQ $d 8
                LET $t: STRING = "n={{ $LIB::N }}"
                ASSERT_EQ $t "n=4"
            "#},
        )
        .expect("qualified CONST reads in math, calls, templates");
}

#[test]
fn const_ambiguous_alias_fails_loud() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // One alias, two files, same exported `CONST`: no silent pick.
    // Same-block guarded duplicates parse (unguarded duplicates
    // still reject as probable typos), and overlapping guards
    // resolve loud on use.
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            CONST $X: INT = 1
            EXPORT CONST $X
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            CONST $X: INT = 2
            EXPORT CONST $X
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                LET $go: BOOL = true
                IF $go {
                    [os:macos] IMPORT ["a.oxdock" AS LIB]
                    [os:macos] IMPORT ["b.oxdock" AS LIB]
                    LET $x: INT = $LIB::X
                }
            "#},
        )
        .expect_err("ambiguous CONST must fail");
    assert!(format!("{err:#}").contains("ambiguous"), "{err:#}");
}

#[test]
fn block_import_unwinds_on_block_exit() {
    // Block imports scope to their block like `LET`/`ENV`: readable
    // inside the block (and nested blocks), unknown past the closing
    // brace, while top-level imports persist for the whole unit. The
    // file-backed `EXPAND` (which skips static validation) proves the
    // runtime unwinds too, not just the static gate.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            CONST $X: INT = 1
            EXPORT CONST $X
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            CONST $Y: INT = 2
            EXPORT CONST $Y
            FUNC WHO(): STRING {
                RETURN "b"
            }
            EXPORT WHO
        "#},
    );
    write_lib(&root, "t.txt", "{{ LIB::WHO() }}");
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["a.oxdock" AS A]
                LET $go: BOOL = true
                IF $go {
                    IMPORT ["b.oxdock" AS LIB]
                    LET $y: INT = $LIB::Y
                    ASSERT_EQ $y 2
                    IF $go {
                        LET $z: INT = $LIB::Y
                        ASSERT_EQ $z 2
                    }
                }
                LET $x: INT = $A::X
                ASSERT_EQ $x 1
            "#},
        )
        .expect("block import reads inside, top-level persists");
    // Static read past the block fails closed.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                LET $go: BOOL = true
                IF $go {
                    IMPORT ["b.oxdock" AS B]
                }
                LET $y: INT = $B::Y
            "#},
        )
        .expect_err("block import must not leak past its block");
    assert!(
        format!("{err:#}").contains("no import alias `B`"),
        "{err:#}"
    );
    // Runtime read past the block (via file-backed `EXPAND`, which
    // static validation never sees) fails the same way: the
    // placeholder gate classifies against the live unit table, so an
    // unwound alias reads unknown instead of resolving.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                LET $go: BOOL = true
                IF $go {
                    IMPORT ["b.oxdock" AS LIB]
                }
                LET $o: STRING = EXPAND "t.txt"
            "#},
        )
        .expect_err("runtime must unwind block imports too");
    assert!(
        format!("{err:#}").contains("unknown placeholder function 'LIB::WHO'"),
        "{err:#}"
    );
}

#[test]
fn func_block_imports_unwind_and_isolate_callees() {
    // Block imports inside a `FUNC` body unwind on block exit in both
    // layers (SEV-1a): the runtime used to route them into the call
    // frame, leaking past the block the static gate already closed.
    // A caller block's import never leaks into a same-unit sibling
    // callee (SEV-1b): the callee resolves against the unit top-level
    // plus its own imports only. Bare guarded blocks unwind too
    // (SEV-2): the static walk used to keep their import frame past
    // the closing brace while the runtime popped it.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            CONST $Y: INT = 2
            EXPORT CONST $Y
            FUNC WHO(): STRING {
                RETURN "b"
            }
            EXPORT WHO
        "#},
    );
    write_lib(&root, "t.txt", "{{ LIB::WHO() }}");
    // SEV-1a, static: a read after the block inside the same function
    // fails closed.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                FUNC READER(): STRING {
                    LET $go: BOOL = true
                    IF $go {
                        IMPORT ["b.oxdock" AS LIB]
                    }
                    LET $y: INT = $LIB::Y
                    RETURN "unreachable"
                }
                LET $r: STRING = READER()
            "#},
        )
        .expect_err("function block import must not leak past its block");
    assert!(
        format!("{err:#}").contains("no import alias `LIB`"),
        "{err:#}"
    );
    // SEV-1a, runtime: the same shape read through file-backed
    // `EXPAND` (invisible to static validation) fails the same way.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                FUNC READER(): STRING {
                    LET $go: BOOL = true
                    IF $go {
                        IMPORT ["b.oxdock" AS LIB]
                    }
                    LET $o: STRING = EXPAND "t.txt"
                    RETURN $o
                }
                LET $r: STRING = READER()
            "#},
        )
        .expect_err("runtime must unwind function block imports too");
    assert!(
        format!("{err:#}").contains("unknown placeholder function 'LIB::WHO'"),
        "{err:#}"
    );
    // SEV-1b, static: sibling callee `G` reads nothing from caller
    // `F`'s block; `G` is checked standalone against the unit top
    // level, so the read fails closed before anything runs.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                FUNC G(): INT {
                    LET $y: INT = $LIB::Y
                    RETURN $y
                }
                FUNC F(): INT {
                    LET $go: BOOL = true
                    IF $go {
                        IMPORT ["b.oxdock" AS LIB]
                        LET $g: INT = G()
                        RETURN $g
                    }
                    RETURN -1
                }
                LET $r: INT = F()
            "#},
        )
        .expect_err("caller block import must not leak into sibling callee");
    assert!(
        format!("{err:#}").contains("no import alias `LIB`"),
        "{err:#}"
    );
    // SEV-1b, runtime: `G` only `EXPAND`s (static-blind), so the
    // runtime gate alone decides; the popped caller frame stays out.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                FUNC G(): STRING {
                    LET $o: STRING = EXPAND "t.txt"
                    RETURN $o
                }
                FUNC F(): STRING {
                    LET $go: BOOL = true
                    IF $go {
                        IMPORT ["b.oxdock" AS LIB]
                        LET $g: STRING = G()
                        RETURN $g
                    }
                    RETURN "unreachable"
                }
                LET $r: STRING = F()
            "#},
        )
        .expect_err("runtime must not leak caller block imports into callees");
    assert!(
        format!("{err:#}").contains("unknown placeholder function 'LIB::WHO'"),
        "{err:#}"
    );
    // SEV-2: a bare guarded block unwinds its import past `}` in both
    // layers. The single-name read below fails closed either way
    // (the runtime drops the frame on scope exit); the overlapping
    // pair pins the static side specifically: without collection
    // scoping or the scope-exit pop, the walk would resolve the
    // leaked entries as ambiguous instead of unknown.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                [bool:true] {
                    IMPORT ["b.oxdock" AS B]
                    LET $y: INT = $B::Y
                    ASSERT_EQ $y 2
                }
            "#},
        )
        .expect("bare guarded block import reads inside its block");
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            CONST $X: INT = 1
            EXPORT CONST $X
        "#},
    );
    write_lib(
        &root,
        "c.oxdock",
        indoc! {r#"
            CONST $X: INT = 2
            EXPORT CONST $X
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                [bool:true] {
                    [os:macos] IMPORT ["a.oxdock" AS LIB]
                    [os:macos] IMPORT ["c.oxdock" AS LIB]
                }
                LET $x: INT = $LIB::X
            "#},
        )
        .expect_err("bare guarded block imports must unwind past their brace");
    assert!(format!("{err:#}").contains("unknown CONST"), "{err:#}");
    assert!(!format!("{err:#}").contains("ambiguous"), "{err:#}");
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                [bool:true] {
                    IMPORT ["b.oxdock" AS LIB]
                }
                LET $o: STRING = EXPAND "t.txt"
            "#},
        )
        .expect_err("runtime must unwind bare guarded block imports too");
    assert!(
        format!("{err:#}").contains("unknown placeholder function 'LIB::WHO'"),
        "{err:#}"
    );
    // `ISOLATED` blocks scope the same way: the runtime runs the
    // body under a pushed scope, so a static walk that merely
    // scrubbed env declarations would leak the import past `}`.
    // In-block reads resolve; the overlapping pair past the brace
    // fails closed as unknown (never ambiguous), pinning the
    // scope push/pop rather than just the collection gate.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                ISOLATED {
                    IMPORT ["b.oxdock" AS B]
                    LET $y: INT = $B::Y
                    ASSERT_EQ $y 2
                }
            "#},
        )
        .expect("ISOLATED block import reads inside its block");
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                ISOLATED {
                    [os:macos] IMPORT ["a.oxdock" AS LIB]
                    [os:macos] IMPORT ["c.oxdock" AS LIB]
                }
                LET $x: INT = $LIB::X
            "#},
        )
        .expect_err("ISOLATED imports must unwind past their brace");
    assert!(format!("{err:#}").contains("unknown CONST"), "{err:#}");
    assert!(!format!("{err:#}").contains("ambiguous"), "{err:#}");
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                ISOLATED {
                    IMPORT ["b.oxdock" AS LIB]
                }
                LET $o: STRING = EXPAND "t.txt"
            "#},
        )
        .expect_err("runtime must unwind ISOLATED block imports too");
    assert!(
        format!("{err:#}").contains("unknown placeholder function 'LIB::WHO'"),
        "{err:#}"
    );
}

#[test]
fn const_missing_hints_stay_alias_scoped() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            CONST $SHARED: INT = 1
            LET $priv: STRING = "shh"
        "#},
    );
    write_lib(&root, "b.oxdock", "ECHO hi\n");
    // A typo under `B` never leaks `A`'s unexported names.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["a.oxdock" AS A]
                IMPORT ["b.oxdock" AS B]
                LET $x: INT = $B::SHARED
            "#},
        )
        .expect_err("cross-alias typo must stay generic");
    assert!(
        format!("{err:#}").contains("no import alias `B` exports that name"),
        "{err:#}"
    );
    assert!(!format!("{err:#}").contains("module holds"), "{err:#}");
    // The same typo under the owning alias hints precisely.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["a.oxdock" AS A]
                LET $x: INT = $A::SHARED
            "#},
        )
        .expect_err("same-alias CONST typo must hint EXPORT");
    assert!(
        format!("{err:#}").contains("module holds `SHARED` as CONST"),
        "{err:#}"
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["a.oxdock" AS A]
                LET $x: STRING = $A::priv
            "#},
        )
        .expect_err("same-alias LET typo must hint non-exportability");
    assert!(
        format!("{err:#}").contains("is a LET binding and cannot be exported"),
        "{err:#}"
    );
}

#[test]
fn const_inside_func_body_refuses_mutation() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // `CONST` declares inside function bodies too, and reads back.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                FUNC MAKE(): INT {
                    CONST $C: INT = 5
                    RETURN $C
                }
                LET $r: INT = MAKE()
                ASSERT_EQ $r 5
            "#},
        )
        .expect("FUNC-local CONST binds and reads");
    // Mutating it refuses with the one message.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                FUNC BAD(): INT {
                    CONST $C: INT = 5
                    $C = 6
                    RETURN $C
                }
                LET $r: INT = BAD()
            "#},
        )
        .expect_err("FUNC-local CONST mutation must fail");
    assert!(
        format!("{err:#}").contains("cannot mutate CONST C"),
        "{err:#}"
    );
}

#[test]
fn const_loop_var_shadows_and_iterates() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // A loop variable shadows an outer `CONST` for the loop, then
    // the outer binding survives intact.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $I: INT = 99
                FOR $I: INT IN [1, 2, 3] {
                    ECHO $I
                }
                LET $held: INT = $I
                ASSERT_EQ $held 99
            "#},
        )
        .expect("loop var shadows outer CONST");
    // Iterating a `CONST` list reads every element.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $ITEMS: LIST<STRING> = ["a", "b"]
                FOR $s: STRING IN $ITEMS {
                    APPEND seen.txt $s
                }
                LET $got: STRING = READ seen.txt
                ASSERT_CONTAINS $got "a"
                ASSERT_CONTAINS $got "b"
            "#},
        )
        .expect("FOR over a CONST list reads");
}

#[test]
fn const_inner_const_shadows_outer_let() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // Shadowing runs both ways: an inner `CONST` shadows an outer
    // `LET`, reads inside, and the outer survives the block.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                LET $X: INT = 1
                [bool:true] {
                    CONST $X: INT = 10
                    LET $t: INT = $X
                    ASSERT_EQ $t 10
                }
                ASSERT_EQ $X 1
            "#},
        )
        .expect("inner CONST shadows outer LET");
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                LET $X: INT = 1
                [bool:true] {
                    CONST $X: INT = 10
                    $X = 11
                }
            "#},
        )
        .expect_err("inner CONST mutation must fail");
    assert!(
        format!("{err:#}").contains("cannot mutate CONST X"),
        "{err:#}"
    );
}

#[test]
fn const_sigiled_pipe_and_control_flow_reads() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // A sigiled `CONST $P: PIPE` mints a pipe like `LET`, and `CONST`
    // values read in every condition position.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $P: PIPE
                WITH_IO [stdout=$P] {
                    ECHO hi
                }
                WITH_IO [stdin=$P] READ_LINE $line
                ASSERT_EQ $line "hi"
                CONST $MODE: STRING = "go"
                CONST $LIMIT: INT = 2
                LET $n: INT = 0
                IF $MODE == "go" {
                    WRITE marker.txt "branched"
                }
                SWITCH $MODE {
                    CASE "go" {
                        APPEND marker.txt "switched"
                    }
                }
                WHILE $n < $LIMIT {
                    $n = $n + 1
                }
                ASSERT_EQ $n 2
                LET $m: STRING = READ marker.txt
                ASSERT_CONTAINS $m "branched"
                ASSERT_CONTAINS $m "switched"
            "#},
        )
        .expect("bare PIPE CONST and control-flow reads");
}

#[test]
fn const_func_export_with_private_const_coexists() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // Same-name `FUNC` and `CONST` coexist when only the `FUNC` is
    // exported; the private `CONST` never leaks through the call.
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            FUNC FOO(): INT {
                RETURN 1
            }
            CONST $FOO: INT = 2
            EXPORT FOO
        "#},
    );
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                LET $y: INT = LIB::FOO()
                ASSERT_EQ $y 1
            "#},
        )
        .expect("FUNC reads when the same-name CONST stays private");
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

#[test]
fn const_local_scalar_alias_rebinds() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // Frozen-ness guards container buffers, never rebinding: a
    // `LET` copy of a scalar `CONST` rebinds freely while the
    // original holds.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $S: STRING = "a"
                LET $copy: STRING = $S
                $copy = "b"
                ASSERT_EQ $copy "b"
                ASSERT_EQ {{ $S }} "a"
            "#},
        )
        .expect("scalar CONST alias rebinds, original holds");
}

#[test]
fn const_qualified_targets_never_bind() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            CONST $N: INT = 4
            CONST $ITEMS: LIST<STRING> = ["a"]
            EXPORT CONST $N
            EXPORT [CONST $ITEMS]
        "#},
    );
    // `ALIAS::NAME` is a read-only expression: it never serves as
    // a reassignment (`$var = <expr>`), `LIST_APPEND`, or `READ_LINE` target.
    for (script, fragment) in [
        (
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                $LIB::N = 5
            "#},
            "never binding targets",
        ),
        (
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                LIST_APPEND $LIB::ITEMS "b"
            "#},
            "expected $var",
        ),
        (
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                READ_LINE $LIB::N
            "#},
            "requires a $variable",
        ),
    ] {
        let err = Engine::new()
            .run_script(&root, script)
            .expect_err("qualified binding target must fail");
        assert!(format!("{err:#}").contains(fragment), "{err:#}");
    }
}

#[test]
fn const_direct_qualified_functional_rebuild_fails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            CONST $LIMITS: MAP<ANY> = {max: 10}
            EXPORT CONST $LIMITS
        "#},
    );
    // No local copy needed: the qualified value itself carries the
    // frozen flag, so a direct functional rebuild refuses.
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                IMPORT ["lib.oxdock" AS LIB]
                LET $z: MAP<ANY> = MAP_SET($LIB::LIMITS, "x", 1)
            "#},
        )
        .expect_err("direct MAP_SET on qualified CONST must fail");
    assert!(
        format!("{err:#}").contains("cannot mutate CONST"),
        "{err:#}"
    );
}

#[test]
fn const_func_param_shadows_outer_const() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // Parameters bind in the function scope: same name as an outer
    // `CONST` shadows for the call, and the outer holds after.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $X: INT = 1
                FUNC F($X: INT): INT {
                    RETURN $X
                }
                LET $r: INT = F(7)
                ASSERT_EQ $r 7
                LET $held: INT = $X
                ASSERT_EQ $held 1
            "#},
        )
        .expect("func param shadows outer CONST");
}

#[test]
fn const_declares_inside_loop_bodies() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // A `CONST` inside a loop body binds fresh per iteration like
    // any block-scoped declaration.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                FOR $i: INT IN [1, 2] {
                    CONST $C: INT = 5
                    ECHO $C
                }
                ECHO loop-ok
            "#},
        )
        .expect("CONST binds inside loop bodies");
}

#[test]
fn const_local_reads_in_templates_math_and_calls() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // Local `CONST` rides every expression position a `LET` does.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                FUNC D($x: INT): INT {
                    RETURN $x * 2
                }
                CONST $S: STRING = "hi"
                CONST $A: INT = 3
                LET $t: STRING = "{{ $S }}!"
                ASSERT_EQ $t "hi!"
                LET $m: INT = $A * 2
                ASSERT_EQ $m 6
                LET $d: INT = D($A)
                ASSERT_EQ $d 6
            "#},
        )
        .expect("local CONST in templates, math, call args");
}

#[test]
fn const_switch_case_and_kv_iteration_read_const() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // `CONST` values ride `SWITCH` case patterns and key-value
    // iteration over a `CONST` map reads every entry.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $GO: STRING = "go"
                LET $mode: STRING = "go"
                SWITCH $mode {
                    CASE $GO {
                        WRITE hit.txt "matched"
                    }
                }
                LET $h: STRING = READ hit.txt
                ASSERT_EQ $h "matched"
                CONST $M: MAP<ANY> = {a: "x", b: "y"}
                FOR $k: STRING, $v: STRING IN $M {
                    APPEND seen.txt "{{ $k }}={{ $v }}"
                }
                LET $g: STRING = READ seen.txt
                ASSERT_CONTAINS $g "a=x"
                ASSERT_CONTAINS $g "b=y"
            "#},
        )
        .expect("SWITCH CASE and FOR k,v read CONST");
}

#[test]
fn const_await_cancel_display_round_trips_sigiled() {
    // Sigiled `$CONST` handles survive `Display` verbatim:
    // `AWAIT $TASK`, `CANCEL $TASK`, and both await-capture forms
    // re-parse from their rendered spelling.
    for script in [
        "CONST $TASK: HANDLE<INT> = ASYNC { RETURN 1 }\nAWAIT $TASK\n",
        "CONST $TASK: HANDLE<VOID> = ASYNC { RETURN }\nCANCEL $TASK\n",
        "CONST $TASK: HANDLE<INT> = ASYNC { RETURN 7 }\nLET $v: INT = AWAIT $TASK\n",
        "CONST $TASK: HANDLE<INT> = ASYNC { RETURN 7 }\nCONST $OUT: INT = AWAIT $TASK\n",
    ] {
        let steps = oxdock_core::parse_script(script).expect("parse sigiled CONST handle");
        let rendered = steps
            .iter()
            .map(|s| s.kind.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        oxdock_core::parse_script(&rendered).expect("re-parse rendered CONST handle");
    }
    // A `CONST` pipe wires like any other binding: using the handle
    // never rebinds it, so `WITH_IO` accepts `$P` either way.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(&root, "CONST $P: PIPE\nWITH_IO [stdout=$P] ECHO hi\n")
        .expect("WITH_IO accepts a CONST pipe handle");
}

#[test]
fn entry_const_visible_in_entry_func() {
    // File-top-level `CONST`s read inside the file's own `FUNC`s
    // (#318): immutable file state is visible by scope, while only
    // mutable state crosses as explicit arguments.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                CONST $WORDS: LIST<STRING> = ["alpha", "bravo", "charlie"]
                FUNC COUNT(): INT {
                    RETURN LEN($WORDS)
                }
                FUNC FIRST(): STRING {
                    LET $first: STRING = ""
                    FOR $i: INT, $w: STRING IN $WORDS {
                        IF $i == 0 {
                            $first = $w
                        }
                    }
                    RETURN $first
                }
                LET $n: INT = COUNT()
                ASSERT_EQ $n 3
                LET $f: STRING = FIRST()
                ASSERT_EQ $f "alpha"
            "#},
        )
        .expect("entry CONST reads inside entry FUNCs");
}

#[test]
fn entry_const_call_before_declare_fails() {
    // Declaration order gates visibility: a call landing before the
    // `CONST` executes would read uninitialized memory, so the
    // static gate fails it instead of seeding the full union.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                FUNC EARLY(): STRING {
                    RETURN $MARKER
                }
                LET $r: STRING = EARLY()
                CONST $MARKER: STRING = "late"
            "#},
        )
        .expect_err("call before CONST declaration must fail");
    assert!(
        format!("{err:#}").contains("undefined variable $MARKER"),
        "{err:#}"
    );
}

#[test]
fn entry_const_visible_across_nested_entry_calls() {
    // Seeding is transitive: a callee called from a callee observes
    // the same file `CONST`s, never caller locals.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                CONST $BASE: INT = 21
                FUNC DOUBLE(): INT {
                    RETURN $BASE * 2
                }
                FUNC QUAD(): INT {
                    LET $d: INT = DOUBLE()
                    RETURN $d * 2
                }
                LET $q: INT = QUAD()
                ASSERT_EQ $q 84
            "#},
        )
        .expect("nested entry calls observe file CONSTs");
}

#[test]
fn entry_const_mutation_from_func_fails() {
    // The const gate rides the seed: writes through a file `CONST`
    // fail inside bodies with the same one-shape message.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for script in [
        indoc! {r#"
            IMPORT [STD]
            CONST $GREETING: STRING = "hi"
            FUNC REASSIGN(): VOID {
                $GREETING = "bye"
            }
            REASSIGN()
        "#},
        indoc! {r#"
            IMPORT [STD]
            CONST $ITEMS: LIST<STRING> = ["a"]
            FUNC TAMPER(): VOID {
                $ITEMS = ["b"]
            }
            TAMPER()
        "#},
    ] {
        let err = Engine::new()
            .run_script(&root, script)
            .expect_err("file CONST mutation from a body must fail");
        assert!(
            format!("{err:#}").contains("cannot mutate CONST"),
            "{err:#}"
        );
    }
}

#[test]
fn entry_let_stays_invisible_while_const_shows() {
    // The exact boundary in one scope: the file `CONST` reads, the
    // file `LET` does not.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                CONST $C: STRING = "const-val"
                LET $v: STRING = "let-val"
                FUNC MIXED(): STRING {
                    RETURN $C
                }
                LET $c: STRING = MIXED()
                ASSERT_EQ $c "const-val"
            "#},
        )
        .expect("file CONST reads while file LET stays out");
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                CONST $C: STRING = "const-val"
                LET $v: STRING = "let-val"
                FUNC LEAK(): STRING {
                    RETURN $v
                }
                LET $c: STRING = LEAK()
            "#},
        )
        .expect_err("file LET must stay invisible in the body");
    assert!(format!("{err:#}").contains("undefined variable"), "{err:#}");
}

#[test]
fn module_const_visible_in_module_func() {
    // The module precedent: a file's own top-level `CONST` reads in
    // its bodies through the module store.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            CONST $TAG: STRING = "lib-tag"
            EXPORT [CONST $TAG, FUNC TAGOF]
            FUNC TAGOF(): STRING {
                RETURN $TAG
            }
        "#},
    );
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "lib.oxfile" AS LIB]
                LET $t: STRING = LIB::TAGOF()
                ASSERT_EQ $t "lib-tag"
            "#},
        )
        .expect("module CONST reads in module bodies");
}

#[test]
fn qualified_const_readable_in_func_bodies() {
    // Cross-module `CONST`s resolve through the export registry, so
    // they read inside bodies exactly like at top level: an entry
    // body reads the import, and a module body reads another
    // module's export.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "a.oxfile",
        indoc! {r#"
            CONST $FLAG: STRING = "a-flag"
            EXPORT [CONST $FLAG]
        "#},
    );
    write_lib(
        &root,
        "b.oxfile",
        indoc! {r#"
            IMPORT ["a.oxfile" AS A]
            EXPORT [FUNC SHOW]
            FUNC SHOW(): STRING {
                RETURN $A::FLAG
            }
        "#},
    );
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "a.oxfile" AS A, "b.oxfile" AS B]
                FUNC READQ(): STRING {
                    RETURN $A::FLAG
                }
                LET $q: STRING = READQ()
                ASSERT_EQ $q "a-flag"
                LET $s: STRING = B::SHOW()
                ASSERT_EQ $s "a-flag"
            "#},
        )
        .expect("qualified CONST reads in entry and module bodies");
}

#[test]
fn entry_param_and_local_shadow_const_stays_mutable() {
    // Shadowing runs both ways inside bodies too: a param or a
    // body-local LET shadowing a file CONST stays mutable while
    // the file CONST holds. The seed frame sits beneath the params
    // frame, so the innermost binding decides on both sides.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                CONST $X: INT = 1
                FUNC SHADOW($X: INT): INT {
                    $X = $X + 10
                    RETURN $X
                }
                FUNC LOCALSHADOW(): INT {
                    LET $X: INT = 2
                    $X = $X + 10
                    RETURN $X
                }
                LET $s: INT = SHADOW(5)
                ASSERT_EQ $s 15
                LET $hx: INT = $X
                ASSERT_EQ $hx 1
                LET $l: INT = LOCALSHADOW()
                ASSERT_EQ $l 12
                LET $hx2: INT = $X
                ASSERT_EQ $hx2 1
            "#},
        )
        .expect("shadowing param and local stay mutable");
}

#[test]
fn entry_nested_callee_sees_file_const_not_caller_param() {
    // Caller locals never cross: `INNER` observes the file `CONST`
    // even when the caller shadows it with a parameter, and the
    // static seeding agrees exactly (no divergence to debug).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                CONST $BASE: INT = 21
                FUNC INNER(): INT {
                    RETURN $BASE
                }
                FUNC SHADOW($BASE: INT): INT {
                    LET $inner: INT = INNER()
                    RETURN $inner + $BASE
                }
                LET $r: INT = SHADOW(99)
                ASSERT_EQ $r 120
            "#},
        )
        .expect("nested callee sees file CONST, caller keeps its param");
}

#[test]
fn entry_body_local_const_does_not_leak_to_nested_callee() {
    // A body-local `CONST` belongs to its own call frame: a nested
    // callee reads the file `CONST` of the same name, never the
    // caller's local.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                CONST $V: INT = 1
                FUNC INNER(): INT {
                    RETURN $V
                }
                FUNC OUTER(): INT {
                    CONST $V: INT = 2
                    LET $inner: INT = INNER()
                    RETURN $inner + $V
                }
                LET $r: INT = OUTER()
                ASSERT_EQ $r 3
            "#},
        )
        .expect("body-local CONST stays local, file CONST stays visible");
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                FUNC OUTER(): INT {
                    CONST $ONLY_LOCAL: INT = 5
                    RETURN INNER()
                }
                FUNC INNER(): INT {
                    RETURN $ONLY_LOCAL
                }
                LET $r: INT = OUTER()
            "#},
        )
        .expect_err("body-local CONST must stay invisible to nested callees");
    assert!(
        format!("{err:#}").contains("undefined variable $ONLY_LOCAL"),
        "{err:#}"
    );
}

#[test]
fn entry_body_local_const_in_seeded_func_stays_immutable() {
    // The frame-skew shape: a body-local CONST declared inside a
    // seeded entry FUNC (file top-level CONST present) must refuse
    // every write exactly like a file CONST, scalar and container.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for script in [
        indoc! {r#"
            IMPORT [STD]
            CONST $A: INT = 1
            FUNC F(): INT {
                CONST $B: INT = 2
                $B = 3
                RETURN $B
            }
            LET $r: INT = F()
        "#},
        indoc! {r#"
            IMPORT [STD]
            CONST $A: INT = 1
            FUNC F(): LIST<STRING> {
                CONST $C: LIST<STRING> = ["x"]
                $C = ["y"]
                RETURN $C
            }
            LET $r: LIST<STRING> = F()
        "#},
    ] {
        let err = Engine::new()
            .run_script(&root, script)
            .expect_err("body-local CONST in a seeded body must stay immutable");
        assert!(
            format!("{err:#}").contains("cannot mutate CONST"),
            "{err:#}"
        );
    }
}

#[test]
fn template_call_reads_local_const_and_key_path() {
    // Runtime/static agreement: `{{ STD::LEN($ITEMS) }}` and
    // `{{ STD::LEN($CFG.items) }}` resolve `$`-prefixed local `CONST`s and
    // `CONST` key paths at runtime exactly as the static gate admits.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                CONST $ITEMS: LIST<STRING> = ["a", "b"]
                LET $t: STRING = "n={{ STD::LEN($ITEMS) }}"
                ASSERT_EQ $t "n=2"
                CONST $CFG: MAP<items:LIST<STRING>> = {items: ["a", "b", "c"]}
                LET $u: STRING = "n={{ STD::LEN($CFG.items) }}"
                ASSERT_EQ $u "n=3"
            "#},
        )
        .expect("template calls read local CONST and key path");
}

#[test]
fn uppercase_let_handle_display_round_trips() {
    // Uppercase `LET` handles keep their `$` sigil through
    // `Display`: the rendered steps re-parse without error.
    let script = "LET $TASK: HANDLE<INT> = ASYNC { RETURN 1 }\nAWAIT $TASK\n";
    let steps = oxdock_core::parse_script(script).expect("parse uppercase LET handle");
    let rendered = steps
        .iter()
        .map(|s| s.kind.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert!(
        rendered.contains("AWAIT $TASK"),
        "Display must keep the sigil, got: {rendered}"
    );
    oxdock_core::parse_script(&rendered).expect("re-parse rendered LET handle");
}

#[test]
fn func_body_same_frame_const_redeclare_fails() {
    // Same-frame rule inside `FUNC` bodies: two `CONST X` at the
    // function top level share the call frame with params, so the
    // second fails exactly like top-level redeclaration. A file
    // top-level `CONST X` is present to prove the error is the
    // same-frame double, not the legal shadowing of the file binding:
    // a single body-local `CONST X` shadows it cleanly.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $X: INT = 99
                FUNC F(): INT {
                    CONST $X: INT = 1
                    RETURN $X
                }
                LET $r: INT = F()
                ASSERT_EQ $r 1
                LET $held: INT = $X
                ASSERT_EQ $held 99
            "#},
        )
        .expect("single body-local CONST shadows file CONST");
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $X: INT = 99
                FUNC F(): INT {
                    CONST $X: INT = 1
                    CONST $X: INT = 2
                    RETURN $X
                }
                LET $r: INT = F()
            "#},
        )
        .expect_err("double CONST in one function frame must fail");
    assert!(
        format!("{err:#}").contains("redeclaration error"),
        "{err:#}"
    );
    assert!(format!("{err:#}").contains("CONST"), "{err:#}");
}

#[test]
fn func_body_nested_block_const_shadows_cleanly() {
    // Separate frames shadow cleanly across all three levels: file
    // `CONST X`, function-body `CONST X`, and `FOR`-body `CONST X`.
    // The innermost wins inside the loop, the function binding
    // decides after it, and the file binding survives the call.
    // (Bare guard blocks share the function frame statically, so
    // `FOR` is the nested scope that isolates here.)
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $X: INT = 100
                FUNC F(): INT {
                    CONST $X: INT = 1
                    LET $r: INT = 0
                    FOR $i: INT IN [7] {
                        CONST $X: INT = 10
                        $r = $X
                    }
                    RETURN $r + $X
                }
                LET $r: INT = F()
                ASSERT_EQ $r 11
                LET $held: INT = $X
                ASSERT_EQ $held 100
            "#},
        )
        .expect("nested-block CONST shadows function-body CONST");
}

#[test]
fn local_sigiled_const_coexists_with_alias_const() {
    // Namespace split: imported `CONST`s read only as `$ALIAS::NAME`
    // and never occupy the importer `var_scopes`, so a file
    // top-level `$CONST VERSION` coexists with `$LIB::VERSION`
    // at top level and inside a seeded `FUNC` body, which observes
    // both the seeded file binding and the qualified import.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            CONST $VERSION: STRING = "1.2.3"
            EXPORT CONST $VERSION
        "#},
    );
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                CONST $VERSION: STRING = "local"
                FUNC GETBOTH(): STRING {
                    RETURN "{{ $VERSION }}|{{ $LIB::VERSION }}"
                }
                LET $q: STRING = $LIB::VERSION
                ASSERT_EQ $q "1.2.3"
                ASSERT_EQ {{ $VERSION }} "local"
                LET $b: STRING = GETBOTH()
                ASSERT_EQ $b "local|1.2.3"
            "#},
        )
        .expect("local sigiled CONST coexists with alias CONST");
}

#[test]
fn pipe_talks_across_module_boundary() {
    // Pipes share their backend cell on clone, so a pipe crosses
    // the module boundary by value: as a FUNC argument or as a
    // copied `EXPORT CONST $PIPE`. `WITH_IO` bindings take a local
    // `$var`, so a qualified `LIB::P` must be copied first.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxdock",
        indoc! {r#"
            FUNC EMIT($p: PIPE): VOID {
                WITH_IO [stdout=$p] {
                    ECHO hello-from-lib
                }
            }
            CONST $P: PIPE
            EXPORT EMIT
            EXPORT CONST $P
        "#},
    );
    // Entry mints the pipe and hands it to the module: the module
    // writes, the entry reads back on the same backend.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                LET $p: PIPE
                LIB::EMIT($p)
                WITH_IO [stdin=$p] READ_LINE $line
                ASSERT_EQ $line "hello-from-lib"
            "#},
        )
        .expect("pipe passed as FUNC arg talks across the boundary");
    // The module mints the pipe and exports it as CONST: the entry
    // copies it to a local and both sides share the backend.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT ["lib.oxdock" AS LIB]
                LET $q: PIPE = $LIB::P
                WITH_IO [stdout=$q] {
                    ECHO hi-via-const-pipe
                }
                WITH_IO [stdin=$q] READ_LINE $line
                ASSERT_EQ $line "hi-via-const-pipe"
            "#},
        )
        .expect("exported CONST PIPE talks across the boundary");
}

#[test]
fn run_uses_const_via_template() {
    // `RUN` reads `CONST` only through `{{ }}` templates: the shell
    // line stays literal text otherwise, so `{{ $GREETING }}` expands
    // while bare words never do.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $GREETING: STRING = "hello-const"
                LET $out: STRING = RUN echo "{{ $GREETING }}"
                ASSERT_CONTAINS $out "hello-const"
            "#},
        )
        .expect("RUN expands CONST through {{ }}");
}

#[test]
fn run_bare_uppercase_stays_literal() {
    // Bare uppercase in `RUN` is shell text, never a binding read:
    // raw `SECRET` survives verbatim instead of resolving.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $SECRET: STRING = "s3cr3t-value"
                LET $out: STRING = RUN echo SECRET
                ASSERT_CONTAINS $out "SECRET"
            "#},
        )
        .expect("bare uppercase in RUN stays literal");
    // And the const value itself never leaks through the raw spelling:
    // only the `{{ }}` form reads it. `CONTAINS` (not exact `EQ`)
    // because Windows shells terminate the line with CRLF while Unix
    // uses LF: the literal survives verbatim on both, which is the
    // property under test. (If the bare word evaluated the const,
    // the output would be the secret value, which lacks `SECRET`.)
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $SECRET: STRING = "s3cr3t-value"
                LET $out: STRING = RUN echo SECRET
                ASSERT_CONTAINS $out "SECRET"
            "#},
        )
        .expect("raw bare does not evaluate the const");
}

#[test]
fn echo_spellings_read_distinct_stores() {
    // `$var`, `env:KEY`, bare words, and `{{ $NAME }}` are four
    // distinct reads: LET or CONST binding, script env, literal
    // text, and CONST binding. Bare never touches any binding.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                LET $note: STRING = "let-hello"
                ENV DOC_MOOD = "env-hello"
                CONST $BADGE: STRING = "const-hello"
                ECHO $note
                ASSERT_CONTAINS stdout "let-hello"
                ECHO env:DOC_MOOD
                ASSERT_CONTAINS stdout "env-hello"
                ECHO BADGE
                ASSERT_CONTAINS stdout "BADGE"
                ECHO "{{ $BADGE }}"
                ASSERT_CONTAINS stdout "const-hello"
                LET $raw: STRING = RUN echo BADGE
                ASSERT_CONTAINS $raw "BADGE"
                LET $cooked: STRING = RUN echo "{{ $BADGE }}"
                ASSERT_CONTAINS $cooked "const-hello"
            "#},
        )
        .expect("ECHO spellings read distinct stores");
}
