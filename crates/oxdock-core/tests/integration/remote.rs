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

    fn seed(root: &GuardedPath, files: &[(String, Vec<u8>)], dirs: &[String]) -> Result<()> {        let resolver = PathResolver::new(root.root(), root.root())?;
        for dir in dirs {
            if dir.is_empty() {
                continue;
            }
            resolver.create_dir_all(&root.join(dir)?)?;
        }
        for (rel, bytes) in files {
            if let Some(idx) = rel.rfind('/') {
                resolver.create_dir_all(&root.join(&rel[..idx])?)?;
            }
            let mut writer = resolver.open_write(&root.join(rel)?)?;
            writer.write_all(bytes)?;
            writer.flush()?;
        }
        Ok(())
    }
}

impl RemoteRunner for LocalRunner {
    fn run_remote(&self, request: RemoteRequest) -> Result<RemoteResponse> {
        let stdin_bytes = match &request.stdin {
            Some(backend) => Self::drain_backend(backend)?,
            None => Vec::new(),
        };
        // Empty staging plus declared fetches: nothing else crosses.
        let temp = GuardedPath::tempdir()?;
        let root = temp.as_guarded_path().clone();
        Self::seed(&root, &request.fetch_files, &request.fetch_dirs)?;

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

        // Declared pushes only: missing sources fail loudly.
        let resolver = PathResolver::new(root.root(), root.root())?;
        let mut push_files = Vec::new();
        let mut push_dirs = Vec::new();
        for (guest_src, host_dst) in &request.push_decls {
            let src = root.join(guest_src).map_err(|err| {
                anyhow::anyhow!("guest push source {guest_src:?} escapes staging: {err:#}")
            })?;
            match resolver.entry_kind(&src) {
                Ok(oxdock_fs::EntryKind::Dir) => {
                    push_dirs.push(host_dst.clone());
                    let mut stack = vec![(src, host_dst.clone())];
                    while let Some((dir, rel)) = stack.pop() {
                        let mut entries = resolver.read_dir_entries(&dir)?;
                        entries.sort_by_key(|entry| entry.file_name());
                        for entry in entries {
                            let name = entry.file_name().to_string_lossy().to_string();
                            let child = dir.join(&name)?;
                            let child_rel = format!("{rel}/{name}");
                            match resolver.entry_kind(&child)? {
                                oxdock_fs::EntryKind::Dir => {
                                    push_dirs.push(child_rel.clone());
                                    stack.push((child, child_rel));
                                }
                                _ => {
                                    let mut reader = resolver.open_read(&child)?;
                                    let mut bytes = Vec::new();
                                    reader.read_to_end(&mut bytes)?;
                                    push_files.push((child_rel, bytes));
                                }
                            }
                        }
                    }
                }
                Ok(_) => {
                    let mut reader = resolver.open_read(&src).map_err(|err| {
                        anyhow::anyhow!("guest push source {guest_src:?} unreadable: {err:#}")
                    })?;
                    let mut bytes = Vec::new();
                    reader.read_to_end(&mut bytes)?;
                    push_files.push((host_dst.clone(), bytes));
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
            push_files,
            push_dirs,
            push_symlinks: Vec::new(),
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
    let fs: Box<dyn WorkspaceFs> =
        Box::new(PathResolver::new(root.root(), root.root()).unwrap());
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
    let fs: Box<dyn WorkspaceFs> =
        Box::new(PathResolver::new(root.root(), root.root()).unwrap());
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
        LET $flags: LIST = ["a", "b"]
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
    assert!(err.to_string().contains("unknown variable $missing"), "{err:#}");
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
