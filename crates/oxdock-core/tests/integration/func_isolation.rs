//! Callee isolation (#322): the only thing crossing a `FUNC`
//! boundary is explicit arguments. Same module or across modules,
//! no distinction: no ambient `WORKSPACE`, `WORKDIR`, `ENV`, or
//! outer-variable leakage. Every test below pins the negative
//! (ambient state does not flow in, callee mutations do not flow
//! out) plus the positive (explicit arguments do).

use indoc::indoc;
use oxdock_core::{Engine, SNAPSHOT_PENDING_DISPLAY, parse_script};
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver};

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn write_lib(root: &GuardedPath, rel: &str, contents: &str) {
    let resolver = PathResolver::new(root.as_path(), root.as_path()).unwrap();
    let path = root.join(rel).unwrap();
    resolver.write_file(&path, contents.as_bytes()).unwrap();
}

fn lazy_bindings(
    root: &GuardedPath,
    script: &str,
) -> std::collections::BTreeMap<String, oxdock_core::Value> {
    let steps = parse_script(script).unwrap();
    Engine::new().run_steps(root, &steps).unwrap().bindings
}

#[test]
fn same_module_caller_workspace_never_reaches_callee() {
    // Caller selects LOCAL and moves; the callee still reports the
    // pending snapshot jail. Chained through MID so no caller in the
    // chain can smuggle its workspace in.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let bindings = lazy_bindings(
        &root,
        indoc! {r#"
            IMPORT [STD]
            FUNC INNER(): STRING {
                LET $c: STRING = CWD()
                RETURN $c
            }
            FUNC MID(): STRING {
                WORKSPACE LOCAL
                RETURN INNER()
            }
            WORKSPACE LOCAL
            MKDIR sub
            WORKDIR sub
            LET $caller: STRING = CWD()
            LET $callee: STRING = MID()
        "#},
    );
    let caller = bindings.get("caller").and_then(|v| v.as_str()).unwrap();
    let callee = bindings.get("callee").and_then(|v| v.as_str()).unwrap();
    assert!(caller.ends_with("sub"), "caller really moved: {caller}");
    assert_eq!(
        callee, SNAPSHOT_PENDING_DISPLAY,
        "callee stays in the snapshot jail, got {callee}"
    );
}

#[test]
fn same_module_workdir_never_reaches_callee() {
    // Direct runner: snapshot and local collapse, but the subdir
    // offset must not leak. The callee reports the root.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                FUNC WHERE(): STRING {
                    LET $c: STRING = CWD()
                    RETURN $c
                }
                MKDIR sub
                WORKDIR sub
                LET $caller: STRING = CWD()
                LET $callee: STRING = WHERE()
            "#},
        )
        .expect("workdir isolation runs");
    let caller = out.bindings.get("caller").and_then(|v| v.as_str()).unwrap();
    let callee = out.bindings.get("callee").and_then(|v| v.as_str()).unwrap();
    assert!(caller.ends_with("sub"), "caller really moved: {caller}");
    assert_ne!(caller, callee, "callee must not inherit the caller workdir");
    assert!(
        !callee.ends_with("sub"),
        "callee stays at the jail root, got {callee}"
    );
}

#[test]
fn same_module_env_never_leaks_either_direction() {
    // Caller and callee declare the same name with different values.
    // Each observes its own, and the caller value survives the call.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                ENV ISO_PROBE=caller-val
                FUNC WHICH(): STRING {
                    ENV ISO_PROBE=callee-val
                    RETURN "{{ env:ISO_PROBE }}"
                }
                LET $got: STRING = WHICH()
                LET $after: STRING = "{{ env:ISO_PROBE }}"
            "#},
        )
        .expect("env isolation runs");
    assert_eq!(
        out.bindings.get("got").and_then(|v| v.as_str()),
        Some("callee-val"),
        "callee observes its own env"
    );
    assert_eq!(
        out.bindings.get("after").and_then(|v| v.as_str()),
        Some("caller-val"),
        "callee ENV must not leak back into the caller"
    );
}

#[test]
fn same_module_outer_var_never_reaches_callee() {
    // Reading an outer binding without a param fails statically.
    // Passing it explicitly is the supported path.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                LET $v: STRING = "outer"
                FUNC READIT(): STRING {
                    RETURN $v
                }
                LET $got: STRING = READIT()
            "#},
        )
        .expect_err("outer var must not leak into the callee");
    assert!(
        format!("{err:#}").contains("undefined variable"),
        "isolation must report undefined variable, got: {err:#}"
    );
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                LET $v: STRING = "outer"
                FUNC READIT($v: STRING): STRING {
                    RETURN $v
                }
                LET $got: STRING = READIT($v)
            "#},
        )
        .expect("explicit argument must pass");
    assert_eq!(
        out.bindings.get("got").and_then(|v| v.as_str()),
        Some("outer")
    );
}

#[test]
fn cross_module_workspace_and_workdir_never_reach_callee() {
    // Entry selects LOCAL and moves into sub; the lib callee still
    // reports the snapshot jail.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC WHERE]
            FUNC WHERE(): STRING {
                LET $c: STRING = CWD()
                RETURN $c
            }
        "#},
    );
    let steps = parse_script(indoc! {r#"
        IMPORT [STD, "lib.oxfile" AS LIB]
        WORKSPACE LOCAL
        MKDIR sub
        WORKDIR sub
        LET $caller: STRING = CWD()
        LET $callee: STRING = LIB::WHERE()
    "#})
    .unwrap();
    let bindings = Engine::new().run_steps(&root, &steps).unwrap().bindings;
    let caller = bindings.get("caller").and_then(|v| v.as_str()).unwrap();
    let callee = bindings.get("callee").and_then(|v| v.as_str()).unwrap();
    assert!(caller.ends_with("sub"), "caller really moved: {caller}");
    assert_eq!(
        callee, SNAPSHOT_PENDING_DISPLAY,
        "lib callee stays in the snapshot jail, got {callee}"
    );
}

#[test]
fn cross_module_env_never_leaks_either_direction() {
    // Entry and lib declare the same name independently. Neither
    // observes the other.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC WHICH, FUNC SETIT]
            FUNC WHICH(): STRING {
                ENV ISO_XMOD=lib-val
                RETURN "{{ env:ISO_XMOD }}"
            }
            FUNC SETIT(): STRING {
                ENV ISO_XMOD=lib-mutation
                RETURN "done"
            }
        "#},
    );
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "lib.oxfile" AS LIB]
                ENV ISO_XMOD=entry-val
                LET $got: STRING = LIB::WHICH()
                LET $probe: STRING = LIB::SETIT()
                LET $after: STRING = "{{ env:ISO_XMOD }}"
            "#},
        )
        .expect("cross-module env isolation runs");
    assert_eq!(
        out.bindings.get("got").and_then(|v| v.as_str()),
        Some("lib-val"),
        "lib observes its own env, not the entry"
    );
    assert_eq!(
        out.bindings.get("after").and_then(|v| v.as_str()),
        Some("entry-val"),
        "lib ENV mutation must not leak back into the entry"
    );
}

#[test]
fn cross_module_outer_var_never_reaches_callee_but_file_global_does() {
    // An entry binding is invisible inside the lib callee, while the
    // lib own file global stays visible. The entry value crosses only
    // as an explicit argument.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            LET $SEG: STRING = "lib-seg"
            EXPORT [FUNC SEGOF, FUNC ECHOIT]
            FUNC SEGOF(): STRING {
                RETURN $SEG
            }
            FUNC ECHOIT($v: STRING): STRING {
                RETURN $v
            }
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "lib.oxfile" AS LIB]
                LET $v: STRING = "entry-val"
                FUNC TRYREAD(): STRING {
                    RETURN LIB::ECHOIT($v)
                }
            "#},
        )
        .expect_err("entry wrapper reading outer without a param must fail");
    assert!(
        format!("{err:#}").contains("undefined variable"),
        "got: {err:#}"
    );
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "lib.oxfile" AS LIB]
                LET $v: STRING = "entry-val"
                FUNC PASSIT($v: STRING): STRING {
                    RETURN LIB::ECHOIT($v)
                }
                LET $seg: STRING = LIB::SEGOF()
                LET $echo: STRING = PASSIT($v)
            "#},
        )
        .expect("explicit arguments cross modules");
    assert_eq!(
        out.bindings.get("seg").and_then(|v| v.as_str()),
        Some("lib-seg"),
        "lib file global stays visible to lib bodies"
    );
    assert_eq!(
        out.bindings.get("echo").and_then(|v| v.as_str()),
        Some("entry-val"),
        "entry value crosses as an explicit argument"
    );
}

#[test]
fn nested_chain_never_inherits_anything() {
    // Three-deep chain across the module boundary: every frame sets
    // LOCAL plus its own workdir and env, yet the innermost callee
    // still reports the snapshot jail with its own env.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC MID, FUNC INNER]
            FUNC INNER(): STRING {
                ENV ISO_DEEP=inner-val
                LET $c: STRING = CWD()
                LET $e: STRING = "{{ env:ISO_DEEP }}"
                RETURN "{{ $c }}|{{ $e }}"
            }
            FUNC MID(): STRING {
                WORKSPACE LOCAL
                WORKDIR sub-mid
                ENV ISO_DEEP=mid-val
                RETURN INNER()
            }
        "#},
    );
    let steps = parse_script(indoc! {r#"
        IMPORT [STD, "lib.oxfile" AS LIB]
        WORKSPACE LOCAL
        MKDIR sub-mid
        MKDIR sub-top
        WORKDIR sub-top
        ENV ISO_DEEP=top-val
        FUNC OUTER(): STRING {
            WORKSPACE LOCAL
            RETURN LIB::MID()
        }
        LET $got: STRING = OUTER()
    "#})
    .unwrap();
    let bindings = Engine::new().run_steps(&root, &steps).unwrap().bindings;
    let got = bindings.get("got").and_then(|v| v.as_str()).unwrap();
    let (cwd, env) = got.split_once('|').expect("cwd|env shape");
    assert_eq!(
        cwd, SNAPSHOT_PENDING_DISPLAY,
        "innermost callee stays in the snapshot jail, got {cwd}"
    );
    assert_eq!(env, "inner-val", "innermost callee sees its own env");
}

#[test]
fn callee_mutation_never_leaks_back_to_caller() {
    // The callee switches workspace, moves, and sets env. After the
    // return the caller observes exactly what it set before.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                ENV ISO_BACK=before
                FUNC MUTATE(): STRING {
                    WORKSPACE LOCAL
                    MKDIR inner
                    WORKDIR inner
                    ENV ISO_BACK=mutated
                    LET $c: STRING = CWD()
                    RETURN $c
                }
                MKDIR outer
                WORKDIR outer
                LET $before: STRING = CWD()
                LET $inner_cwd: STRING = MUTATE()
                LET $after: STRING = CWD()
                LET $env_after: STRING = "{{ env:ISO_BACK }}"
            "#},
        )
        .expect("mutation isolation runs");
    let before = out.bindings.get("before").and_then(|v| v.as_str()).unwrap();
    let after = out.bindings.get("after").and_then(|v| v.as_str()).unwrap();
    assert_eq!(before, after, "caller workdir must survive the callee");
    assert_ne!(
        out.bindings.get("inner_cwd").and_then(|v| v.as_str()),
        Some(after),
        "callee really moved somewhere else"
    );
    assert_eq!(
        out.bindings.get("env_after").and_then(|v| v.as_str()),
        Some("before"),
        "callee ENV must not leak back"
    );
}

#[test]
fn cross_module_callee_own_workspace_declaration_wins() {
    // The lib callee declares its own jail: WORKSPACE LOCAL resolves
    // to the shared local root even though the caller sits in a
    // subdir of it. The caller offset never leaks in.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC OWNLOCAL]
            FUNC OWNLOCAL(): STRING {
                WORKSPACE LOCAL
                LET $c: STRING = CWD()
                RETURN $c
            }
        "#},
    );
    let steps = parse_script(indoc! {r#"
        IMPORT [STD, "lib.oxfile" AS LIB]
        WORKSPACE LOCAL
        LET $local_root: STRING = CWD()
        MKDIR entry-sub
        WORKDIR entry-sub
        LET $caller: STRING = CWD()
        LET $callee: STRING = LIB::OWNLOCAL()
    "#})
    .unwrap();
    let bindings = Engine::new().run_steps(&root, &steps).unwrap().bindings;
    let local_root = bindings.get("local_root").and_then(|v| v.as_str()).unwrap();
    let caller = bindings.get("caller").and_then(|v| v.as_str()).unwrap();
    let callee = bindings.get("callee").and_then(|v| v.as_str()).unwrap();
    assert!(
        caller.ends_with("entry-sub"),
        "caller really moved: {caller}"
    );
    assert_eq!(
        callee, local_root,
        "callee own WORKSPACE LOCAL resolves to the local root, got {callee}"
    );
}

#[test]
fn cross_module_callee_default_jail_ignores_caller_system() {
    // Strongest boundary: the caller selects SYSTEM (full host
    // access, lexical only, no writes) and the lib callee with no
    // workspace statement still reports the snapshot jail.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC SNAPOF]
            FUNC SNAPOF(): STRING {
                LET $c: STRING = CWD()
                RETURN $c
            }
        "#},
    );
    let steps = parse_script(indoc! {r#"
        IMPORT [STD, "lib.oxfile" AS LIB]
        WORKSPACE SYSTEM
        LET $caller: STRING = CWD()
        LET $callee: STRING = LIB::SNAPOF()
    "#})
    .unwrap();
    let bindings = Engine::new().run_steps(&root, &steps).unwrap().bindings;
    let caller = bindings.get("caller").and_then(|v| v.as_str()).unwrap();
    let callee = bindings.get("callee").and_then(|v| v.as_str()).unwrap();
    assert_eq!(
        callee, SNAPSHOT_PENDING_DISPLAY,
        "lib callee defaults to the snapshot jail, got {callee}"
    );
    assert_ne!(
        caller, callee,
        "caller SYSTEM ({caller}) must not leak into the callee"
    );
}

#[test]
fn cross_module_module_top_level_context_reaches_its_funcs() {
    // The module top-level establishes the callee jail: WORKSPACE
    // LOCAL plus WORKDIR, ENV, and LET at the top flow into every
    // FUNC of that file, no matter which workspace the caller sits
    // in. The caller conflicting values never appear inside.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            WORKSPACE LOCAL
            MKDIR modsub
            WORKDIR modsub
            ENV MODCTX=lib-val
            LET $G: STRING = "lib-global"
            EXPORT [FUNC CTX]
            FUNC CTX(): STRING {
                LET $c: STRING = CWD()
                LET $e: STRING = "{{ env:MODCTX }}"
                RETURN "{{ $c }}|{{ $e }}|{{ $G }}"
            }
        "#},
    );
    let steps = parse_script(indoc! {r#"
        IMPORT [STD, "lib.oxfile" AS LIB]
        WORKSPACE LOCAL
        LET $local_root: STRING = CWD()
        MKDIR entry-sub
        WORKDIR entry-sub
        ENV MODCTX=entry-val
        LET $caller: STRING = CWD()
        LET $got: STRING = LIB::CTX()
    "#})
    .unwrap();
    let bindings = Engine::new().run_steps(&root, &steps).unwrap().bindings;
    let local_root = bindings.get("local_root").and_then(|v| v.as_str()).unwrap();
    let caller = bindings.get("caller").and_then(|v| v.as_str()).unwrap();
    let got = bindings.get("got").and_then(|v| v.as_str()).unwrap();
    assert!(
        caller.ends_with("entry-sub"),
        "caller really moved: {caller}"
    );
    let (cwd, rest) = got.split_once('|').expect("cwd|env|global shape");
    let (env, global) = rest.split_once('|').expect("env|global shape");
    assert_eq!(env, "lib-val", "callee sees the module top-level ENV");
    assert_eq!(global, "lib-global", "callee sees the module top-level LET");
    assert!(
        cwd.ends_with("modsub"),
        "callee runs in the module top-level WORKDIR, got {cwd}"
    );
    assert_ne!(cwd, caller, "caller workdir must not leak into the callee");
    assert!(
        cwd.starts_with(local_root),
        "callee stays under the module LOCAL root {local_root}, got {cwd}"
    );
}

#[test]
fn cross_module_func_level_workspace_overrides_module_top() {
    // Unless the called function overrides it: a FUNC-level
    // WORKSPACE statement wins over the module top-level for that
    // call, and the caller still sees its own context afterwards.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            WORKSPACE LOCAL
            EXPORT [FUNC BACKTOSNAP]
            FUNC BACKTOSNAP(): STRING {
                WORKSPACE SNAPSHOT
                LET $c: STRING = CWD()
                RETURN $c
            }
        "#},
    );
    let steps = parse_script(indoc! {r#"
        IMPORT [STD, "lib.oxfile" AS LIB]
        WORKSPACE LOCAL
        LET $caller: STRING = CWD()
        LET $callee: STRING = LIB::BACKTOSNAP()
        LET $after: STRING = CWD()
    "#})
    .unwrap();
    let bindings = Engine::new().run_steps(&root, &steps).unwrap().bindings;
    let caller = bindings.get("caller").and_then(|v| v.as_str()).unwrap();
    let callee = bindings.get("callee").and_then(|v| v.as_str()).unwrap();
    let after = bindings.get("after").and_then(|v| v.as_str()).unwrap();
    assert_eq!(
        callee, SNAPSHOT_PENDING_DISPLAY,
        "func-level WORKSPACE overrides the module top-level, got {callee}"
    );
    assert_eq!(caller, after, "caller workspace must survive the call");
}

#[test]
fn harvested_func_observes_module_top_level_jail() {
    // Harvests run the module init when its top-level sets a jail
    // (#322): the harvested FUNC serves from the module LOCAL jail
    // exactly like a whole-file import, not the snapshot default.
    // Modules without a top-level jail skip init and stay
    // side-effect free (see `file_import_from_bare_harvest_skips_body`).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            WORKSPACE LOCAL
            MKDIR hsub
            WORKDIR hsub
            EXPORT [FUNC HWHERE]
            FUNC HWHERE(): STRING {
                LET $c: STRING = CWD()
                RETURN $c
            }
        "#},
    );
    let steps = parse_script(indoc! {r#"
        IMPORT [STD, [FUNC HWHERE FROM "lib.oxfile"]]
        LET $callee: STRING = HWHERE()
    "#})
    .unwrap();
    let bindings = Engine::new().run_steps(&root, &steps).unwrap().bindings;
    let callee = bindings.get("callee").and_then(|v| v.as_str()).unwrap();
    assert!(
        callee.ends_with("hsub"),
        "harvested FUNC serves from the module top-level WORKDIR, got {callee}"
    );
    assert_ne!(
        callee, SNAPSHOT_PENDING_DISPLAY,
        "harvest must not fall back to the snapshot default"
    );
}

#[test]
fn cross_module_explicit_args_reach_callee_jail() {
    // Only arguments cross: the entry passes a dir name plus string
    // data, and the lib callee applies them inside its own jail with
    // a write/read roundtrip.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC SINK]
            FUNC SINK($dir: STRING, $data: STRING): STRING {
                WORKDIR $dir
                WRITE got.txt $data
                LET $c: STRING = READ "got.txt"
                RETURN $c
            }
        "#},
    );
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "lib.oxfile" AS LIB]
                LET $payload: STRING = "entry-data"
                LET $got: STRING = LIB::SINK("drop", $payload)
            "#},
        )
        .expect("explicit cross-module args must reach the callee");
    assert_eq!(
        out.bindings.get("got").and_then(|v| v.as_str()),
        Some("entry-data")
    );
}

#[test]
fn explicit_path_and_value_args_reach_callee() {
    // The caller passes a relative dir name plus string data. The
    // callee applies them inside its own jail: no ambient path, env,
    // or variable flows, only arguments.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                ENV ISO_SEND=entry-env
                LET $payload: STRING = "entry-data"
                FUNC SINK($dir: STRING, $data: STRING, $env_val: STRING): STRING {
                    ENV ISO_SEND=$env_val
                    WORKDIR $dir
                    WRITE got.txt $data
                    LET $c: STRING = READ "got.txt"
                    LET $e: STRING = "{{ env:ISO_SEND }}"
                    RETURN "{{ $c }}|{{ $e }}"
                }
                LET $got: STRING = SINK("drop", $payload, "{{ env:ISO_SEND }}")
            "#},
        )
        .expect("explicit args must reach the callee");
    assert_eq!(
        out.bindings.get("got").and_then(|v| v.as_str()),
        Some("entry-data|entry-env")
    );
}
