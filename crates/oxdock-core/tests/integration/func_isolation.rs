//! Callee isolation (#322): the only thing crossing a `FUNC`
//! boundary is explicit arguments. Same module or across modules,
//! no distinction: no ambient `WORKSPACE`, `WORKDIR`, `ENV`, or
//! outer-variable leakage. Every test below pins the negative
//! (ambient state does not flow in, callee mutations do not flow
//! out) plus the positive (explicit arguments do).

use indoc::indoc;
use oxdock_core::{Engine, ExecIo, SNAPSHOT_PENDING_DISPLAY, parse_script};
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
fn cross_module_system_caller_with_shadowed_state_stays_isolated() {
    // The ticket proof in one call (#322): caller sits in WORKSPACE
    // SYSTEM with a custom workdir, a conflicting ENV, and a
    // shadowing local; the lib callee still reports its own
    // declared root/dir/env/values. SYSTEM keeps cwd (rebased), so
    // the MKDIR/WORKDIR setup happens under LOCAL first and only
    // reads run under SYSTEM.
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
            ENV COMBINED=lib-val
            LET $G: STRING = "lib-global"
            EXPORT [FUNC CTX]
            FUNC CTX(): STRING {
                LET $c: STRING = CWD()
                LET $e: STRING = "{{ env:COMBINED }}"
                RETURN "{{ $c }}|{{ $e }}|{{ $G }}"
            }
        "#},
    );
    let steps = parse_script(indoc! {r#"
        IMPORT [STD, "lib.oxfile" AS LIB]
        WORKSPACE LOCAL
        MKDIR entry-sub
        WORKDIR entry-sub
        ENV COMBINED=entry-val
        LET $G: STRING = "entry-global"
        WORKSPACE SYSTEM
        LET $caller: STRING = CWD()
        LET $got: STRING = LIB::CTX()
        LET $after: STRING = "{{ env:COMBINED }}"
    "#})
    .unwrap();
    let bindings = Engine::new().run_steps(&root, &steps).unwrap().bindings;
    let caller = bindings.get("caller").and_then(|v| v.as_str()).unwrap();
    let got = bindings.get("got").and_then(|v| v.as_str()).unwrap();
    assert!(
        caller.ends_with("entry-sub"),
        "caller really holds its custom cwd under SYSTEM: {caller}"
    );
    let (cwd, rest) = got.split_once('|').expect("cwd|env|global shape");
    let (env, global) = rest.split_once('|').expect("env|global shape");
    assert_eq!(env, "lib-val", "caller ENV must not leak into callee");
    assert_eq!(
        global, "lib-global",
        "caller shadowing local must not leak into callee"
    );
    assert!(
        cwd.ends_with("modsub"),
        "callee runs in its own declared WORKDIR, got {cwd}"
    );
    assert_ne!(cwd, caller, "caller SYSTEM cwd must not leak into callee");
    assert_eq!(
        bindings.get("after").and_then(|v| v.as_str()),
        Some("entry-val"),
        "callee must not leak its ENV back into the SYSTEM caller"
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
fn harvested_func_observes_guarded_top_level_jail() {
    // Guards only skip, never push a scope, so a conditional
    // top-level jail set through `[env:...]` persists to init-final
    // and reaches harvested FUNCs. Contrast with the `IF`-block
    // case above, which unwinds on block exit.
    let _flag = oxdock_process::TestEnvGuard::set("OXDOCK_JAIL_FLAG", "1");
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            INHERIT_ENV [OXDOCK_JAIL_FLAG]
            [env:OXDOCK_JAIL_FLAG] WORKSPACE LOCAL
            [env:OXDOCK_JAIL_FLAG] MKDIR gsub
            [env:OXDOCK_JAIL_FLAG] WORKDIR gsub
            EXPORT [FUNC GWHERE]
            FUNC GWHERE(): STRING {
                LET $c: STRING = CWD()
                RETURN $c
            }
        "#},
    );
    let steps = parse_script(indoc! {r#"
        IMPORT [STD, [FUNC GWHERE FROM "lib.oxfile"]]
        LET $callee: STRING = GWHERE()
    "#})
    .unwrap();
    let bindings = Engine::new().run_steps(&root, &steps).unwrap().bindings;
    let callee = bindings.get("callee").and_then(|v| v.as_str()).unwrap();
    assert!(
        callee.ends_with("gsub"),
        "guard-gated module jail persists to harvested FUNCs, got {callee}"
    );
}

#[test]
fn harvested_func_runs_init_for_conditional_top_level_jail() {
    // The classifier walks control-flow blocks: a conditional
    // `IF ... { WORKSPACE LOCAL }` at the module top-level still
    // triggers init on harvest. Block scoping unwinds the selection
    // on IF exit (like every block), so the restored callee jail is
    // the snapshot default; the proof that init ran is its surviving
    // filesystem effect, visible from an explicit `WORKSPACE LOCAL`
    // in the entry.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            LET $flag: BOOL = true
            IF $flag {
                WORKSPACE LOCAL
                WRITE csub-marker.txt "conditional-init-ran"
            }
            EXPORT [FUNC CWHERE]
            FUNC CWHERE(): STRING {
                LET $c: STRING = CWD()
                RETURN $c
            }
        "#},
    );
    let steps = parse_script(indoc! {r#"
        IMPORT [STD, [FUNC CWHERE FROM "lib.oxfile"]]
        LET $callee: STRING = CWHERE()
        WORKSPACE LOCAL
        LET $hits: LIST<STRING> = GLOB("csub-marker.txt")
        LET $n: INT = LEN($hits)
    "#})
    .unwrap();
    let bindings = Engine::new().run_steps(&root, &steps).unwrap().bindings;
    assert_eq!(
        bindings.get("n").and_then(|v| v.as_i64()),
        Some(1),
        "conditional top-level jail must trigger init on harvest"
    );
    assert_eq!(
        bindings.get("callee").and_then(|v| v.as_str()),
        Some(SNAPSHOT_PENDING_DISPLAY),
        "block-scoped WORKSPACE unwinds on IF exit, so the callee keeps the snapshot default"
    );
}

#[test]
fn harvested_func_ignores_nested_funcdef_jail() {
    // `WORKSPACE` inside a nested `FUNC` body never counts: it runs
    // in that function call jail, never the init flow. Harvesting an
    // unrelated pure FUNC skips init entirely, so the top-level
    // side effect never runs.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            WRITE side.txt "effect"
            EXPORT [FUNC PURE, FUNC INNER]
            FUNC INNER(): STRING {
                WORKSPACE LOCAL
                RETURN "inner"
            }
            FUNC PURE($x: STRING): STRING {
                RETURN $x
            }
        "#},
    );
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, [FUNC PURE FROM "lib.oxfile"]]
                LET $v: STRING = PURE("ok")
                LET $hits: LIST<STRING> = GLOB("side.txt")
                LET $n: INT = LEN($hits)
            "#},
        )
        .expect("harvest runs");
    assert_eq!(out.bindings.get("v").and_then(|v| v.as_str()), Some("ok"));
    assert_eq!(
        out.bindings.get("n").and_then(|v| v.as_i64()),
        Some(0),
        "harvest must not execute the module body"
    );
}

#[test]
fn nested_funcdef_does_not_inherit_enclosing_call_jail() {
    // A `FUNC` defined inside another `FUNC` body still enters
    // through `call_func_value`, so it resets to the definition-site
    // jail like any other callee: the enclosing call frame workspace,
    // workdir, and env never leak into it, and there is no breakout
    // path around the boundary.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let steps = parse_script(indoc! {r#"
        IMPORT [STD]
        FUNC OUTER(): STRING {
            WORKSPACE LOCAL
            MKDIR outer-sub
            WORKDIR outer-sub
            ENV NESTED_PROBE=outer-val
            FUNC INNER(): STRING {
                LET $c: STRING = CWD()
                RETURN $c
            }
            RETURN INNER()
        }
        LET $got: STRING = OUTER()
    "#})
    .unwrap();
    let bindings = Engine::new().run_steps(&root, &steps).unwrap().bindings;
    assert_eq!(
        bindings.get("got").and_then(|v| v.as_str()),
        Some(SNAPSHOT_PENDING_DISPLAY),
        "nested callee resets to the snapshot default despite the enclosing LOCAL jail"
    );
}

#[test]
fn cross_module_same_name_internal_call_hits_own_def() {
    // Confusion proof (#322): entry and lib define the same
    // function name with the same arity but different bodies, and
    // the lib keeps its helper private. The lib internal unqualified
    // call must resolve home-scope to its own def; the entry bare
    // call must resolve to its own. Same arity is the dangerous
    // shape: a wrong resolution would silently return the other
    // party's data with no error.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC CALLIT]
            FUNC FOO(): STRING {
                RETURN "lib-foo"
            }
            FUNC CALLIT(): STRING {
                RETURN FOO()
            }
        "#},
    );
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "lib.oxfile" AS LIB]
                FUNC FOO(): STRING {
                    RETURN "entry-foo"
                }
                LET $internal: STRING = LIB::CALLIT()
                LET $own: STRING = FOO()
            "#},
        )
        .expect("same-name shadowing runs");
    assert_eq!(
        out.bindings.get("internal").and_then(|v| v.as_str()),
        Some("lib-foo"),
        "lib internal unqualified call must hit the lib def, never the entry shadow"
    );
    assert_eq!(
        out.bindings.get("own").and_then(|v| v.as_str()),
        Some("entry-foo"),
        "entry bare call still hits the entry def"
    );
}

#[test]
fn mismatched_shadow_validates_against_home_signature() {
    // Static/runtime agreement proof (#322): the entry defines a
    // same-named function with a DIFFERENT signature (one arg vs
    // none). Validation of the lib internal call must use the lib's
    // own 0-arg signature, not the entry's 1-arg one.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC CALLIT]
            FUNC FOO(): STRING {
                RETURN "lib-foo"
            }
            FUNC CALLIT(): STRING {
                RETURN FOO()
            }
        "#},
    );
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "lib.oxfile" AS LIB]
                FUNC FOO($x: INT): INT {
                    RETURN $x
                }
                LET $internal: STRING = LIB::CALLIT()
            "#},
        )
        .expect("module internal call validates against the home signature");
    assert_eq!(
        out.bindings.get("internal").and_then(|v| v.as_str()),
        Some("lib-foo"),
        "runtime must serve the lib def"
    );
}

#[test]
fn bare_harvested_caller_still_hits_lib_def() {
    // Harvest variant of the confusion proof: only `CALLIT` is
    // imported, called bare with no alias prefix, while the entry
    // defines its own same-named private `FOO`. The harvested body
    // must still resolve its internal call home-scope to the lib def.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC CALLIT]
            FUNC FOO(): STRING {
                RETURN "lib-foo"
            }
            FUNC CALLIT(): STRING {
                RETURN FOO()
            }
        "#},
    );
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, [FUNC CALLIT FROM "lib.oxfile"]]
                FUNC FOO(): STRING {
                    RETURN "entry-foo"
                }
                LET $internal: STRING = CALLIT()
                LET $own: STRING = FOO()
            "#},
        )
        .expect("bare harvest runs");
    assert_eq!(
        out.bindings.get("internal").and_then(|v| v.as_str()),
        Some("lib-foo"),
        "harvested body must hit the lib def, never the entry shadow"
    );
    assert_eq!(
        out.bindings.get("own").and_then(|v| v.as_str()),
        Some("entry-foo"),
        "entry bare call still hits the entry def"
    );
}

#[test]
fn module_nested_shadow_beats_top_level_home_def() {
    // Precedence proof (#322): a nested `FUNC` inside a module body
    // shadows the module's own top-level def of the same name for
    // calls within that body. Deliberately DIFFERENT signatures
    // (nested takes an arg, top-level takes none) so static and
    // runtime must both resolve the nested def: validating against
    // the top-level would false-reject, executing it would take the
    // wrong body.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC CALLIT, FUNC CALLTOP]
            FUNC FOO(): STRING {
                RETURN "top"
            }
            FUNC CALLIT(): STRING {
                FUNC FOO($x: INT): STRING {
                    RETURN "nested"
                }
                RETURN FOO(41)
            }
            FUNC CALLTOP(): STRING {
                RETURN FOO()
            }
        "#},
    );
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "lib.oxfile" AS LIB]
                LET $got: STRING = LIB::CALLIT()
                LET $top: STRING = LIB::CALLTOP()
            "#},
        )
        .expect("nested shadow runs");
    assert_eq!(
        out.bindings.get("got").and_then(|v| v.as_str()),
        Some("nested"),
        "innermost nested def wins over the module top-level def"
    );
    assert_eq!(
        out.bindings.get("top").and_then(|v| v.as_str()),
        Some("top"),
        "outside the nested scope the home top-level def still serves"
    );
}

#[test]
fn entry_bare_call_to_foreign_only_def_fails_closed() {
    // No silent cross-unit execution (#322): the entry never
    // imported `FOO` (no alias use, no harvest). The parser
    // qualifies the bare call to the sole opaque alias and execution
    // fails closed instead of running the lib's private def.
    // Same-unit and explicitly imported spellings are unaffected.
    // (Explicit `SCRIPT::` spellings never reach runtime: the
    // parser rejects unknown `SCRIPT::` modules up front.)
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            FUNC FOO(): STRING {
                RETURN "lib-foo"
            }
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "lib.oxfile" AS LIB]
                LET $got: STRING = FOO()
            "#},
        )
        .expect_err("bare call to a foreign-only def must fail");
    assert!(
        format!("{err:#}").contains("unknown function `LIB::FOO`"),
        "fails closed on the qualified spelling, got: {err:#}"
    );
}

#[test]
fn module_cannot_reach_caller_nested_def() {
    // Lexical scoping across calls (#322): a nested `FUNC` defined
    // in the caller's body is invisible inside the callee, even in
    // the same file. Only the callee's own nested defs, its file
    // top-level, and its explicit imports resolve there.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "lib.oxfile",
        indoc! {r#"
            IMPORT [STD]
            EXPORT [FUNC OUTER, FUNC INNER]
            FUNC OUTER(): STRING {
                FUNC FOO(): STRING {
                    RETURN "caller-nested"
                }
                RETURN INNER()
            }
            FUNC INNER(): STRING {
                RETURN FOO()
            }
        "#},
    );
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "lib.oxfile" AS LIB]
                LET $got: STRING = LIB::OUTER()
            "#},
        )
        .expect_err("callee must not see the caller nested def");
    assert!(
        format!("{err:#}").contains("unknown function `FOO`"),
        "fails closed naming the function, got: {err:#}"
    );
}

#[test]
fn entry_caller_nested_def_invisible_to_sibling_callee() {
    // Same rule, entry edition (#322): a nested `FUNC` defined in an
    // entry caller's body is invisible inside the sibling callee,
    // even though both live in the entry file. The callee's own
    // nested defs, the entry top-level index, and explicit imports
    // resolve there; the caller's frames do not.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                FUNC OUTER(): STRING {
                    FUNC FOO(): STRING {
                        RETURN "caller-nested"
                    }
                    RETURN INNER()
                }
                FUNC INNER(): STRING {
                    RETURN FOO()
                }
                LET $got: STRING = OUTER()
            "#},
        )
        .expect_err("sibling callee must not see the caller nested def");
    assert!(
        format!("{err:#}").contains("unknown function `FOO`"),
        "fails closed naming the function, got: {err:#}"
    );
}

#[test]
fn entry_forward_reference_executes() {
    // Alignment (#322): the static pass already validated forward
    // calls; the runtime file frame serves them instead of bailing
    // unknown-function. Matches module behavior, where the home
    // index always resolved regardless of definition order.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let out = Engine::new()
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                LET $got: STRING = FOO()
                FUNC FOO(): STRING {
                    RETURN "hi"
                }
            "#},
        )
        .expect("entry forward reference runs");
    assert_eq!(
        out.bindings.get("got").and_then(|v| v.as_str()),
        Some("hi"),
        "entry top-level defs resolve regardless of position"
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
fn cross_module_inherit_env_stays_independent() {
    // Modules inherit from the engine, never each other (#322
    // follow-up): A declares MOD_A_TOKEN, B declares MOD_B_TOKEN.
    // A calls B::GET_VALUE and gets B's engine value back even
    // though A never inherited it; A reading B's name directly
    // fails closed. Neither callee observes caller env, only its
    // own file prelude plus engine re-reads.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    write_lib(
        &root,
        "b.oxfile",
        indoc! {r#"
            IMPORT [STD]
            INHERIT_ENV [MOD_B_TOKEN]
            EXPORT [FUNC GET_VALUE]
            FUNC GET_VALUE(): STRING {
                RETURN "{{ env:MOD_B_TOKEN }}"
            }
        "#},
    );
    write_lib(
        &root,
        "a.oxfile",
        indoc! {r#"
            INHERIT_ENV [MOD_A_TOKEN]
            IMPORT [STD, "b.oxfile" AS B]
            EXPORT [FUNC CALL_B, FUNC GET_OWN]
            FUNC GET_OWN(): STRING {
                RETURN "{{ env:MOD_A_TOKEN }}"
            }
            FUNC CALL_B(): STRING {
                RETURN B::GET_VALUE()
            }
        "#},
    );
    let mut io = ExecIo::new();
    io.insert_inherit_env("MOD_A_TOKEN", "aye-value");
    io.insert_inherit_env("MOD_B_TOKEN", "bee-value");
    let out = Engine::new()
        .with_io(io.clone())
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "a.oxfile" AS A]
                LET $via_a: STRING = A::CALL_B()
                LET $own: STRING = A::GET_OWN()
            "#},
        )
        .expect("cross-module inherit runs");
    assert_eq!(
        out.bindings.get("via_a").and_then(|v| v.as_str()),
        Some("bee-value"),
        "A serves B's engine value without inheriting it"
    );
    assert_eq!(
        out.bindings.get("own").and_then(|v| v.as_str()),
        Some("aye-value"),
        "A still serves its own engine value"
    );
    // Negative direction: A reading B's name directly fails closed,
    // proving A never inherited it. Separate file so the positive
    // closure above validates clean.
    write_lib(
        &root,
        "a.oxfile",
        indoc! {r#"
            INHERIT_ENV [MOD_A_TOKEN]
            IMPORT [STD, "b.oxfile" AS B]
            EXPORT [FUNC TRY_READ_B]
            FUNC TRY_READ_B(): STRING {
                RETURN "{{ env:MOD_B_TOKEN }}"
            }
        "#},
    );
    let err = Engine::new()
        .with_io(io)
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD, "a.oxfile" AS A]
                LET $got: STRING = A::TRY_READ_B()
            "#},
        )
        .expect_err("A must not see B's inherit");
    assert!(
        format!("{err:#}").contains("env:MOD_B_TOKEN"),
        "fails closed naming the uninherited name, got: {err:#}"
    );
}

#[test]
fn entry_prelude_ignores_block_scoped_env() {
    // Blocks unwind: `ENV` / `INHERIT_ENV` inside `ISOLATED`, `IF`,
    // `FOR` and friends must never seed the entry prelude. Only
    // true top-level (and guard-gated top-level) declarations serve
    // entry calls; block writes leak neither via `entry_envs` nor
    // `entry_inherit_names`.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let mut io = ExecIo::new();
    io.insert_inherit_env("PRELUDE_OK", "ok");
    io.insert_inherit_env("SECRET", "eng");
    let out = Engine::new()
        .with_io(io)
        .run_script(
            &root,
            indoc! {r#"
                IMPORT [STD]
                INHERIT_ENV [PRELUDE_OK, SECRET]
                ENV BLOCK_ONLY=top
                ISOLATED {
                    INHERIT_ENV [PRELUDE_OK]
                    ENV SECRET=hacked
                    ENV BLOCK_ONLY=hacked2
                }
                IF true {
                    ENV BLOCK_ONLY=hacked3
                }
                FUNC PROBE(): STRING {
                    RETURN "{{ env:SECRET }}|{{ env:BLOCK_ONLY }}|{{ env:PRELUDE_OK }}"
                }
                LET $got: STRING = PROBE()
            "#},
        )
        .expect("block-scoped env must not poison the prelude");
    assert_eq!(
        out.bindings.get("got").and_then(|v| v.as_str()),
        Some("eng|top|ok"),
        "entry call serves the prelude, never unwound block writes"
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
