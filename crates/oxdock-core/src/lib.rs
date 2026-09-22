extern crate self as oxdock_core;

pub mod exec;
pub mod pipeline;
pub use exec::*;
pub use oxdock_parser::{
    Arg, ArgType, CommandMeta, CommandSpec, StepKind, all_metadata, all_structural_metadata,
    lower_command,
};
pub use oxdock_process::ProcessManager;

define_pipeline! {
    StepKind::Run(..) => exec::dispatch_run,
    StepKind::RunExec { .. } => exec::dispatch_run_exec,
    StepKind::AsyncBlock { .. } => exec::dispatch_async_block,
    StepKind::Echo(..) => exec::dispatch_echo,
    StepKind::Workdir(..) => exec::dispatch_workdir,
    StepKind::Workspace(..) => exec::dispatch_workspace,
    StepKind::Env { .. } => exec::dispatch_env,
    StepKind::Copy { .. } => exec::dispatch_copy,
    StepKind::CopyGit { .. } => exec::dispatch_copy_git,
    StepKind::Symlink { .. } => exec::dispatch_symlink,
    StepKind::Mkdir(..) => exec::dispatch_mkdir,
    StepKind::Ls(..) => exec::dispatch_ls,
    StepKind::Cwd => exec::dispatch_cwd,
    StepKind::Read(..) => exec::dispatch_read,
    StepKind::ReadLine { .. } => exec::dispatch_read_line,
    StepKind::Write { .. } => exec::dispatch_write,
    StepKind::Append { .. } => exec::dispatch_append,
    StepKind::Expand { .. } => exec::dispatch_expand,
    StepKind::AssertEq { .. } => exec::dispatch_assert_eq,
    StepKind::AssertContains { .. } => exec::dispatch_assert_contains,
    StepKind::HashSha256 { .. } => exec::dispatch_hash_sha256,
    StepKind::Exit(..) => exec::dispatch_exit,
    StepKind::AssignAsync { .. } => exec::dispatch_assign_async_step,
    StepKind::Set { .. } => exec::dispatch_set,
    StepKind::Await { .. } => exec::dispatch_await_step,
    StepKind::AssignCapture { .. } => exec::dispatch_assign_capture_step,
    StepKind::AwaitCapture { .. } => exec::dispatch_await_capture_step,
    StepKind::Cancel { .. } => exec::dispatch_cancel_step,
    StepKind::Timeout { .. } => exec::dispatch_timeout_step,
    StepKind::Sleep { .. } => exec::dispatch_sleep_step,
    StepKind::ListAppend { .. } => exec::dispatch_push_into_step,
    StepKind::FuncDef { .. } => exec::dispatch_func_def,
    StepKind::Call { .. } => exec::dispatch_call,
    StepKind::Return { .. } => exec::dispatch_return,
    StepKind::While { .. } => exec::dispatch_while_loop,
    StepKind::Break => exec::dispatch_break,
    StepKind::Continue => exec::dispatch_continue,
}

/// Parse a script using the production `lower_command` dispatcher.
/// The typed `ParseError` converts into `anyhow::Error` at this boundary
/// with no intermediate `.context()` wrapping, so the message survives.
/// Builtin function names seed the reserved set, so `FUNC` shadowing a
/// native fails here; hosts unknown at parse time fall back to the runtime
/// `define_func` guard.
pub fn parse_script(input: &str) -> anyhow::Result<Vec<oxdock_parser::Step>> {
    parse_script_with_modules(input, std_module_table())
}

/// Parse with a module provenance table so calls resolve statically:
/// qualified `MODULE::NAME` checks membership, bare `NAME` resolves through
/// `SCRIPT` definitions and `IMPORT`ed modules. Reserved covers builtins
/// plus every table module, so `FUNC` shadowing any of them fails here;
/// hosts unknown at parse time fall back to the runtime `define_func`
/// guard.
pub fn parse_script_with_modules(
    input: &str,
    modules: oxdock_parser::ModuleTable,
) -> anyhow::Result<Vec<oxdock_parser::Step>> {
    let mut reserved = builtin_function_names();
    reserved.extend(modules.reserved_base_names());
    Ok(oxdock_parser::parse_script_with_modules(
        input,
        lower_command,
        reserved,
        modules,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver, env as oxdock_env};
    use oxdock_parser::{Step, StepKind};
    #[cfg(unix)]
    use std::time::Instant;

    fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
        temp.as_guarded_path().clone()
    }

    fn read_trimmed(path: &GuardedPath) -> String {
        let resolver = PathResolver::new(path.root(), path.root()).unwrap();
        resolver
            .read_to_string(path)
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    fn create_dirs(path: &GuardedPath) {
        let resolver = PathResolver::new(path.root(), path.root()).unwrap();
        resolver.create_dir_all(path).unwrap();
    }

    fn exists(root: &GuardedPath, rel: &str) -> bool {
        root.join(rel).map(|p| p.exists()).unwrap_or(false)
    }

    #[test]
    fn run_isolates_cargo_target_dir_from_workspace() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        #[allow(clippy::disallowed_macros)]
        let cmd = if cfg!(windows) {
            "echo %CARGO_TARGET_DIR% > seen.txt"
        } else {
            "printf %s \"$CARGO_TARGET_DIR\" > seen.txt"
        };

        let steps = vec![Step {
            guard: None,
            kind: StepKind::Run(cmd.to_string().into()),
            scope_enter: 0,
            scope_exit: 0,
        }];

        run_steps(&root, &steps).unwrap();

        let seen = read_trimmed(&root.join("seen.txt").unwrap());
        let legacy = root.join(".cargo-target").unwrap();

        assert_ne!(
            seen.trim(),
            legacy.display().to_string(),
            "CARGO_TARGET_DIR must not target the workspace tree"
        );
        assert!(
            seen.trim().contains("oxdock-cargo-"),
            "CARGO_TARGET_DIR must point at the isolated scratch location, got {seen:?}"
        );
    }

    #[test]
    fn lazy_snapshot_run_materializes_but_local_run_does_not() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        // Snapshot-rooted RUN executes against the snapshot workdir.
        let steps = parse_script("RUN echo hi").unwrap();
        let output = run_steps_with_lazy_snapshot(&root, &steps, ExecIo::new()).unwrap();
        assert!(
            output.snapshot.is_materialized(),
            "snapshot-rooted RUN must materialize the snapshot"
        );

        // LOCAL-rooted RUN executes against the live tree.
        let steps = parse_script(indoc! {r#"
            WORKSPACE LOCAL
            RUN echo hi
        "#})
        .unwrap();
        let output = run_steps_with_lazy_snapshot(&root, &steps, ExecIo::new()).unwrap();
        assert!(
            !output.snapshot.is_materialized(),
            "LOCAL-rooted RUN must never materialize the snapshot"
        );
    }

    #[test]
    fn lazy_empty_run_creates_nothing() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let output = run_steps_with_lazy_snapshot(&root, &[], ExecIo::new()).unwrap();
        assert!(!output.snapshot.is_materialized());
        assert!(output.snapshot.get().is_none());
    }

    #[test]
    fn lazy_local_echo_assert_failure_never_materializes_snapshot() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc!(
            r#"
            WORKSPACE LOCAL
            ECHO playground pid is 41067
            ASSERT_CONTAINS stdout "playground pid i!!"
            "#
        );
        let steps = parse_script(script).unwrap();

        // Retain the snapshot handle across the failing run: the lazy
        // convenience wrapper discards it on error, so drive the manager
        // directly to pin the unmaterialized invariant (issue #131).
        let mut resolver = PathResolver::new_lazy(root.clone()).unwrap();
        resolver.set_workspace_root(root.clone());
        let snapshot = resolver.snapshot_handle();
        let fs: Box<dyn oxdock_fs::WorkspaceFs> = Box::new(resolver);
        let result = run_steps_with_manager(
            fs,
            &steps,
            oxdock_process::default_process_manager(),
            ExecIo::new(),
        );
        assert!(result.is_err(), "mismatched ASSERT_CONTAINS must fail");
        let err = result.err().unwrap();
        assert!(!snapshot.is_materialized());
        assert!(snapshot.get().is_none());

        let enriched = enrich_lazy_error(&snapshot, &root, err);
        let msg = enriched.to_string();
        assert!(
            msg.contains("did not contain 'playground pid i!!'"),
            "{msg}"
        );
        assert!(msg.contains("never materialized"), "{msg}");
        assert!(!msg.contains("filesystem snapshot (root"), "{msg}");
    }

    #[test]
    fn lazy_local_echo_assert_success_leaves_snapshot_pending() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc!(
            r#"
            WORKSPACE LOCAL
            ECHO playground pid is 41067
            ASSERT_CONTAINS stdout "playground pid is 41067"
            "#
        );
        let steps = parse_script(script).unwrap();

        let output = run_steps_with_lazy_snapshot(&root, &steps, ExecIo::new()).unwrap();
        assert!(!output.snapshot.is_materialized());
        assert!(output.snapshot.get().is_none());
    }

    #[test]
    fn lazy_default_mode_echo_assert_failure_keeps_snapshot_pending() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        // Same conditions as the LOCAL failure test, minus the WORKSPACE
        // statement: the implicit default is snapshot mode, but the
        // non-mutating steps must still leave it pending (issue #131).
        let script = indoc!(
            r#"
            ECHO playground pid is 41067
            ASSERT_CONTAINS stdout "playground pid i!!"
            "#
        );
        let steps = parse_script(script).unwrap();

        // Retain the snapshot handle across the failing run: the lazy
        // convenience wrapper discards it on error, so drive the manager
        // directly to pin the unmaterialized invariant (issue #131).
        let mut resolver = PathResolver::new_lazy(root.clone()).unwrap();
        resolver.set_workspace_root(root.clone());
        let snapshot = resolver.snapshot_handle();
        let fs: Box<dyn oxdock_fs::WorkspaceFs> = Box::new(resolver);
        let result = run_steps_with_manager(
            fs,
            &steps,
            oxdock_process::default_process_manager(),
            ExecIo::new(),
        );
        assert!(result.is_err(), "mismatched ASSERT_CONTAINS must fail");
        let err = result.err().unwrap();
        assert!(!snapshot.is_materialized());
        assert!(snapshot.get().is_none());

        let enriched = enrich_lazy_error(&snapshot, &root, err);
        let msg = enriched.to_string();
        assert!(
            msg.contains("did not contain 'playground pid i!!'"),
            "{msg}"
        );
        assert!(msg.contains("never materialized"), "{msg}");
        assert!(!msg.contains("filesystem snapshot (root"), "{msg}");
    }

    #[test]
    fn lazy_default_mode_write_materializes_snapshot() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        // No WORKSPACE statement: a snapshot-targeted WRITE must transition
        // the lazy handle to materialized (issue #131).
        let steps = parse_script("WRITE \"snap.txt\" \"snap\"").unwrap();

        let output = run_steps_with_lazy_snapshot(&root, &steps, ExecIo::new()).unwrap();
        assert!(output.snapshot.is_materialized());
        let snapshot_root = output.snapshot.get().expect("snapshot must be published");
        assert!(
            exists(snapshot_root, "snap.txt"),
            "WRITE must land inside the materialized snapshot"
        );
    }

    #[test]
    fn guard_skips_when_env_missing() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let guard_var = oxdock_env::GUARD_TEST_TOKEN_UNSET;
        let script = format!(
            indoc!(
                r#"
                [env:{guard}] WRITE "skipped.txt" "hi"
                WRITE "kept.txt" "ok"
                "#
            ),
            guard = guard_var
        );
        let steps = crate::parse_script(&script).unwrap();

        run_steps(&root, &steps).unwrap();

        assert!(
            !exists(&root, "skipped.txt"),
            "guarded WRITE should be skipped"
        );
        assert!(exists(&root, "kept.txt"), "unguarded WRITE should run");
    }

    #[test]
    fn guard_sees_env_set_by_env_step() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc!(
            r#"
            ENV FOO="1"
            [env:FOO] WRITE "hit.txt" "yes"
            WRITE "always.txt" "ok"
            "#
        );
        let steps = crate::parse_script(script).unwrap();

        run_steps(&root, &steps).unwrap();

        assert!(
            exists(&root, "hit.txt"),
            "guarded WRITE should run after ENV sets variable"
        );
        assert!(exists(&root, "always.txt"), "unguarded WRITE should run");
    }

    #[test]
    fn echo_runs_and_allows_subsequent_steps() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc!(
            r#"
            ECHO "Hello, world"
            WRITE "always.txt" "ok"
            "#
        );
        let steps = crate::parse_script(script).unwrap();

        run_steps(&root, &steps).unwrap();

        assert!(exists(&root, "always.txt"), "WRITE after ECHO should run");
    }

    #[test]
    fn guard_on_previous_line_applies_to_next_command() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc!(
            r#"
            ENV FOO="1"
            [env:FOO]
            WRITE "hit.txt" "yes"
            WRITE "always.txt" "ok"
            "#
        );
        let steps = crate::parse_script(script).unwrap();

        run_steps(&root, &steps).unwrap();

        assert!(
            exists(&root, "hit.txt"),
            "guarded WRITE on next line should run"
        );
        assert!(exists(&root, "always.txt"), "unguarded WRITE should run");
    }

    #[test]
    fn guard_respects_platform_negation() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc!(
            r#"
            [not(unix)] WRITE "platform.txt" "hi"
            WRITE "always.txt" "ok"
            "#
        );
        let steps = crate::parse_script(script).unwrap();

        run_steps(&root, &steps).unwrap();

        #[allow(clippy::disallowed_macros)]
        let expect_skipped = cfg!(unix);
        assert_eq!(
            exists(&root, "platform.txt"),
            !expect_skipped,
            "platform guard should skip on unix and run elsewhere"
        );
        assert!(exists(&root, "always.txt"), "unguarded WRITE should run");
    }

    #[test]
    fn guard_block_env_scope_restores_after_exit() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc!(
            r#"
            ENV RUN="1"
            [env:RUN] {
                ENV INNER="1"
                WRITE "scoped.txt" "hit"
            }
            [env:INNER] WRITE "leak.txt" "nope"
            "#
        );
        let steps = crate::parse_script(script).unwrap();

        run_steps(&root, &steps).unwrap();

        assert!(exists(&root, "scoped.txt"), "block should run");
        assert!(
            !exists(&root, "leak.txt"),
            "env set inside block must not leak outward"
        );
    }

    #[test]
    fn guard_block_workdir_scope_restores_after_exit() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc!(
            r#"
            MKDIR "nested"
            ENV RUN="1"
            [env:RUN] {
                WORKDIR "nested"
                WRITE "inside.txt" "ok"
            }
            WRITE "outside.txt" "root"
            "#
        );
        let steps = crate::parse_script(script).unwrap();

        run_steps(&root, &steps).unwrap();

        assert!(
            exists(&root, "nested/inside.txt"),
            "inside write should land in nested dir"
        );
        assert!(
            exists(&root, "outside.txt"),
            "workdir should reset after block exits"
        );
        assert!(
            !exists(&root, "nested/outside.txt"),
            "writes after block should not stay scoped"
        );
    }

    #[test]
    fn workspace_scope_restores_after_guard_block() {
        let snapshot = GuardedPath::tempdir().unwrap();
        let local = GuardedPath::tempdir().unwrap();
        let snapshot_root = guard_root(&snapshot);
        let local_root = guard_root(&local);

        let script = indoc!(
            r#"
            ENV RUN="1"
            [env:RUN] {
                WORKSPACE LOCAL
                WRITE "local_only.txt" "inside"
            }
            WRITE "snapshot_only.txt" "outside"
            "#
        );
        let steps = crate::parse_script(script).unwrap();

        run_steps_with_context(&snapshot_root, &local_root, &steps).unwrap();

        assert!(
            local_root.join("local_only.txt").unwrap().exists(),
            "workspace switch inside block should affect local root"
        );
        assert!(
            snapshot_root.join("snapshot_only.txt").unwrap().exists(),
            "writes after block must target snapshot again"
        );
        assert!(
            !local_root.join("snapshot_only.txt").unwrap().exists(),
            "workspace should reset after guard block exits"
        );
    }

    #[test]
    fn guard_matches_profile_env() {
        // Set PROFILE via script ENV; guards now only see script-level env.
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let profile = std::env::var(oxdock_env::PROFILE).unwrap_or_else(|_| "debug".to_string());
        let script = format!(
            indoc!(
                r#"
                ENV PROFILE={0}
                [eq(env:PROFILE, {0})] WRITE "hit.txt" "yes"
                [ne(env:PROFILE, {0})] WRITE "miss.txt" "no"
                "#
            ),
            profile
        );

        let steps = crate::parse_script(&script).unwrap();
        run_steps(&root, &steps).unwrap();

        assert!(
            exists(&root, "hit.txt"),
            "PROFILE-matching guard should run"
        );
        assert!(
            !exists(&root, "miss.txt"),
            "PROFILE inequality guard should skip for current profile"
        );
    }

    #[test]
    fn multiple_guards_all_must_pass() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let key = oxdock_env::MULTI_GUARD_TEST_PASS;

        let script = format!(
            indoc!(
                r#"
                ENV {k}=ok
                [env:{k},eq(env:{k}, ok)] WRITE "hit.txt" "yes"
                WRITE "always.txt" "ok"
                "#
            ),
            k = key
        );
        let steps = crate::parse_script(&script).unwrap();
        run_steps(&root, &steps).unwrap();

        assert!(
            exists(&root, "hit.txt"),
            "guarded step should run when all guards pass"
        );
        assert!(exists(&root, "always.txt"), "unguarded step should run");
    }

    #[test]
    fn multiple_guards_skip_when_one_fails() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let key = oxdock_env::MULTI_GUARD_TEST_FAIL;

        let script = format!(
            indoc!(
                r#"
                ENV {k}=ok
                [env:{k},ne(env:{k}, ok)] WRITE "miss.txt" "yes"
                WRITE "always.txt" "ok"
                "#
            ),
            k = key
        );
        let steps = crate::parse_script(&script).unwrap();
        run_steps(&root, &steps).unwrap();

        assert!(
            !exists(&root, "miss.txt"),
            "guarded step should skip when any guard fails"
        );
        assert!(exists(&root, "always.txt"), "unguarded step should run");
    }

    #[cfg(unix)]
    #[cfg_attr(
        miri,
        ignore = "stdout streaming not supported for background command under miri"
    )]
    #[test]
    fn async_exits_success_and_stops_pipeline() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        // Background succeeds quickly; pipeline should complete without error.
        let script = "ASYNC RUN \"sh -c 'sleep 0.05'\"";
        let steps = crate::parse_script(script).unwrap();
        let res = run_steps(&root, &steps);
        assert!(res.is_ok(), "ASYNC success should allow clean exit");
    }

    #[cfg(unix)]
    #[cfg_attr(
        miri,
        ignore = "stdout streaming not supported for background command under miri"
    )]
    #[test]
    fn async_failure_bubbles_status() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = "ASYNC RUN \"sh -c 'sleep 0.05; exit 7'\"";
        let steps = crate::parse_script(script).unwrap();
        let err = run_steps(&root, &steps).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ASYNC process exited with status") || msg.contains("exit status: 7"),
            "should surface failing ASYNC exit code"
        );
    }

    #[cfg(unix)]
    #[cfg_attr(
        miri,
        ignore = "timing-sensitive background process test is unreliable under Miri"
    )]
    #[test]
    fn async_multiple_stops_on_first_exit_and_does_not_block_steps() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc! {
            r#"
            ASYNC RUN "sh -c 'sleep 0.2; echo one > one.txt'"
            ASYNC RUN "sh -c 'sleep 0.5; echo two > two.txt'"
            WRITE "done.txt" "ok"
            "#
        };

        let steps = crate::parse_script(script).unwrap();
        let start = Instant::now();
        let res = run_steps(&root, &steps);
        let elapsed = start.elapsed();

        assert!(res.is_ok(), "ASYNC success should allow clean exit");
        assert!(
            exists(&root, "done.txt"),
            "foreground step should run after spawning backgrounds"
        );
        assert!(
            exists(&root, "one.txt"),
            "first background should finish and emit output"
        );
        // With the new poll-all model, both children run to completion.
        // The second background (~0.5s) should also finish.
        assert!(
            exists(&root, "two.txt"),
            "second background should finish (poll-all waits for all)"
        );

        let upper = 0.8;
        assert!(
            elapsed.as_secs_f32() < upper && elapsed.as_secs_f32() > 0.15,
            "should wait for both backgrounds (~0.5s); got {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[cfg_attr(
        miri,
        ignore = "timing-sensitive background process test is unreliable under Miri"
    )]
    #[test]
    fn background_killed_when_unrelated_step_fails() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc! {
            r#"
            ASYNC RUN "sh -c 'sleep 1; echo late > late.txt'"
            RUN "__oxdock_missing_command_xyz__"
            "#
        };

        let steps = crate::parse_script(script).unwrap();
        assert!(
            run_steps(&root, &steps).is_err(),
            "pipeline should fail on the missing command"
        );
        // The ASYNC handle is dropped mid-pipeline when the error propagates;
        // its Drop safety net must kill the writer before it can emit the
        // late artifact.
        assert!(
            !exists(&root, "late.txt"),
            "abandoned background writer must be killed by Drop teardown"
        );
    }

    #[cfg(unix)]
    #[cfg_attr(
        miri,
        ignore = "stdout streaming not supported for background command under miri"
    )]
    #[test]
    fn exit_terminates_backgrounds_and_returns_code() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc! {
            r#"
            ASYNC RUN "sh -c 'sleep 1; echo late > late.txt'"
            EXIT 5
            "#
        };

        let steps = crate::parse_script(script).unwrap();
        let err = run_steps(&root, &steps).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("EXIT requested with code 5"));
        assert!(
            !exists(&root, "late.txt"),
            "background process should be killed when EXIT is hit"
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "EXIT joins real background threads; timing-sensitive under Miri"
    )]
    fn exit_inside_nested_block_reports_code_and_kills_backgrounds() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc! {
            r#"
            ASYNC {
                SLEEP 30s
            }
            [bool:true] {
                EXIT 5
            }
            "#
        };

        let steps = crate::parse_script(script).unwrap();
        let start = std::time::Instant::now();
        let err = run_steps(&root, &steps).unwrap_err();
        assert!(
            err.to_string().contains("EXIT requested with code 5"),
            "nested EXIT must report its code: {err}"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "nested EXIT must kill the background SLEEP promptly"
        );
    }

    #[cfg_attr(
        miri,
        ignore = "stdout streaming not supported for background command under miri"
    )]
    #[test]
    fn env_applies_to_run_and_background() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        #[allow(clippy::disallowed_macros)]
        let script = if cfg!(windows) {
            indoc! {
                r#"
                ENV FOO="bar"
                RUN "echo %FOO% > run.txt"
                ASYNC RUN "echo %FOO% > bg.txt"
                "#
            }
        } else {
            indoc! {
                r#"
                ENV FOO="bar"
                RUN "sh -c 'printf %s \"$FOO\" > run.txt'"
                ASYNC RUN "sh -c 'printf %s \"$FOO\" > bg.txt'"
                "#
            }
        };

        let steps = crate::parse_script(script).unwrap();
        run_steps(&root, &steps).unwrap();

        assert_eq!(read_trimmed(&root.join("run.txt").unwrap()), "bar");
        assert_eq!(read_trimmed(&root.join("bg.txt").unwrap()), "bar");
    }

    #[test]
    fn workspace_switches_between_snapshot_and_local() {
        let snapshot = GuardedPath::tempdir().unwrap();
        let local = GuardedPath::tempdir().unwrap();
        let snapshot_root = guard_root(&snapshot);
        let local_root = guard_root(&local);

        let script = indoc! {
            r#"
            WRITE "snap.txt" "snap"
            WORKSPACE LOCAL
            WRITE "local.txt" "local"
            WORKSPACE SNAPSHOT
            WRITE "snap2.txt" "again"
            "#
        };

        let steps = crate::parse_script(script).unwrap();
        run_steps_with_context(&snapshot_root, &local_root, &steps).unwrap();

        assert!(snapshot_root.join("snap.txt").unwrap().exists());
        assert!(snapshot_root.join("snap2.txt").unwrap().exists());
        assert!(local_root.join("local.txt").unwrap().exists());
    }

    #[test]
    fn workspace_root_changes_where_slash_points() {
        let snapshot = GuardedPath::tempdir().unwrap();
        let local = GuardedPath::tempdir().unwrap();
        let snapshot_root = guard_root(&snapshot);
        let local_root = guard_root(&local);
        let local_client = local_root.join("client").unwrap();
        create_dirs(&local_client);

        let script = indoc! {
            r#"
            WORKSPACE LOCAL
            WORKDIR "/"
            WRITE "localroot.txt" "one"
            WORKDIR "client"
            WRITE "client.txt" "two"
            WORKSPACE SNAPSHOT
            WORKDIR "/"
            WRITE "snaproot.txt" "three"
            "#
        };

        let steps = crate::parse_script(script).unwrap();
        run_steps_with_context(&snapshot_root, &local_root, &steps).unwrap();

        assert!(local_root.join("localroot.txt").unwrap().exists());
        assert!(local_client.join("client.txt").unwrap().exists());
        assert!(snapshot_root.join("snaproot.txt").unwrap().exists());
    }

    /// Serialized cache-dir pin for hermetic tests. `OXDOCK_CACHE_DIR` is
    /// process-global; the lock plus drop-restore keeps parallel tests
    /// isolated (mirrors `SerialCargoEnv` in `oxdock-process`).
    struct SerialCacheDir {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    static CACHE_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl SerialCacheDir {
        fn pin(dir: &GuardedPath) -> Self {
            let lock = CACHE_DIR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let prev = std::env::var(oxdock_env::CACHE_DIR).ok();
            // SAFETY: serialized by `CACHE_DIR_LOCK`, restored on drop.
            unsafe {
                std::env::set_var(
                    oxdock_env::CACHE_DIR,
                    dir.as_path().to_string_lossy().into_owned(),
                );
            }
            Self { _lock: lock, prev }
        }
    }

    impl Drop for SerialCacheDir {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(oxdock_env::CACHE_DIR, v),
                    None => std::env::remove_var(oxdock_env::CACHE_DIR),
                }
            }
        }
    }

    #[test]
    fn workspace_cache_persists_across_runs() {
        let pin = GuardedPath::tempdir().unwrap();
        let pin_root = guard_root(&pin);
        let _env = SerialCacheDir::pin(&pin_root);

        let snapshot = GuardedPath::tempdir().unwrap();
        let local = GuardedPath::tempdir().unwrap();
        let snapshot_root = guard_root(&snapshot);
        let local_root = guard_root(&local);

        // First run writes through CACHE, then hops roots to prove the
        // entry does not land in snapshot or local.
        let first = indoc! {
            r#"
            WRITE "snap.txt" "snap"
            WORKSPACE LOCAL
            WRITE "local.txt" "local"
            WORKSPACE CACHE
            WRITE "cached.txt" "persistent"
            WORKSPACE SNAPSHOT
            "#
        };
        let steps = crate::parse_script(first).unwrap();
        run_steps_with_context(&snapshot_root, &local_root, &steps).unwrap();

        // Native only: the Miri synthetic backend keys all non-snapshot
        // state by build root, so cross-root absence is vacuous there.
        // Selection itself is covered by mock tests under Miri.
        #[cfg(not(miri))]
        assert!(!exists(&snapshot_root, "cached.txt"));
        #[cfg(not(miri))]
        assert!(!exists(&local_root, "cached.txt"));

        // A fresh run with a fresh snapshot root still sees the entry:
        // the cache survived while the snapshot did not. The Miri
        // synthetic backend keys non-snapshot state by build root, so the
        // second run reuses the local root there; natively it gets a
        // fresh one.
        let fresh_snapshot = GuardedPath::tempdir().unwrap();
        let fresh_snapshot_root = guard_root(&fresh_snapshot);
        #[cfg(not(miri))]
        let fresh_local = GuardedPath::tempdir().unwrap();
        #[cfg(not(miri))]
        let fresh_local_root = guard_root(&fresh_local);
        #[cfg(miri)]
        let fresh_local_root = local_root.clone();

        let second = indoc! {
            r#"
            WORKSPACE CACHE
            LET $v: STRING = READ cached.txt
            ASSERT_EQ $v "persistent"
            "#
        };
        let steps = crate::parse_script(second).unwrap();
        run_steps_with_context(&fresh_snapshot_root, &fresh_local_root, &steps).unwrap();
    }

    #[test]
    fn workspace_system_reaches_outside_roots() {
        let snapshot = GuardedPath::tempdir().unwrap();
        let local = GuardedPath::tempdir().unwrap();
        let snapshot_root = guard_root(&snapshot);
        let local_root = guard_root(&local);
        let outside = GuardedPath::tempdir().unwrap();
        let outside_root = guard_root(&outside);

        // From a snapshot-selected start, SYSTEM writes an outside file by
        // absolute path, reads it back, and writes a sibling next to it.
        // Seeding from inside the script keeps every backend (including
        // the Miri synthetic one, which keys state by guard root) in one
        // namespace.
        let script = format!(
            indoc! {r#"
                WORKSPACE SYSTEM
                WRITE "{secret}" outside
                LET $v: STRING = READ "{secret}"
                ASSERT_EQ $v outside
                WRITE "{sibling}" done
                WORKSPACE SNAPSHOT
            "#},
            secret = outside_root
                .join("secret.txt")
                .unwrap()
                .as_path()
                .to_string_lossy(),
            sibling = outside_root
                .join("sibling.txt")
                .unwrap()
                .as_path()
                .to_string_lossy(),
        );
        let steps = crate::parse_script(&script).unwrap();
        run_steps_with_context(&snapshot_root, &local_root, &steps).unwrap();

        // Native only (see above): Miri shares one namespace per build root,
        // while the in-script ASSERT_EQ already verifies content everywhere.
        #[cfg(not(miri))]
        assert!(exists(&outside_root, "sibling.txt"));
        // Native only (see above): Miri shares one namespace per build root.
        #[cfg(not(miri))]
        assert!(!exists(&snapshot_root, "sibling.txt"));
        #[cfg(not(miri))]
        assert!(!exists(&local_root, "sibling.txt"));
    }

    #[test]
    fn copy_from_workspace_targets() {
        let pin = GuardedPath::tempdir().unwrap();
        let pin_root = guard_root(&pin);
        let _env = SerialCacheDir::pin(&pin_root);

        let snapshot = GuardedPath::tempdir().unwrap();
        let local = GuardedPath::tempdir().unwrap();
        let snapshot_root = guard_root(&snapshot);
        let local_root = guard_root(&local);
        let outside = GuardedPath::tempdir().unwrap();
        let outside_root = guard_root(&outside);

        // SNAPSHOT, CACHE, LOCAL, and SYSTEM sources each resolve against
        // their own root and land in the snapshot cwd. The SYSTEM source
        // is seeded from inside the script so every backend observes one
        // namespace.
        let script = format!(
            indoc! {r#"
                WRITE snap-src.txt from-snap
                WORKSPACE CACHE
                WRITE cache-src.txt from-cache
                WORKSPACE SYSTEM
                WRITE "{secret}" outside
                WORKSPACE SNAPSHOT
                COPY --from-workspace SNAPSHOT snap-src.txt snap-copy.txt
                COPY --from-workspace CACHE cache-src.txt cache-copy.txt
                COPY --from-workspace SYSTEM "{secret}" sys-copy.txt
                COPY --from-workspace LOCAL "{local_src}" local-copy.txt
            "#},
            secret = outside_root
                .join("secret.txt")
                .unwrap()
                .as_path()
                .to_string_lossy(),
            local_src = local_root
                .join("local-src.txt")
                .unwrap()
                .as_path()
                .to_string_lossy(),
        );
        // Seed the LOCAL source through the build context side.
        let local_seeder = PathResolver::new(local_root.as_path(), local_root.as_path()).unwrap();
        local_seeder
            .write_file(&local_root.join("local-src.txt").unwrap(), b"from-local")
            .unwrap();

        let steps = crate::parse_script(&script).unwrap();
        run_steps_with_context(&snapshot_root, &local_root, &steps).unwrap();

        assert!(exists(&snapshot_root, "snap-copy.txt"));
        assert!(exists(&snapshot_root, "cache-copy.txt"));
        assert!(exists(&snapshot_root, "sys-copy.txt"));
        assert!(exists(&snapshot_root, "local-copy.txt"));

        // A pending snapshot has no source to copy from.
        let pending = indoc! {r#"
            COPY --from-workspace SNAPSHOT missing.txt out.txt
        "#};
        let steps = crate::parse_script(pending).unwrap();
        let fresh_snapshot = GuardedPath::tempdir().unwrap();
        let fresh_local = GuardedPath::tempdir().unwrap();
        assert!(
            run_steps_with_context(
                &guard_root(&fresh_snapshot),
                &guard_root(&fresh_local),
                &steps
            )
            .is_err(),
            "COPY from a pending snapshot must fail"
        );
    }
}
