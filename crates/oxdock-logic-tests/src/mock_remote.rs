//! In-process mock REMOTE transport session, shared by the
//! `ast_commands` fixture runner and the docs-conformance fence runner.
//!
//! Sealed guest execution on a temp workspace with the same engine and
//! parser the host uses: empty staging plus declared fetches in,
//! declared pushes plus stdio bytes out, nothing else crosses. Mirrors
//! the mock in oxdock-core's remote integration tests.

use anyhow::{Result, anyhow};
use oxdock_core::{Engine, ExecIo, RemoteRequest, RemoteResponse, RemoteRunner};
use oxdock_fs::{GuardedPath, PathResolver, WorkspaceFs};
use std::sync::{Arc, Mutex};

/// In-process stand-in for a REMOTE transport session: sealed guest
/// execution on a temp workspace with the same engine and parser the host
/// uses. Bound per target by harness configuration (`mock_remote`).
pub struct MockRemoteRunner;

impl MockRemoteRunner {
    fn drain_backend(backend: &oxdock_pipe::PipeInner) -> Result<Vec<u8>> {
        use std::time::Duration;
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

    fn seed(
        fs: &dyn WorkspaceFs,
        root: &GuardedPath,
        files: &[(String, GuardedPath)],
        dirs: &[String],
    ) -> Result<()> {
        use std::io::{Read, Write};
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

    fn copy_back(
        fs: &dyn WorkspaceFs,
        host_root: &GuardedPath,
        guest_resolver: &PathResolver,
        guest_root: &GuardedPath,
        guest_src: &str,
        host_dst: &str,
    ) -> Result<()> {
        use std::io::{Read, Write};
        let src = guest_root
            .join(guest_src)
            .map_err(|err| anyhow!("guest push source {guest_src:?} escapes staging: {err:#}"))?;
        let dst = host_root.join(host_dst).map_err(|err| {
            anyhow!("push destination {host_dst:?} escapes the workspace: {err:#}")
        })?;
        if let Some(idx) = host_dst.rfind('/') {
            let dir = host_root.join(&host_dst[..idx])?;
            fs.create_dir_all(&dir)?;
        }
        let mut reader = guest_resolver
            .open_read(&src)
            .map_err(|err| anyhow!("guest push source {guest_src:?} unreadable: {err:#}"))?;
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
                anyhow!("push destination {host_rel:?} escapes the workspace: {err:#}")
            })?;
            fs.create_dir_all(&host_dir)?;
            let guest_dir = guest_root.join(&guest_rel).map_err(|err| {
                anyhow!("guest push source {guest_rel:?} escapes staging: {err:#}")
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

impl RemoteRunner for MockRemoteRunner {
    fn run_remote(&self, request: RemoteRequest, fs: &dyn WorkspaceFs) -> Result<RemoteResponse> {
        let stdin_bytes = match &request.stdin {
            Some(backend) => Self::drain_backend(backend)?,
            None => Vec::new(),
        };
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
        io.set_remote_guest(true);
        Engine::new()
            .with_io(io)
            .run_script(&root, &request.script_text)?;

        let guest_resolver = PathResolver::new(root.root(), root.root())?;
        let host_root = fs.root().clone();
        for (guest_src, host_dst) in &request.push_decls {
            let src = root.join(guest_src).map_err(|err| {
                anyhow!("guest push source {guest_src:?} escapes staging: {err:#}")
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
