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

#[test]
fn const_readline_and_prompt_refuse_const() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // `READ_LINE`/`PROMPT` declare when absent but mutate when
    // present: onto a `CONST` both must refuse, statically (new
    // `is_const` gates) and at runtime (`mutate_var`).
    for script in [
        indoc! {r#"
            CONST $x: STRING = "a"
            READ_LINE $x
        "#},
        indoc! {r#"
            CONST $x: STRING = "a"
            PROMPT $x "name?"
        "#},
    ] {
        let err = Engine::new()
            .run_script(&root, script)
            .expect_err("READ_LINE/PROMPT onto a CONST must fail");
        assert!(
            format!("{err:#}").contains("cannot mutate CONST $x"),
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
            "CONST $t: HANDLE<INT> = ASYNC { RETURN 1 }",
            "LET $t: HANDLE<INT> = ASYNC { RETURN 2 }",
        ),
        (
            "LET $t: HANDLE<INT> = ASYNC { RETURN 1 }",
            "CONST $t: HANDLE<INT> = ASYNC { RETURN 2 }",
        ),
        (
            "CONST $t: HANDLE<INT> = ASYNC { RETURN 1 }",
            "CONST $t: HANDLE<INT> = ASYNC { RETURN 2 }",
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
        format!("{err:#}").contains("cannot mutate CONST $X"),
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
            EXPORT CONST N
        "#},
    );
    // `$ALIAS::NAME` rides every expression position a bare
    // variable does: math operands, call arguments, templates.
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
    // (Separate blocks: same-frame duplicate aliases reject at
    // parse, but block-scoped imports under one alias merge into
    // overlapping targets, which resolve loud.)
    write_lib(
        &root,
        "a.oxdock",
        indoc! {r#"
            CONST $X: INT = 1
            EXPORT CONST X
        "#},
    );
    write_lib(
        &root,
        "b.oxdock",
        indoc! {r#"
            CONST $X: INT = 2
            EXPORT CONST X
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                LET $go: BOOL = true
                IF $go {
                    IMPORT ["a.oxdock" AS LIB]
                }
                IF $go {
                    IMPORT ["b.oxdock" AS LIB]
                }
                LET $x: INT = $LIB::X
            "#},
        )
        .expect_err("ambiguous CONST must fail");
    assert!(format!("{err:#}").contains("ambiguous"), "{err:#}");
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
                    CONST $c: INT = 5
                    RETURN $c
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
                    CONST $c: INT = 5
                    $c = 6
                    RETURN $c
                }
                LET $r: INT = BAD()
            "#},
        )
        .expect_err("FUNC-local CONST mutation must fail");
    assert!(
        format!("{err:#}").contains("cannot mutate CONST $c"),
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
                CONST $i: INT = 99
                FOR $i: INT IN [1, 2, 3] {
                    ECHO $i
                }
                ASSERT_EQ $i 99
            "#},
        )
        .expect("loop var shadows outer CONST");
    // Iterating a `CONST` list reads every element.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $items: LIST<STRING> = ["a", "b"]
                FOR $s: STRING IN $items {
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
                LET $x: INT = 1
                [bool:true] {
                    CONST $x: INT = 10
                    ASSERT_EQ $x 10
                }
                ASSERT_EQ $x 1
            "#},
        )
        .expect("inner CONST shadows outer LET");
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                LET $x: INT = 1
                [bool:true] {
                    CONST $x: INT = 10
                    $x = 11
                }
            "#},
        )
        .expect_err("inner CONST mutation must fail");
    assert!(
        format!("{err:#}").contains("cannot mutate CONST $x"),
        "{err:#}"
    );
}

#[test]
fn const_bare_pipe_and_control_flow_reads() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // Bare `CONST $p: PIPE` mints a pipe like `LET`, and `CONST`
    // values read in every condition position.
    Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                CONST $p: PIPE
                WITH_IO [stdout=$p] {
                    ECHO hi
                }
                WITH_IO [stdin=$p] READ_LINE $line
                ASSERT_EQ $line "hi"
                CONST $mode: STRING = "go"
                CONST $limit: INT = 2
                LET $n: INT = 0
                IF $mode == "go" {
                    WRITE marker.txt "branched"
                }
                SWITCH $mode {
                    CASE "go" {
                        APPEND marker.txt "switched"
                    }
                }
                WHILE $n < $limit {
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
                CONST $s: STRING = "a"
                LET $copy: STRING = $s
                $copy = "b"
                ASSERT_EQ $copy "b"
                ASSERT_EQ $s "a"
            "#},
        )
        .expect("scalar CONST alias rebinds, original holds");
}
