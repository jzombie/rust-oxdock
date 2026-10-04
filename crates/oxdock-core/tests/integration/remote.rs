//! Sealed `REMOTE` execution through an in-process mock runner: no ssh,
//! no muxio, no tarballs. The mock drains the stdin backend, executes the
//! shipped text with a fresh guest [`Engine`](oxdock_core::Engine) on a
//! seeded temp workspace, and returns the result workspace plus stdio
//! bytes. This proves the core contract: closed scope, header injection,
//! `WITH_IO` byte flow, and manifest-diff deletion.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use indoc::indoc;
use oxdock_core::{
    Engine, ExecIo, RemoteRequest, RemoteResponse, RemoteRunner, run_steps_with_manager,
};
use oxdock_fs::{GuardedPath, GuardedTempDir, PathResolver, WorkspaceFs};
use oxdock_process::MockProcessManager;

/// In-process stand-in for the NET SSH session: sealed guest execution on
/// a temp workspace with the same engine and parser the host uses.
struct LocalRunner;

impl LocalRunner {
    fn drain_backend(backend: &oxdock_pipe::PipeInner) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match backend.read_into_timeout(&mut buf, Duration::from_millis(10))? {
                Some(0) | None => break,
                Some(n) => out.extend_from_slice(&buf[..n]),
            }
        }
        Ok(out)
    }

    /// Seed staging from declared fetch paths, streaming bytes straight
    /// from the host filesystem with O(chunk) memory.
    fn seed(
        fs: &dyn WorkspaceFs,
        root: &GuardedPath,
        files: &[(String, GuardedPath)],
        dirs: &[String],
    ) -> Result<()> {
        let resolver = PathResolver::new(root.root(), root.root())?;
        for dir in dirs {
            if dir.is_empty() {
                continue;
            }
            resolver.create_dir_all(&root.join(dir)?)?;
        }
        for (rel, host_path) in files {
            if let Some(idx) = rel.rfind('/') {
                resolver.create_dir_all(&root.join(&rel[..idx])?)?;
            }
            let mut reader = fs.open_read(host_path)?;
            let mut writer = resolver.open_write(&root.join(rel)?)?;
            let mut buf = [0u8; 65536];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                writer.write_all(&buf[..n])?;
            }
            writer.flush()?;
        }
        Ok(())
    }

    /// Copy one guest file back to the host through the passed filesystem,
    /// streaming with O(chunk) memory. Both sides already resolved under
    /// guard by the callers.
    fn copy_back(
        fs: &dyn WorkspaceFs,
        host_root: &GuardedPath,
        guest_resolver: &PathResolver,
        guest_root: &GuardedPath,
        guest_src: &str,
        host_dst: &str,
    ) -> Result<()> {
        let src = guest_root.join(guest_src).map_err(|err| {
            anyhow::anyhow!("guest push source {guest_src:?} escapes staging: {err:#}")
        })?;
        let dst = host_root.join(host_dst).map_err(|err| {
            anyhow::anyhow!("push destination {host_dst:?} escapes the workspace: {err:#}")
        })?;
        if let Some(idx) = host_dst.rfind('/') {
            let dir = host_root.join(&host_dst[..idx])?;
            fs.create_dir_all(&dir)?;
        }
        let mut reader = guest_resolver.open_read(&src).map_err(|err| {
            anyhow::anyhow!("guest push source {guest_src:?} unreadable: {err:#}")
        })?;
        let mut writer = fs.open_write(&dst)?;
        let mut buf = [0u8; 65536];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
        }
        writer.flush()?;
        Ok(())
    }

    /// Push one declared directory back, fanning out under the host
    /// destination with the same mapping the real session enforces.
    fn copy_dir_back(
        fs: &dyn WorkspaceFs,
        host_root: &GuardedPath,
        guest_resolver: &PathResolver,
        guest_root: &GuardedPath,
        guest_src: &str,
        host_dst: &str,
    ) -> Result<()> {
        let mut stack = vec![(guest_src.to_string(), host_dst.to_string())];
        while let Some((guest_rel, host_rel)) = stack.pop() {
            let host_dir = host_root.join(&host_rel).map_err(|err| {
                anyhow::anyhow!("push destination {host_rel:?} escapes the workspace: {err:#}")
            })?;
            fs.create_dir_all(&host_dir)?;
            let guest_dir = guest_root.join(&guest_rel).map_err(|err| {
                anyhow::anyhow!("guest push source {guest_rel:?} escapes staging: {err:#}")
            })?;
            let mut entries = guest_resolver.read_dir_entries(&guest_dir)?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let name = entry.file_name().to_string_lossy().to_string();
                let child_guest_rel = format!("{guest_rel}/{name}");
                let child_host_rel = format!("{host_rel}/{name}");
                let child = guest_dir.join(&name)?;
                match guest_resolver.entry_kind(&child)? {
                    oxdock_fs::EntryKind::Dir => {
                        stack.push((child_guest_rel, child_host_rel));
                    }
                    _ => {
                        Self::copy_back(
                            fs,
                            host_root,
                            guest_resolver,
                            guest_root,
                            &child_guest_rel,
                            &child_host_rel,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl RemoteRunner for LocalRunner {
    fn run_remote(&self, request: RemoteRequest, fs: &dyn WorkspaceFs) -> Result<RemoteResponse> {
        let stdin_bytes = match &request.stdin {
            Some(backend) => Self::drain_backend(backend)?,
            None => Vec::new(),
        };
        // Empty staging plus declared fetches: nothing else crosses.
        let temp = GuardedPath::tempdir()?;
        let root = temp.as_guarded_path().clone();
        Self::seed(fs, &root, &request.fetch_files, &request.fetch_dirs)?;

        let stdout_sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr_sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let mut io = ExecIo::new();
        io.set_stdin(Some(Arc::new(Mutex::new(std::io::Cursor::new(
            stdin_bytes,
        )))));
        io.set_stdout(Some(stdout_sink.clone()));
        io.set_stderr(Some(stderr_sink.clone()));
        // Guest mode: flagged COPY declarations acknowledge as no-ops
        // (transfers already fulfilled around execution).
        io.set_remote_guest(true);
        Engine::new()
            .with_io(io)
            .run_script(&root, &request.script_text)?;

        // Declared pushes only, applied straight to the host filesystem:
        // missing sources fail loudly. Directories fan out; symlinks
        // resolve through to content like local COPY.
        let guest_resolver = PathResolver::new(root.root(), root.root())?;
        let host_root = fs.root().clone();
        for (guest_src, host_dst) in &request.push_decls {
            let src = root.join(guest_src).map_err(|err| {
                anyhow::anyhow!("guest push source {guest_src:?} escapes staging: {err:#}")
            })?;
            match guest_resolver.entry_kind(&src) {
                Ok(oxdock_fs::EntryKind::Dir) => {
                    Self::copy_dir_back(
                        fs,
                        &host_root,
                        &guest_resolver,
                        &root,
                        guest_src,
                        host_dst,
                    )?;
                }
                Ok(_) => {
                    Self::copy_back(fs, &host_root, &guest_resolver, &root, guest_src, host_dst)?;
                }
                Err(err) => {
                    anyhow::bail!("guest push source {guest_src:?} missing: {err:#}");
                }
            }
        }
        let stdout_bytes = stdout_sink.lock().unwrap().clone();
        let stderr_bytes = stderr_sink.lock().unwrap().clone();
        Ok(RemoteResponse {
            stdout_bytes,
            stderr_bytes,
        })
    }
}

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn run_with_runner(
    root: &GuardedPath,
    script: &str,
) -> Result<std::collections::BTreeMap<String, oxdock_parser::Value>> {
    let steps = oxdock_core::parse_script(script).expect("parse REMOTE script");
    let fs: Box<dyn WorkspaceFs> = Box::new(PathResolver::new(root.root(), root.root()).unwrap());
    let mut io = ExecIo::new();
    io.set_remote_runner_for_target("prod".to_string(), Arc::new(LocalRunner));
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), io)
        .map(|(_cwd, _fs, bindings)| bindings)
}

fn read_trimmed(root: &GuardedPath, rel: &str) -> String {
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    resolver
        .read_to_string(&root.join(rel).unwrap())
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[test]
fn remote_without_runner_names_the_binding() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let steps = oxdock_core::parse_script(indoc! {r#"
        REMOTE prod {
            ECHO hi
        }
    "#})
    .unwrap();
    let fs: Box<dyn WorkspaceFs> = Box::new(PathResolver::new(root.root(), root.root()).unwrap());
    let err = run_steps_with_manager(fs, &steps, MockProcessManager::default(), ExecIo::new())
        .map(|_| ())
        .expect_err("REMOTE without a runner must bail");
    assert!(
        err.to_string().contains("--remote prod="),
        "actionable bail, got: {err:#}"
    );
}

#[test]
fn remote_block_is_sealed_but_declared_files_cross() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    let mut writer = resolver
        .open_write(&root.join("input.txt").unwrap())
        .unwrap();
    writer.write_all(b"host-data").unwrap();
    // Flush before the guest reads: buffered backends (Miri synthetic
    // state) commit on flush/drop, and the handle below stays alive
    // across the run. Native files tolerate the omission; Miri does not.
    writer.flush().unwrap();
    let bindings = run_with_runner(
        &root,
        indoc! {r#"
            LET $outside: STRING = "host-only"
            ENV SHARED=host
            REMOTE prod {
                LET $inside: STRING = "guest-only"
                ENV SHARED=guest
                COPY --from-host input.txt ./input.txt
                LET $fetched: STRING = READ input.txt
                WRITE result.txt "saw: {{ $fetched }}"
                COPY --to-host ./result.txt result.txt
                WRITE undeclared.txt guest-wrote-this
            }
        "#},
    )
    .expect("sealed REMOTE block runs");
    // Variables and env do not cross back; declared files do, and only them.
    assert!(!bindings.contains_key("inside"), "guest LET must unwind");
    assert!(!bindings.contains_key("fetched"), "guest LET must unwind");
    assert_eq!(
        bindings.get("outside").and_then(|v| v.as_str()),
        Some("host-only")
    );
    assert_eq!(read_trimmed(&root, "result.txt"), "saw: host-data");
    assert!(
        !root.join("undeclared.txt").unwrap().exists(),
        "undeclared guest writes must never cross"
    );
}

#[test]
fn remote_header_injection_executes() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        LET $version: STRING = "1.2.3"
        LET $count: INT = 41
        LET $flags: LIST<STRING> = ["a", "b"]
        ENV DEPLOY_ENV=staging
        REMOTE prod [$version, $count, $flags, env:DEPLOY_ENV] {
            WRITE version.txt $version
            WRITE env.txt "{{ env:DEPLOY_ENV }}"
            COPY --to-host ./version.txt version.txt
            COPY --to-host ./env.txt env.txt
        }
    "#};
    run_with_runner(&root, script).expect("header injection runs");
    assert_eq!(read_trimmed(&root, "version.txt"), "1.2.3");
    assert_eq!(read_trimmed(&root, "env.txt"), "staging");
}

#[test]
fn remote_header_injection_hostile_string() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    // The hostile value is spelled with DSL escapes in a raw string, so the
    // file holds literal backslashes the renderer must escape. An unescaped
    // `{{ $nope }}` would try to interpolate a missing guest variable and
    // fail the block, so success itself proves the escaping.
    let script = indoc! {r#"
        LET $payload: STRING = "quote\" back\\slash\nnewline \{{ $nope }} semi; brace}"
        REMOTE prod [$payload] {
            WRITE out.txt $payload
            COPY --to-host ./out.txt out.txt
        }
    "#};
    run_with_runner(&root, script).expect("hostile injection runs");
    let expected = "quote\" back\\slash\nnewline {{ $nope }} semi; brace}";
    assert_eq!(read_trimmed(&root, "out.txt"), expected);
}

#[test]
fn remote_unknown_header_name_bails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_with_runner(
        &root,
        indoc! {r#"
            REMOTE prod [$missing] {
                ECHO hi
            }
        "#},
    )
    .expect_err("unknown header var must bail");
    assert!(
        err.to_string().contains("unknown variable $missing"),
        "{err:#}"
    );
}

#[test]
fn remote_pipe_header_entry_bails_naming_the_var() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_with_runner(
        &root,
        indoc! {r#"
            LET $p: PIPE
            REMOTE prod [$p] {
                ECHO hi
            }
        "#},
    )
    .expect_err("PIPE header entry must bail");
    let text = format!("{err:#}");
    assert!(text.contains("$p"), "{text}");
    assert!(text.contains("PIPE"), "{text}");
}

#[test]
fn remote_body_let_shadows_injected_name() {
    // A body LET over an injected name follows normal nested-scope rules:
    // it shadows for the rest of the body instead of erroring, exactly
    // like any braced block. The pushed file proves which value won.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    run_with_runner(
        &root,
        indoc! {r#"
            LET $version: STRING = "1.0"
            REMOTE prod [$version] {
                LET $version: STRING = "2.0"
                WRITE out.txt $version
                COPY --to-host ./out.txt out.txt
            }
        "#},
    )
    .expect("shadowing runs");
    assert_eq!(read_trimmed(&root, "out.txt"), "2.0");
}

#[test]
fn remote_declared_push_lands_and_guest_deletes_nothing() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    for dir in ["pkg", "pkg/deep", "keep"] {
        resolver.create_dir_all(&root.join(dir).unwrap()).unwrap();
    }
    for (rel, text) in [
        ("pkg/file.txt", "data"),
        ("pkg/deep/nested.txt", "data"),
        ("keep/mine.txt", "stays"),
    ] {
        let mut writer = resolver.open_write(&root.join(rel).unwrap()).unwrap();
        writer.write_all(text.as_bytes()).unwrap();
        // Same flush discipline as above: the guest reads these after
        // this setup block, and the handles stay alive across the run.
        writer.flush().unwrap();
    }
    run_with_runner(
        &root,
        indoc! {r#"
            REMOTE prod {
                COPY --from-host pkg ./pkg
                WRITE pkg/deep/nested.txt changed
                COPY --to-host ./pkg mirrored
            }
        "#},
    )
    .expect("fetch and push run");
    assert_eq!(read_trimmed(&root, "mirrored/file.txt"), "data");
    assert_eq!(read_trimmed(&root, "mirrored/deep/nested.txt"), "changed");
    // Host sources are untouched by guest writes.
    assert_eq!(read_trimmed(&root, "pkg/deep/nested.txt"), "data");
    assert_eq!(read_trimmed(&root, "keep/mine.txt"), "stays");
}

#[test]
fn remote_missing_fetch_source_fails_before_execution() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_with_runner(
        &root,
        indoc! {r#"
            REMOTE prod {
                COPY --from-host nope.txt ./nope.txt
                ECHO unreachable
            }
        "#},
    )
    .expect_err("missing fetch source must fail fast");
    assert!(
        format!("{err:#}").contains("fetch source missing"),
        "fail fast, got: {err:#}"
    );
}

#[test]
fn remote_missing_push_source_fails_loudly() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_with_runner(
        &root,
        indoc! {r#"
            REMOTE prod {
                ECHO hi
                COPY --to-host ./never-created.txt back.txt
            }
        "#},
    )
    .expect_err("missing push source must fail");
    assert!(
        format!("{err:#}").contains("never-created.txt"),
        "loud failure, got: {err:#}"
    );
}

#[test]
fn remote_with_io_streams_stdin_and_stdout() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let bindings = run_with_runner(
        &root,
        indoc! {r#"
            LET $in: PIPE
            LET $out: PIPE
            WITH_IO [stdout=$in] ECHO "hello-remote"
            WITH_IO [stdin=$in, stdout=$out] {
                REMOTE prod {
                    LET $line: STRING = READ
                    ECHO "guest-saw: {{ $line }}"
                }
            }
            WITH_IO [stdin=$out] WRITE back.txt
            LET $back: STRING = READ back.txt
        "#},
    )
    .expect("piped REMOTE runs");
    let back = bindings
        .get("back")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        back.contains("guest-saw: hello-remote"),
        "unexpected guest echo: {back:?}"
    );
}

/// Runner wrapper recording whether the request carried a live stdout
/// backend. The mock executes synchronously either way, so presence (not
/// timing) is the observable contract.
struct BackendProbeRunner {
    saw_live_stdout: Arc<std::sync::atomic::AtomicBool>,
}

impl RemoteRunner for BackendProbeRunner {
    fn run_remote(&self, request: RemoteRequest, fs: &dyn WorkspaceFs) -> Result<RemoteResponse> {
        self.saw_live_stdout.store(
            request.stdout.is_some(),
            std::sync::atomic::Ordering::SeqCst,
        );
        LocalRunner.run_remote(request, fs)
    }
}

#[test]
fn remote_runner_death_mid_stream_fails_loudly() {
    // Abrupt guest death: partial bytes already streamed live, then the
    // transport dies. The step must fail with transport context instead
    // of hanging or surfacing the partial output as success.
    struct DyingRunner;
    impl RemoteRunner for DyingRunner {
        fn run_remote(
            &self,
            request: RemoteRequest,
            _fs: &dyn WorkspaceFs,
        ) -> Result<RemoteResponse> {
            if let Some(backend) = &request.stdout {
                let writer = backend.writer_handle();
                if let Ok(mut guard) = writer.lock() {
                    let _ = guard.write_all(b"partial\n");
                    let _ = guard.flush();
                }
            }
            Err(anyhow::anyhow!(
                "simulated guest disconnect: unexpected EOF (transport closed)"
            ))
        }
    }
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let steps = oxdock_core::parse_script(indoc! {r#"
        LET $cap: PIPE
        WITH_IO [stdout=$cap] REMOTE prod {
            ECHO "never-arrives"
        }
    "#})
    .unwrap();
    let fs: Box<dyn WorkspaceFs> = Box::new(PathResolver::new(root.root(), root.root()).unwrap());
    let mut io = ExecIo::new();
    io.set_remote_runner_for_target("prod".to_string(), Arc::new(DyingRunner));
    let err = run_steps_with_manager(fs, &steps, MockProcessManager::default(), io)
        .map(|_| ())
        .expect_err("dead guest must fail the step");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("transport failed") && msg.contains("unexpected EOF"),
        "transport context with cause, got: {msg:?}"
    );
}

#[test]
fn remote_shared_capture_fan_in_drains_via_eof() {
    // Promoted from the `t.ox` dogfood script: two concurrent ASYNC
    // REMOTEs share one capture pipe, and the host drains it with
    // `WHILE !EOF` (no sentinel, no baked-in count). Both guests echo
    // the same line so the assertion is order-immune.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    run_with_runner(
        &root,
        indoc! {r#"
            IMPORT [STD]
            LET $cap: PIPE
            WITH_IO [stdout=$cap] ASYNC {
                LET $a: HANDLE = ASYNC REMOTE prod {
                    ECHO "1.2.3"
                }
                LET $b: HANDLE = ASYNC REMOTE prod {
                    ECHO "1.2.3"
                }
                AWAIT $a
                AWAIT $b
            }
            LET $n: INT = 0
            WHILE !EOF($cap) {
                WITH_IO [stdin=$cap] READ_LINE $line
                IF EOF($cap) {
                    IF $line == "" { BREAK }
                }
                ASSERT_EQ $line "1.2.3"
                $n = $n + 1
            }
            WRITE count.txt "{{ $n }}"
        "#},
    )
    .expect("shared capture drains");
    assert_eq!(read_trimmed(&root, "count.txt"), "2");
}

#[test]
fn remote_inside_async_task_keeps_live_stdout_backend() {
    // Regression: task workers dropped out_pipe/stdin_pipe, so a REMOTE
    // step in any ASYNC task silently lost its streaming backend (the
    // session accumulated while core flushed at completion) instead of
    // delivering bytes as they arrive.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let steps = oxdock_core::parse_script(indoc! {r#"
        LET $cap: PIPE
        WITH_IO [stdout=$cap] {
            LET $t: HANDLE = ASYNC REMOTE prod {
                ECHO "live"
            }
            AWAIT $t
        }
        WITH_IO [stdin=$cap] READ_LINE $got
        WRITE out.txt "{{ $got }}"
    "#})
    .unwrap();
    let fs: Box<dyn WorkspaceFs> = Box::new(PathResolver::new(root.root(), root.root()).unwrap());
    let mut io = ExecIo::new();
    io.set_remote_runner_for_target(
        "prod".to_string(),
        Arc::new(BackendProbeRunner {
            saw_live_stdout: Arc::clone(&seen),
        }),
    );
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), io)
        .expect("task-nested REMOTE runs");
    assert!(
        seen.load(std::sync::atomic::Ordering::SeqCst),
        "REMOTE in an ASYNC task must keep its live stdout backend"
    );
    assert_eq!(read_trimmed(&root, "out.txt"), "live");
}

#[test]
fn remote_inside_async_block_keeps_live_stdout_backend() {
    // Same contract through the block-task path (`ASYNC { ... }` rather
    // than the single-command form): the worker must inherit the
    // enclosing live backend.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let steps = oxdock_core::parse_script(indoc! {r#"
        LET $cap: PIPE
        WITH_IO [stdout=$cap] ASYNC {
            REMOTE prod {
                ECHO "live-block"
            }
        }
        WITH_IO [stdin=$cap] READ_LINE $got
        WRITE out.txt "{{ $got }}"
    "#})
    .unwrap();
    let fs: Box<dyn WorkspaceFs> = Box::new(PathResolver::new(root.root(), root.root()).unwrap());
    let mut io = ExecIo::new();
    io.set_remote_runner_for_target(
        "prod".to_string(),
        Arc::new(BackendProbeRunner {
            saw_live_stdout: Arc::clone(&seen),
        }),
    );
    run_steps_with_manager(fs, &steps, MockProcessManager::default(), io)
        .expect("block-nested REMOTE runs");
    assert!(
        seen.load(std::sync::atomic::Ordering::SeqCst),
        "REMOTE in an ASYNC block must keep its live stdout backend"
    );
    assert_eq!(read_trimmed(&root, "out.txt"), "live-block");
}
