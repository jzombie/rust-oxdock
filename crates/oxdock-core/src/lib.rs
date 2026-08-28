pub mod exec;
pub use exec::*;

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver};
    use oxdock_parser::{Step, StepKind, parse_script};
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
    fn run_sets_cargo_target_dir_to_fs_root() {
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
        let expected = root.join(".cargo-target").unwrap();

        assert_eq!(
            seen.trim(),
            expected.display().to_string(),
            "CARGO_TARGET_DIR should be scoped"
        );
    }

    #[test]
    fn guard_skips_when_env_missing() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let guard_var = "OXDOCK_GUARD_TEST_TOKEN_UNSET";
        let script = format!(
            indoc!(
                r#"
                [env:{guard}] WRITE skipped.txt hi
                WRITE kept.txt ok
                "#
            ),
            guard = guard_var
        );
        let steps = parse_script(&script).unwrap();

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
            ENV FOO=1
            [env:FOO] WRITE hit.txt yes
            WRITE always.txt ok
            "#
        );
        let steps = parse_script(script).unwrap();

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
            ECHO Hello, world
            WRITE always.txt ok
            "#
        );
        let steps = parse_script(script).unwrap();

        run_steps(&root, &steps).unwrap();

        assert!(exists(&root, "always.txt"), "WRITE after ECHO should run");
    }

    #[test]
    fn guard_on_previous_line_applies_to_next_command() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc!(
            r#"
            ENV FOO=1
            [env:FOO]
            WRITE hit.txt yes
            WRITE always.txt ok
            "#
        );
        let steps = parse_script(script).unwrap();

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
            [!unix] WRITE platform.txt hi
            WRITE always.txt ok
            "#
        );
        let steps = parse_script(script).unwrap();

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
            ENV RUN=1
            [env:RUN] {
                ENV INNER=1
                WRITE scoped.txt hit
            }
            [env:INNER] WRITE leak.txt nope
            "#
        );
        let steps = parse_script(script).unwrap();

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
            MKDIR nested
            ENV RUN=1
            [env:RUN] {
                WORKDIR nested
                WRITE inside.txt ok
            }
            WRITE outside.txt root
            "#
        );
        let steps = parse_script(script).unwrap();

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
            ENV RUN=1
            [env:RUN] {
                WORKSPACE LOCAL
                WRITE local_only.txt inside
            }
            WRITE snapshot_only.txt outside
            "#
        );
        let steps = parse_script(script).unwrap();

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
        // Cargo sets PROFILE during builds/tests; verify guards see it.
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
        let _env_guard = oxdock_sys_test_utils::TestEnvGuard::set("PROFILE", &profile);
        let script = format!(
            indoc!(
                r#"
                [env:PROFILE=={0}] WRITE hit.txt yes
                [env:PROFILE!={0}] WRITE miss.txt no
                "#
            ),
            profile
        );

        let steps = parse_script(&script).unwrap();
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

        let key = "OXDOCK_MULTI_GUARD_TEST_PASS";
        let _env_guard = oxdock_sys_test_utils::TestEnvGuard::set(key, "ok");

        let script = format!(
            indoc!(
                r#"
                [env:{k},env:{k}==ok] WRITE hit.txt yes
                WRITE always.txt ok
                "#
            ),
            k = key
        );
        let steps = parse_script(&script).unwrap();
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

        let key = "OXDOCK_MULTI_GUARD_TEST_FAIL";
        let _env_guard = oxdock_sys_test_utils::TestEnvGuard::set(key, "ok");

        let script = format!(
            indoc!(
                r#"
                [env:{k},env:{k}!=ok] WRITE miss.txt yes
                WRITE always.txt ok
                "#
            ),
            k = key
        );
        let steps = parse_script(&script).unwrap();
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
    fn run_bg_exits_success_and_stops_pipeline() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        // Background succeeds quickly; pipeline should complete without error.
        let script = "RUN_BG sh -c 'sleep 0.05'";
        let steps = parse_script(script).unwrap();
        let res = run_steps(&root, &steps);
        assert!(res.is_ok(), "RUN_BG success should allow clean exit");
    }

    #[cfg(unix)]
    #[cfg_attr(
        miri,
        ignore = "stdout streaming not supported for background command under miri"
    )]
    #[test]
    fn run_bg_failure_bubbles_status() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = "RUN_BG sh -c 'sleep 0.05; exit 7'";
        let steps = parse_script(script).unwrap();
        let err = run_steps(&root, &steps).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("RUN_BG exited with status") || msg.contains("exit status: 7"),
            "should surface failing RUN_BG exit code"
        );
    }

    #[cfg(unix)]
    #[cfg_attr(
        miri,
        ignore = "timing-sensitive background process test is unreliable under Miri"
    )]
    #[test]
    fn run_bg_multiple_stops_on_first_exit_and_does_not_block_steps() {
        let temp = GuardedPath::tempdir().unwrap();
        let root = guard_root(&temp);

        let script = indoc! {
            r#"
            RUN_BG sh -c 'sleep 0.2; echo one > one.txt'
            RUN_BG sh -c 'sleep 0.5; echo two > two.txt'
            WRITE done.txt ok
            "#
        };

        let steps = parse_script(script).unwrap();
        let start = Instant::now();
        let res = run_steps(&root, &steps);
        let elapsed = start.elapsed();

        assert!(res.is_ok(), "RUN_BG success should allow clean exit");
        assert!(
            exists(&root, "done.txt"),
            "foreground step should run after spawning backgrounds"
        );
        assert!(
            exists(&root, "one.txt"),
            "first background should finish and emit output"
        );
        assert!(
            !exists(&root, "two.txt"),
            "second background should be terminated once the first exits"
        );

        let upper = 0.45;
        assert!(
            elapsed.as_secs_f32() < upper && elapsed.as_secs_f32() > 0.15,
            "should wait roughly for first background (~0.2s) but not the second (~0.5s); got {elapsed:?}"
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
            RUN_BG sh -c 'sleep 1; echo late > late.txt'
            RUN __oxdock_missing_command_xyz__
            "#
        };

        let steps = parse_script(script).unwrap();
        assert!(
            run_steps(&root, &steps).is_err(),
            "pipeline should fail on the missing command"
        );
        // The RUN_BG handle is dropped mid-pipeline when the error propagates;
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
            RUN_BG sh -c 'sleep 1; echo late > late.txt'
            EXIT 5
            "#
        };

        let steps = parse_script(script).unwrap();
        let err = run_steps(&root, &steps).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("EXIT requested with code 5"));
        assert!(
            !exists(&root, "late.txt"),
            "background process should be killed when EXIT is hit"
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
                ENV FOO=bar
                RUN echo %FOO% > run.txt
                RUN_BG echo %FOO% > bg.txt
                "#
            }
        } else {
            indoc! {
                r#"
                ENV FOO=bar
                RUN sh -c 'printf %s "$FOO" > run.txt'
                RUN_BG sh -c 'printf %s "$FOO" > bg.txt'
                "#
            }
        };

        let steps = parse_script(script).unwrap();
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
            WRITE snap.txt snap
            WORKSPACE LOCAL
            WRITE local.txt local
            WORKSPACE SNAPSHOT
            WRITE snap2.txt again
            "#
        };

        let steps = parse_script(script).unwrap();
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
            WORKDIR /
            WRITE localroot.txt one
            WORKDIR client
            WRITE client.txt two
            WORKSPACE SNAPSHOT
            WORKDIR /
            WRITE snaproot.txt three
            "#
        };

        let steps = parse_script(script).unwrap();
        run_steps_with_context(&snapshot_root, &local_root, &steps).unwrap();

        assert!(local_root.join("localroot.txt").unwrap().exists());
        assert!(local_client.join("client.txt").unwrap().exists());
        assert!(snapshot_root.join("snaproot.txt").unwrap().exists());
    }
}
