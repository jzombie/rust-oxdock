//! End-to-end SSH plugin tests over loopback TCP.
//!
//! Every test here needs real sockets, threads, and a Tokio runtime, so
//! all of them skip under Miri with a reason.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use indoc::indoc;
use oxdock_core::{Engine, EngineOutput};
use oxdock_fs::{GuardedPath, GuardedTempDir};

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn run_script(root: &GuardedPath, script: &str) -> anyhow::Result<EngineOutput> {
    let mut engine = Engine::new();
    engine.register_module(oxdock_ssh_plugin::module());
    engine.run_script(root, script)
}

fn run_script_with(
    registry: Arc<oxdock_net_plugin::EndpointRegistry>,
    root: &GuardedPath,
    script: &str,
) -> anyhow::Result<EngineOutput> {
    let mut engine = Engine::new();
    engine.register_module(oxdock_ssh_plugin::module_with_endpoints(registry));
    engine.run_script(root, script)
}

#[test]
#[cfg_attr(
    miri,
    ignore = "blind sampling drives SSH session paths outside Miri isolation"
)]
fn returns_gate_covers_ssh_functions() {
    // Same returns contract as STD builtins, over this module's own
    // registrations: sampled calls that reach `Ok` satisfy the slot,
    // and anything sampling cannot reach must carry examples. Examples
    // name `doc-ssh-*` services, so the harness pre-maps them the way a
    // CLI runner would: scripts declare, the runner binds.
    let registry = Arc::new(oxdock_net_plugin::EndpointRegistry::new(false));
    for service in [
        "doc-ssh-route",
        "doc-ssh-serve",
        "doc-ssh-accept",
        "doc-ssh-pump",
        "doc-ssh-close",
        "doc-ssh-connect",
        "doc-ssh-pty",
        "doc-ssh-run",
        "doc-ssh-exit-status",
    ] {
        registry
            .add_cli_mapping(service, "127.0.0.1:0")
            .expect("map doc service");
    }
    registry.bind_all().expect("bind doc services");
    let module = oxdock_ssh_plugin::module_with_endpoints(registry);
    let proc = oxdock_process::default_process_manager();
    oxdock_core::assert_module_returns_conformance(
        "SSH",
        &module.funcs,
        vec![module.clone()],
        proc,
    );
}

fn read_trimmed(path: &GuardedPath) -> String {
    use oxdock_fs::PathResolver;
    let resolver = PathResolver::new(path.root(), path.root()).unwrap();
    resolver
        .read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Read the addr file a script writes after serving. Polls with a
/// deadline so a slow start fails loudly instead of racing the write.
fn read_addr(probe: &GuardedPath) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let addr = read_trimmed(&probe.join("addr.txt").unwrap());
        if !addr.is_empty() {
            return addr;
        }
        if Instant::now() >= deadline {
            panic!("addr file never appeared");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Read a stage file a script writes mid-run. Same deadline discipline
/// as `read_addr`, for driver/script rendezvous points.
fn poll_file(probe: &GuardedPath, name: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = read_trimmed(&probe.join(name).unwrap());
        if !text.is_empty() {
            return text;
        }
        if Instant::now() >= deadline {
            panic!("{name} file never appeared");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runner-mapped registry for one service: the harness binds ephemeral
/// loopback, the script only ever names the service. Mirrors a CLI
/// `-p 127.0.0.1:0:<service>` invocation.
fn mapped_registry(service: &str) -> Arc<oxdock_net_plugin::EndpointRegistry> {
    let registry = Arc::new(oxdock_net_plugin::EndpointRegistry::new(false));
    registry
        .add_cli_mapping(service, "127.0.0.1:0")
        .expect("map service");
    registry.bind_all().expect("bind service");
    registry
}

/// Runner-mapped memory registry for services: in-process rendezvous
/// by explicit runner designation. SSH rejects these pre-run; NET
/// rendezvous on them.
fn memory_registry(services: &[&str]) -> Arc<oxdock_net_plugin::EndpointRegistry> {
    let registry = Arc::new(oxdock_net_plugin::EndpointRegistry::new(false));
    for service in services {
        registry
            .add_mapping(
                &oxdock_net_plugin::EndpointKey::tcp(oxdock_net_plugin::VirtualEndpoint::Name(
                    service.to_string(),
                )),
                oxdock_net_plugin::BindingSpec::Memory,
            )
            .expect("map memory service");
    }
    registry
}

/// Real bound address of a mapped service, for the `TestClient` driver.
fn service_addr(registry: &Arc<oxdock_net_plugin::EndpointRegistry>, service: &str) -> SocketAddr {
    registry
        .bound_addr(&oxdock_net_plugin::EndpointKey::tcp(
            oxdock_net_plugin::VirtualEndpoint::Name(service.to_string()),
        ))
        .expect("service bound")
}

/// Hold a numeric service name nobody else holds: bind ephemeral, read
/// the port back, and rebind it, retrying against parallel tests racing
/// for the same port. Used ONLY by the anti-binding proofs, where the
/// script must name a number the host never bound. A plugin that stole
/// the number fails every retry, so theft still fails deterministically.
fn hold_numeric_name() -> (std::net::TcpListener, String) {
    for _ in 0..50 {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("ephemeral bind")
            .local_addr()
            .expect("addr")
            .port();
        match std::net::TcpListener::bind(format!("127.0.0.1:{port}")) {
            Ok(held) => return (held, port.to_string()),
            Err(_) => continue,
        }
    }
    panic!("could not hold a numeric service name");
}

struct TestHandler;

impl russh::client::Handler for TestHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> anyhow::Result<bool> {
        Ok(true)
    }
}

/// Synchronous loopback SSH client for tests.
struct TestClient {
    runtime: tokio::runtime::Runtime,
    session: russh::client::Handle<TestHandler>,
    reader: russh::ChannelReadHalf,
    writer: russh::ChannelWriteHalf<russh::client::Msg>,
}

impl TestClient {
    fn connect(addr: SocketAddr, user: &str, password: &str) -> anyhow::Result<Self> {
        Self::connect_impl(addr, user, password, None, None)
    }

    /// Connect requesting the given `exec` command instead of a shell.
    /// The server announces the session with the command attached, so
    /// scripts can route before pumping.
    fn connect_exec(
        addr: SocketAddr,
        user: &str,
        password: &str,
        command: &str,
    ) -> anyhow::Result<Self> {
        Self::connect_impl(addr, user, password, None, Some(command))
    }

    /// Connect requesting a pseudo-terminal first, like interactive
    /// clients do. The server answers without allocating one.
    fn connect_with_pty(addr: SocketAddr, user: &str, password: &str) -> anyhow::Result<Self> {
        Self::connect_impl(addr, user, password, Some((80, 24)), None)
    }

    /// Connect with explicit pty dimensions (cols, rows).
    fn connect_with_pty_dims(
        addr: SocketAddr,
        user: &str,
        password: &str,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Self> {
        Self::connect_impl(addr, user, password, Some((cols, rows)), None)
    }

    /// Send a live window-change for the open channel.
    fn window_change(&mut self, cols: u32, rows: u32) -> anyhow::Result<()> {
        self.runtime
            .block_on(self.writer.window_change(cols, rows, 0, 0))
            .map_err(|err| anyhow::anyhow!("window_change failed: {err}"))?;
        Ok(())
    }

    fn connect_impl(
        addr: SocketAddr,
        user: &str,
        password: &str,
        pty_dims: Option<(u16, u16)>,
        exec_command: Option<&str>,
    ) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let (session, reader, writer) = runtime.block_on(async {
            let config = Arc::new(russh::client::Config::default());
            let mut session = russh::client::connect(config, addr, TestHandler).await?;
            let auth = session.authenticate_password(user, password).await?;
            if !matches!(auth, russh::client::AuthResult::Success) {
                anyhow::bail!("test client authentication rejected");
            }
            let channel = session.channel_open_session().await?;
            if let Some((cols, rows)) = pty_dims {
                channel
                    .request_pty(
                        true,
                        "xterm-256color",
                        u32::from(cols),
                        u32::from(rows),
                        0,
                        0,
                        &[],
                    )
                    .await?;
            }
            if let Some(command) = exec_command {
                channel.exec(true, command).await?;
            } else {
                channel.request_shell(true).await?;
            }
            let (mut reader, writer) = channel.split();
            // Synchronize with the server: each want_reply request above
            // is answered with Success once processed (exec/shell requests
            // announce the session first). Returning before the acks lets
            // an immediate close win the race and strand the session
            // unannounced, hanging the dequeuer.
            let mut acks = 1 + usize::from(pty_dims.is_some());
            while acks > 0 {
                match tokio::time::timeout(Duration::from_secs(10), reader.wait())
                    .await
                    .map_err(|_| anyhow::anyhow!("no request acknowledgement from server"))?
                {
                    Some(russh::ChannelMsg::Success) => acks -= 1,
                    Some(_) => {}
                    None => anyhow::bail!("server closed the channel during handshake"),
                }
            }
            Ok::<_, anyhow::Error>((session, reader, writer))
        })?;
        Ok(Self {
            runtime,
            session,
            reader,
            writer,
        })
    }

    fn send(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.runtime
            .block_on(self.writer.data_bytes(bytes::Bytes::copy_from_slice(bytes)))
            .map_err(|err| anyhow::anyhow!("test client send failed: {err}"))?;
        Ok(())
    }

    /// Read until `needle` appears in the stream or `timeout` elapses.
    fn read_until(&mut self, needle: &[u8], timeout: Duration) -> anyhow::Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut collected = Vec::new();
        while !collected
            .windows(needle.len())
            .any(|window| window == needle)
        {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            if remaining.is_zero() {
                anyhow::bail!("timed out waiting for echo");
            }
            let message = self
                .runtime
                .block_on(async { tokio::time::timeout(remaining, self.reader.wait()).await })
                .map_err(|_| anyhow::anyhow!("timed out waiting for echo"))?
                .ok_or_else(|| anyhow::anyhow!("server closed the channel"))?;
            if let russh::ChannelMsg::Data { data } = message {
                collected.extend_from_slice(&data);
            }
        }
        Ok(collected)
    }

    fn close(self) {
        let Self {
            runtime, session, ..
        } = self;
        let _ = runtime.block_on(session.disconnect(russh::Disconnect::ByApplication, "", ""));
    }

    /// Half-close our stdin: the server observes EOF while the channel
    /// stays open for the reply and the close.
    fn eof(&mut self) -> anyhow::Result<()> {
        self.runtime
            .block_on(self.writer.eof())
            .map_err(|err| anyhow::anyhow!("test client eof failed: {err}"))?;
        Ok(())
    }

    /// Wait for the server to close the channel, skipping any straggler
    /// messages. Times out loudly: a server that never closes a drained
    /// pump strands the client.
    fn wait_close(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            if remaining.is_zero() {
                anyhow::bail!("timed out waiting for server close");
            }
            let message = self
                .runtime
                .block_on(async { tokio::time::timeout(remaining, self.reader.wait()).await })
                .map_err(|_| anyhow::anyhow!("timed out waiting for server close"))?;
            if message.is_none() {
                return Ok(());
            }
        }
    }

    /// Wait for the server's `exit-status` request. Data, acks, and a
    /// pre-status EOF are skipped like stock clients skip them; close or
    /// timeout bails.
    fn wait_exit_status(&mut self, timeout: Duration) -> anyhow::Result<u32> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            if remaining.is_zero() {
                anyhow::bail!("timed out waiting for exit-status");
            }
            let message = self
                .runtime
                .block_on(async { tokio::time::timeout(remaining, self.reader.wait()).await })
                .map_err(|_| anyhow::anyhow!("timed out waiting for exit-status"))?
                .ok_or_else(|| anyhow::anyhow!("server closed the channel before exit-status"))?;
            match message {
                russh::ChannelMsg::ExitStatus { exit_status } => return Ok(exit_status),
                russh::ChannelMsg::Close => {
                    anyhow::bail!("channel ended before exit-status")
                }
                _ => {}
            }
        }
    }
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn serve_and_close_roundtrip() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-serve-close");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-serve-close", {username: "guest", password: "close-pass"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $closed: BOOL = SSH_CLOSE($m.server)
        WRITE closed.txt "{{ $closed }}"
    "#};
    run_script_with(registry, &root, script).expect("serve and close runs");
    let addr = read_trimmed(&root.join("addr.txt").unwrap());
    assert!(
        addr.starts_with("127.0.0.1:") && !addr.ends_with(":0"),
        "mapped service reports its real bind: {addr}"
    );
    assert_eq!(read_trimmed(&root.join("closed.txt").unwrap()), "true");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn wrong_password_rejected() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-auth");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-auth", {username: "guest", password: "right-pass"})
        SLEEP 8s
        SSH_CLOSE($m.server)
    "#};
    // Pre-bound by the harness: the driver addr is known before spawning.
    let addr = service_addr(&registry, "test-ssh-auth");
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    // Timed: russh pads every rejection to at least `auth_rejection_time`
    // (the client's opening `none` probe included), so a 1s padding
    // stalls each password prompt by a noticeable beat. The rejection
    // must land far under a second.
    let start = Instant::now();
    let wrong = TestClient::connect(addr, "guest", "wrong-pass");
    let elapsed = start.elapsed();
    assert!(wrong.is_err(), "wrong password must be rejected");
    assert!(
        elapsed < Duration::from_millis(800),
        "wrong-password rejection took {elapsed:?}: auth padding regressed"
    );
    let wrong_user = TestClient::connect(addr, "intruder", "right-pass");
    assert!(wrong_user.is_err(), "wrong username must be rejected");
    handle
        .join()
        .expect("script thread joins")
        .expect("script runs");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn accept_echo_roundtrip() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-echo");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-echo", {username: "guest", password: "echo-pass"})
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $echo: HANDLE = ASYNC { SSH_PUMP($out, $in) }
        AWAIT $acc
        AWAIT $echo
        SSH_CLOSE($m.server)
    "#};
    let addr = service_addr(&registry, "test-ssh-echo");
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let mut client = TestClient::connect(addr, "guest", "echo-pass").expect("client connects");
    client.send(b"hello-echo").expect("client sends");
    let echoed = client
        .read_until(b"hello-echo", Duration::from_secs(10))
        .expect("echo returns");
    assert!(
        echoed.windows(10).any(|window| window == b"hello-echo"),
        "echoed bytes must round-trip"
    );
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("script completes after disconnect");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn pty_request_accepted_echo_roundtrip() {
    // Interactive shape: pty request, then shell, then bytes. The server
    // answers the pty request without allocating a terminal, and the
    // session must proceed to full duplex like a pty-less one.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-pty");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-pty", {username: "guest", password: "pty-pass"})
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $echo: HANDLE = ASYNC { SSH_PUMP($out, $in) }
        AWAIT $acc
        AWAIT $echo
        SSH_CLOSE($m.server)
    "#};
    let addr = service_addr(&registry, "test-ssh-pty");
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let mut client =
        TestClient::connect_with_pty(addr, "guest", "pty-pass").expect("pty client connects");
    client.send(b"hello-pty").expect("client sends");
    let echoed = client
        .read_until(b"hello-pty", Duration::from_secs(10))
        .expect("echo returns");
    assert!(
        echoed.windows(9).any(|window| window == b"hello-pty"),
        "echoed bytes must round-trip"
    );
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("script completes after disconnect");
}

/// Drive an outer session that resizes mid-stream, then give the
/// change time to flush through TCP and the poll loop before dropping:
/// `block_on` returns after queueing, not after delivery.
fn drive_outer_resize(addr: SocketAddr, cols: u16, rows: u16) {
    let mut client =
        TestClient::connect_with_pty_dims(addr, "u", "p", 90, 30).expect("client connects");
    std::thread::sleep(Duration::from_secs(1));
    client
        .window_change(u32::from(cols), u32::from(rows))
        .expect("resize sends");
    std::thread::sleep(Duration::from_secs(2));
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a terminal plus subprocesses"
)]
fn pty_explicit_size() {
    // A pty sized explicitly must report exactly that size. The session
    // comes from DEQUEUE; explicit dims override whatever the client
    // requested. Unix reads `stty size` ("rows cols"); ConPTY answers
    // `mode con`.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-pty-size");
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-pty-size", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $child: LIST<STRING> = ["sh", "-c", "stty size"]
        [family:windows] {
            $child = ["cmd", "/c", "mode con"]
        }
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, $child, 40, 100, $in, $out) }
            AWAIT $t
            [family:unix] {
                ASSERT_CONTAINS $out "40 100"
            }
            [family:windows] {
                ASSERT_CONTAINS $out "Lines:"
                ASSERT_CONTAINS $out "40"
                ASSERT_CONTAINS $out "Columns:"
                ASSERT_CONTAINS $out "100"
            }
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client = TestClient::connect(addr, "u", "p").expect("client connects");
    handle
        .join()
        .expect("script thread joins")
        .expect("explicit pty size runs");
    client.close();
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a terminal plus subprocesses"
)]
fn pty_child_sees_session_env() {
    // Block-scoped ENV rides into the pty spawn: the child observes the
    // dequeued session's identity (user, client and server addresses,
    // requested command) through SSH_* vars. Unix expands `$VAR`;
    // ConPTY expands `%VAR%`.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-env");
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-env", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        ENV PROBE_TOP="top"
        LET $child: LIST<STRING> = ["sh", "-c", "echo relay:$SSH_USER:$SSH_CLIENT:$SSH_SERVER:$SSH_COMMAND:$PROBE_TOP"]
        [family:windows] {
            $child = ["cmd", "/c", "echo relay:%SSH_USER%:%SSH_CLIENT%:%SSH_SERVER%:%SSH_COMMAND%:%PROBE_TOP%"]
        }
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            ENV SSH_USER="{{ $dq.username }}"
            ENV SSH_CLIENT="{{ $dq.addr }}"
            ENV SSH_SERVER="{{ $m.addr }}"
            ENV SSH_COMMAND="{{ $dq.command }}"
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, $child, 0, 0, $in, $out) }
            AWAIT $t
            ASSERT_CONTAINS $out "relay:u:127.0.0.1:"
            ASSERT_CONTAINS $out "127.0.0.1:"
            ASSERT_CONTAINS $out "probe-command"
            ASSERT_CONTAINS $out ":top"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client =
        TestClient::connect_exec(addr, "u", "p", "probe-command").expect("client connects");
    handle
        .join()
        .expect("script thread joins")
        .expect("session env relay runs");
    client.close();
}

#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus forkpty plus subprocesses"
)]
fn argv_preserves_quote_backslash_unix() {
    // Unix-only by construction: asserts `execvp` delivers argv
    // byte-verbatim. Windows joins argv into one command line by
    // design, so the same assertion cannot hold there; the Windows
    // side pins its own contract in the `cmd_*` tests below.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-argv-unix");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-argv-unix", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["printf", "%s", $dq.command], 0, 0, $in, $out) }
            LET $code: INT = AWAIT $t
            ASSERT_EQ $code 0
            ASSERT_CONTAINS $out "a\"b\\c"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client = TestClient::connect_exec(addr, "u", "p", "a\"b\\c").expect("client connects");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("argv fidelity holds");
}

#[test]
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn cmd_argv_preserves_backslashes_windows() {
    // Issue 223 claim, `cmd`-specific: backslashes without quotes must
    // survive the argv to command-line join literally. No Unix twin:
    // Unix never joins, proven by `argv_preserves_quote_backslash_unix`.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-argv-bs-win");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-argv-bs-win", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["cmd", "/c", $dq.command], 0, 0, $in, $out) }
            LET $code: INT = AWAIT $t
            ASSERT_EQ $code 0
            ASSERT_CONTAINS $out "AB\\CD"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client = TestClient::connect_exec(addr, "u", "p", "echo AB\\CD").expect("client connects");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("backslashes survive argv");
}

#[test]
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn cmd_env_quote_behavior_windows() {
    // Quoted segments travel through the environment relay instead of
    // `cmd /c` argv: the join quotes for CRT programs, which `cmd`
    // does not parse back, so command text rides `%SSH_COMMAND%`
    // verbatim (see issue 223).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-env-q-win");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-env-q-win", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            ENV SSH_COMMAND="{{ $dq.command }}"
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["cmd", "/c", "%SSH_COMMAND%"], 0, 0, $in, $out) }
            LET $code: INT = AWAIT $t
            ASSERT_EQ $code 0
            ASSERT_CONTAINS $out "\"foo bar\""
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client =
        TestClient::connect_exec(addr, "u", "p", "echo \"foo bar\"").expect("client connects");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("quotes survive env relay");
}

#[test]
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn cmd_argv_percent_verbatim_windows() {
    // Issue 223 claim, `cmd`-specific: percent signs travel verbatim
    // through the spawn with no doubling. No Unix twin: `%` is
    // ordinary data outside `cmd`.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-argv-pct-win");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-argv-pct-win", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["cmd", "/c", $dq.command], 0, 0, $in, $out) }
            LET $code: INT = AWAIT $t
            ASSERT_EQ $code 0
            ASSERT_CONTAINS $out "100%%done"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client =
        TestClient::connect_exec(addr, "u", "p", "echo 100%%done").expect("client connects");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("percent signs survive argv");
}

#[test]
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn env_relay_preserves_backslashes_windows() {
    // Environment relay is byte-verbatim: backslash runs in the
    // dequeued command must reach `cmd` untouched. Windows-only
    // because the corruption claim (issue 223 bug 2) is about `cmd`
    // observing the relay; the shared verbatim path is proven on both
    // platforms by `pty_child_sees_session_env`.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-env-bs-win");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-env-bs-win", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            ENV SSH_COMMAND="{{ $dq.command }}"
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["cmd", "/c", "%SSH_COMMAND%"], 0, 0, $in, $out) }
            LET $code: INT = AWAIT $t
            ASSERT_EQ $code 0
            ASSERT_CONTAINS $out "AB\\CD\\\\EF"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client =
        TestClient::connect_exec(addr, "u", "p", "echo AB\\CD\\\\EF").expect("client connects");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("backslashes survive env relay");
}

#[test]
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn cmd_env_relay_quoted_reg_query_windows() {
    // The windows-headless TODO, verbatim: plain quoted `reg query`
    // paths must work once the transport is faithful. They do through
    // the environment relay: quotes reach `cmd` untouched, so the key
    // resolves instead of looking up a `\"...`-prefixed name.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-env-reg-win");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-env-reg-win", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            ENV SSH_COMMAND="{{ $dq.command }}"
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["cmd", "/c", "%SSH_COMMAND%"], 0, 0, $in, $out) }
            LET $code: INT = AWAIT $t
            ASSERT_EQ $code 0
            ASSERT_CONTAINS $out "SetupType"
            ASSERT_CONTAINS $out "REG_DWORD"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client = TestClient::connect_exec(
        addr,
        "u",
        "p",
        "reg query \"HKLM\\SYSTEM\\Setup\" /v SetupType",
    )
    .expect("client connects");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("quoted reg query resolves");
}

#[test]
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn cmd_env_relay_full_probe_windows() {
    // The windows-headless readiness probe, verbatim
    // (`src/ssh.rs::reg_probe_command` on feat/oxdock-ssh): `&`-chained
    // queries with `2>nul` redirects, `set` + `call` deferred expansion
    // with single `%`, and `&&`/`||` verdicts. All five markers must
    // arrive through the environment relay, proving the quote-free
    // contortions are unnecessary: this exact shape runs unmodified.
    // The single `%` also proves interpolation never touches `%`.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-env-probe-win");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-env-probe-win", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            ENV SSH_COMMAND="{{ $dq.command }}"
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["cmd", "/c", "%SSH_COMMAND%"], 0, 0, $in, $out) }
            LET $code: INT = AWAIT $t
            ASSERT_EQ $code 0
            ASSERT_CONTAINS $out "[SETUP]"
            ASSERT_CONTAINS $out "[OOBE]"
            ASSERT_CONTAINS $out "[CBS]"
            ASSERT_CONTAINS $out "[WU]"
            ASSERT_CONTAINS $out "[END]"
            ASSERT_CONTAINS $out "SetupType"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let command = [
        "echo [SETUP]",
        "reg query HKLM\\SYSTEM\\Setup /v SetupType 2>nul",
        "echo [OOBE]",
        "reg query HKLM\\SYSTEM\\Setup /v OOBEInProgress 2>nul",
        "echo [CBS]",
        "set WHCBSK=HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Component Based Servicing\\RebootPending& call reg query %WHCBSK% 2>nul && echo YES || echo NO",
        "echo [WU]",
        "set WHWUK=HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\WindowsUpdate\\Auto Update\\RebootRequired& call reg query %WHWUK% 2>nul && echo YES || echo NO",
        "echo [END]",
    ]
    .join(" & ");
    let client = TestClient::connect_exec(addr, "u", "p", &command).expect("client connects");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("full probe runs unmodified");
}

#[test]
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn cmd_env_relay_quoted_powershell_windows() {
    // The windows-headless diet TODO: a plain quoted `-Command` string
    // carrying a pipeline must survive. The relay hands `cmd` the text
    // verbatim; `cmd` keeps the quoted pipe inside the argument, and
    // PowerShell parses the pipeline itself.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-env-ps-win");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-env-ps-win", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            ENV SSH_COMMAND="{{ $dq.command }}"
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["cmd", "/c", "%SSH_COMMAND%"], 0, 0, $in, $out) }
            LET $code: INT = AWAIT $t
            ASSERT_EQ $code 0
            ASSERT_CONTAINS $out "QP-ALPHA"
            ASSERT_CONTAINS $out "QP-BETA"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client = TestClient::connect_exec(
        addr,
        "u",
        "p",
        "powershell -NoProfile -Command \"'qp-alpha','qp-beta' | ForEach-Object { $_.ToUpper() }\"",
    )
    .expect("client connects");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("quoted powershell pipeline runs");
}

#[test]
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn interactive_shell_first_bytes_survive_windows() {
    // Windows-only stress probe for the issue 223 first-keystroke loss:
    // the suspected race site is the ConPTY console-setup handshake,
    // which has no Unix counterpart. Each of several fresh pty+shell
    // connections sends a marker immediately after the handshake, and
    // every marker must round-trip through the echo pump.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-shell-stress");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-shell-stress", {username: "guest", password: "pass"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $i: INT = 0
        WHILE $i < 10 {
            LET $in: PIPE
            LET $out: PIPE
            LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
            LET $echo: HANDLE = ASYNC { SSH_PUMP($out, $in) }
            AWAIT $acc
            AWAIT $echo
            $i = $i + 1
        }
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    for i in 0..10 {
        let marker = format!("first-byte-{i}");
        let mut client =
            TestClient::connect_with_pty(addr, "guest", "pass").expect("client connects");
        client.send(marker.as_bytes()).expect("client sends");
        let echoed = client
            .read_until(marker.as_bytes(), Duration::from_secs(10))
            .expect("marker round-trips");
        assert!(
            echoed
                .windows(marker.len())
                .any(|window| window == marker.as_bytes()),
            "session {i} must deliver its first bytes"
        );
        client.close();
    }
    handle
        .join()
        .expect("script thread joins")
        .expect("stress script completes");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn semaphore_guards_the_accept_loop() {
    // End to end of the proxy admission shape: cap 1 admits the first
    // session into a worker, rejects the second while the permit is
    // held, and admits a third after the worker returns (release on
    // task return, no decrement code anywhere).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-sem");
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-sem", {username: "guest", password: "pass"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $sem: SEMAPHORE = SEMAPHORE_NEW(1)
        LET $w: HANDLE = ASYNC {
            LET $s1: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $a1: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
            LET $held1: BOOL = $a1.held
            ASSERT_EQ $held1 true
            LET $permit: PERMIT = $a1.permit
            WRITE stage1.txt "admitted"
            LET $hold: HANDLE = ASYNC {
                LET $s2: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
                LET $a2: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
                WRITE rejected.txt "{{ $a2.held }}"
                RETURN $a2.held
            }
            LET $r: BOOL = AWAIT $hold
            ASSERT_EQ $r false
            RETURN $r
        }
        LET $res: BOOL = AWAIT $w
        ASSERT_EQ $res false
        LET $t3: HANDLE = ASYNC { SSH_DEQUEUE($m.server) }
        LET $s3: MAP<ANY> = AWAIT $t3
        LET $a3: MAP<held: BOOL, permit?: PERMIT> = SEMAPHORE_TRY_ACQUIRE($sem)
        WRITE admitted.txt "{{ $a3.held }}"
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client_a = TestClient::connect(addr, "guest", "pass").expect("first client connects");
    poll_file(&probe, "stage1.txt");
    let client_b = TestClient::connect(addr, "guest", "pass").expect("second client connects");
    assert_eq!(poll_file(&probe, "rejected.txt"), "false");
    let client_c = TestClient::connect(addr, "guest", "pass").expect("third client connects");
    handle
        .join()
        .expect("script thread joins")
        .expect("guarded accept loop runs");
    assert_eq!(
        read_trimmed(&probe.join("admitted.txt").unwrap()),
        "true",
        "cap frees after worker return"
    );
    client_a.close();
    client_b.close();
    client_c.close();
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a terminal plus subprocesses"
)]
fn pty_live_resize() {
    // An outer window-change mid-session must resize the local terminal.
    // The session's own cell carries the resize; other sessions never
    // see it. Unix reads `stty size` after a delay; ConPTY answers
    // `mode con` (`ping` is the delay hack: `timeout` can interact
    // with stdin).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-resize");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-resize", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $child: LIST<STRING> = ["sh", "-c", "sleep 4; stty size"]
        [family:windows] {
            $child = ["cmd", "/c", "ping -n 5 127.0.0.1 >nul & mode con"]
        }
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, $child, 0, 0, $in, $out) }
            AWAIT $t
            [family:unix] {
                ASSERT_CONTAINS $out "50 120"
            }
            [family:windows] {
                ASSERT_CONTAINS $out "Lines:"
                ASSERT_CONTAINS $out "50"
                ASSERT_CONTAINS $out "Columns:"
                ASSERT_CONTAINS $out "120"
            }
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr_path = temp.as_guarded_path().join("addr.txt").unwrap();
    let addr: SocketAddr = loop {
        std::thread::sleep(Duration::from_millis(100));
        let text = read_trimmed(&addr_path);
        if !text.is_empty() {
            break text.parse().expect("addr parses");
        }
    };
    drive_outer_resize(addr, 120, 50);
    handle
        .join()
        .expect("script thread joins")
        .expect("resize script completes");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn main_thread_accept_bails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-main-accept");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-main-accept", {username: "guest", password: "pass"})
        LET $in: PIPE
        LET $out: PIPE
        SSH_ACCEPT($m.server, $in, $out)
    "#};
    let err = run_script_with(registry, &root, script).expect_err("main-thread ACCEPT must bail");
    assert!(format!("{err:#}").contains("requires ASYNC"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn teardown_wakes_blocked_accept() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-teardown");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-teardown", {username: "guest", password: "pass"})
        LET $in: PIPE
        LET $out: PIPE
        LET $t: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        SLEEP 2s
        SSH_CLOSE($m.server)
        AWAIT $t
    "#};
    let start = Instant::now();
    let err = run_script_with(registry, &root, script).expect_err("ACCEPT must fail after close");
    // ASYNC task errors now carry the full causal chain across the task
    // boundary, so the inner close reason is visible end to end.
    assert!(format!("{err:#}").contains("closed"), "{err:#}");
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "close must wake the waiter promptly"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn cancel_mid_pump_terminates() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $a: PIPE
        LET $b: PIPE
        LET $t: HANDLE = ASYNC SSH_PUMP($a, $b)
        TIMEOUT 2s AWAIT $t
    "#};
    let start = Instant::now();
    let err = run_script(&root, script).expect_err("silent pump must time out");
    assert!(err.to_string().contains("TIMEOUT"), "{err}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "pump must not outlive its timeout"
    );
}

#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a Tokio runtime plus subprocesses"
)]
fn inner_exit_closes_outer_session() {
    // Nano analog: the RUN leg produces output and exits while the outer
    // client stays connected. The inner death must propagate outer-ward
    // (response flushes, then the channel closes) instead of stranding
    // ACCEPT in its pump with the client hanging.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-inner-exit");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-inner-exit", {username: "test", password: "test123"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $c_in: PIPE
        LET $c_out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $c_in, $c_out) }
        WITH_IO [stdin=$c_out, stdout=$c_in] RUN ["sh", "-c", "echo canned-response"]
        AWAIT $acc
        SSH_CLOSE($m.server)
    "#};
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let result = run_script_with(registry, &root, script);
        let _ = done_tx.send(());
        result
    });
    let addr_path = temp.as_guarded_path().join("addr.txt").unwrap();
    let addr: SocketAddr = loop {
        std::thread::sleep(Duration::from_millis(100));
        let text = read_trimmed(&addr_path);
        if !text.is_empty() {
            break text.parse().expect("addr parses");
        }
    };
    let start = Instant::now();
    let mut client = TestClient::connect(addr, "test", "test123").expect("client connects");
    client
        .read_until(b"canned-response", Duration::from_secs(10))
        .expect("inner output arrives");
    // Stay connected: the script must still finish on its own once the
    // inner leg is gone, and the client must observe the close.
    let closed = done_rx.recv_timeout(Duration::from_secs(15)).is_ok();
    assert!(closed, "script must complete after inner exit");
    assert!(
        start.elapsed() < Duration::from_secs(25),
        "teardown must not stall"
    );
    drop(client);
    handle
        .join()
        .expect("script thread joins")
        .expect("script completes");
}

#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a Tokio runtime plus subprocesses"
)]
fn real_openssh_client_echo_roundtrip() {
    // The real `ssh` binary (password via SSH_ASKPASS, no tty anywhere)
    // through ACCEPT plus an echo PUMP: pins the diagnosis that an
    // interactive password prompt on a synthetic pty is the only
    // openssh configuration that stalls, client-side and pre-auth.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-real-client");
    let script = indoc! {r##"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        WRITE askpass.sh "#!/bin/sh\necho test123\n"
        RUN ["chmod", "+x", "askpass.sh"]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-real-client", {username: "test", password: "test123"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $echo: HANDLE = ASYNC { SSH_PUMP($out, $in) }
        AWAIT $acc
        AWAIT $echo
        SSH_CLOSE($m.server)
    "##};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr_path = temp.as_guarded_path().join("addr.txt").unwrap();
    let addr = loop {
        std::thread::sleep(Duration::from_millis(100));
        let text = read_trimmed(&addr_path);
        if !text.is_empty() {
            break text;
        }
    };
    let askpass = temp
        .as_guarded_path()
        .join("askpass.sh")
        .unwrap()
        .as_path()
        .display()
        .to_string();
    let shell_command = format!(
        "printf 'hello-openssh' | SSH_ASKPASS=\"{askpass}\" SSH_ASKPASS_REQUIRE=force ssh -T -p {} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o PreferredAuthentications=password -o PubkeyAuthentication=no -o LogLevel=ERROR test@{}",
        addr.trim()
            .rsplit_once(':')
            .map(|(_, port)| port)
            .unwrap_or("0"),
        addr.trim()
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or("127.0.0.1"),
    );
    let mut cmd = oxdock_process::CommandBuilder::new("sh");
    cmd.args(["-c".to_string(), shell_command]);
    cmd.current_dir(temp.as_guarded_path().as_path());
    let output = cmd.output().expect("run outer ssh");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello-openssh"),
        "real ssh must round-trip through cat, got: {stdout:?} stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    handle
        .join()
        .expect("script thread joins")
        .expect("script completes after disconnect");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn proxy_outer_to_inner_roundtrip() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $o: SSH_SERVE_INFO = SSH_SERVE("test-ssh-proxy-outer", {username: "outer", password: "outer-pass"})
        LET $i: SSH_SERVE_INFO = SSH_SERVE("test-ssh-proxy-inner", {username: "inner", password: "inner-pass"})
        LET $j_in: PIPE
        LET $j_out: PIPE
        LET $h_inner: HANDLE = ASYNC { SSH_ACCEPT($i.server, $j_in, $j_out) }
        LET $h_iecho: HANDLE = ASYNC { SSH_PUMP($j_out, $j_in) }
        LET $c_in: PIPE
        LET $c_out: PIPE
        LET $i_in: PIPE
        LET $i_out: PIPE
        LET $h_conn: HANDLE = ASYNC { SSH_CONNECT("test-ssh-proxy-inner", "inner", "inner-pass", $i_in, $i_out) }
        LET $h_p1: HANDLE = ASYNC { SSH_PUMP($c_out, $i_in) }
        LET $h_p2: HANDLE = ASYNC { SSH_PUMP($i_out, $c_in) }
        LET $h_acc: HANDLE = ASYNC { SSH_ACCEPT($o.server, $c_in, $c_out) }
        AWAIT $h_acc
        AWAIT $h_conn
        AWAIT $h_p1
        AWAIT $h_p2
        AWAIT $h_inner
        AWAIT $h_iecho
        SSH_CLOSE($o.server)
        SSH_CLOSE($i.server)
    "#};
    // One registry maps both legs; the inner CONNECT goes through the
    // mapped name, never a physical dial.
    let registry = mapped_registry("test-ssh-proxy-outer");
    registry
        .add_cli_mapping("test-ssh-proxy-inner", "127.0.0.1:0")
        .expect("map inner");
    registry.bind_all().expect("bind inner");
    let addr = service_addr(&registry, "test-ssh-proxy-outer");
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(3));
    let mut client =
        TestClient::connect(addr, "outer", "outer-pass").expect("outer client connects");
    client.send(b"proxy-ping").expect("client sends");
    let echoed = client
        .read_until(b"proxy-ping", Duration::from_secs(15))
        .expect("proxied echo returns");
    assert!(
        echoed.windows(10).any(|window| window == b"proxy-ping"),
        "bytes must traverse both SSH legs"
    );
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("proxy script completes after disconnect");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn unknown_serve_option_bails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-bad-option");
    let err = run_script_with(
        registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-bad-option", {username: "guest", password: "pass", frobnicate: 1})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("unknown option must fail");
    assert!(
        err.to_string().contains("unknown option 'frobnicate'"),
        "{err:#}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn non_map_serve_options_bails() {
    // MAP-typed options enforce at the pre-pass: a non-map fails
    // statically with TypeMismatch before any socket opens.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-bad-options");
    let err = run_script_with(
        registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-bad-options", "nope")
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("non-map options must fail");
    assert!(err.to_string().contains("TypeMismatch"), "{err:#}");
    assert!(err.to_string().contains("expected MAP"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn missing_username_bails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-no-user");
    let err = run_script_with(
        registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-no-user", {password: "pass"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("missing username must fail");
    assert!(
        err.to_string().contains("option 'username' is required"),
        "{err:#}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn empty_password_bails() {
    // A server without credentials is meaningless: no silent ephemeral
    // password is generated.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-empty-pass");
    let err = run_script_with(
        registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-empty-pass", {username: "guest", password: ""})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("empty password must fail");
    assert!(
        err.to_string()
            .contains("option 'password' must not be empty"),
        "{err:#}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn missing_password_bails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-no-pass");
    let err = run_script_with(
        registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-no-pass", {username: "guest"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("missing password must fail");
    assert!(
        err.to_string().contains("option 'password' is required"),
        "{err:#}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn non_string_username_bails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-bad-user-type");
    let err = run_script_with(
        registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-bad-user-type", {username: 1, password: "pass"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("non-string username must fail");
    assert!(
        err.to_string()
            .contains("option 'username': expected STRING, got INT"),
        "{err:#}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn unknown_option_names_all_valid_keys() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-frobnicate");
    let err = run_script_with(
        registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-frobnicate", {username: "guest", password: "pass", frobnicate: 1})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("unknown option must fail");
    assert!(
        err.to_string()
            .contains("(expected: username, password, key_path)"),
        "{err:#}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn non_string_key_path_bails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-bad-keypath-type");
    let err = run_script_with(
        registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-bad-keypath-type", {username: "guest", password: "pass", key_path: 1})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("non-string key_path must fail");
    assert!(
        err.to_string().contains("expected STRING, got INT"),
        "{err:#}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn stable_host_key_reused_across_restarts() {
    use oxdock_fs::PathResolver;
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-key-stable");
    let serve_first = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-stable", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
        SSH_CLOSE($m.server)
    "#};
    let serve_second = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-stable", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
        SSH_CLOSE($m.server)
    "#};
    run_script_with(registry.clone(), &root, serve_first).expect("first boot creates the key");
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    let key_path = root.join("ssh_host_key").unwrap();
    let first = resolver.read_file(&key_path).expect("key file exists");
    assert!(
        first.starts_with(b"-----BEGIN OPENSSH PRIVATE KEY-----"),
        "key file is OpenSSH PEM"
    );
    run_script_with(registry.clone(), &root, serve_second).expect("second boot loads the key");
    let second = resolver.read_file(&key_path).expect("key file still there");
    assert_eq!(first, second, "restart must reuse the key, not regenerate");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn loaded_host_key_serves_clients() {
    // Behavioral proof the loaded key is a working host key: create it on
    // one boot, then run a full echo roundtrip on the next. Both boots
    // serve mapped names: key loading happens past acquisition.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-key-first");
    registry
        .add_cli_mapping("test-ssh-key-client", "127.0.0.1:0")
        .expect("map client service");
    registry.bind_all().expect("bind client service");
    run_script_with(
        registry.clone(),
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-first", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect("first boot creates the key");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-client", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $echo: HANDLE = ASYNC { SSH_PUMP($out, $in) }
        AWAIT $acc
        AWAIT $echo
        SSH_CLOSE($m.server)
    "#};
    let addr = service_addr(&registry, "test-ssh-key-client");
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let mut client = TestClient::connect(addr, "guest", "key-pass").expect("client connects");
    client.send(b"stable-key-echo").expect("client sends");
    let echoed = client
        .read_until(b"stable-key-echo", Duration::from_secs(10))
        .expect("echo returns");
    assert!(
        echoed
            .windows(15)
            .any(|window| window == b"stable-key-echo"),
        "echoed bytes must round-trip on the loaded key"
    );
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("script completes after disconnect");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
#[cfg(unix)]
fn created_host_key_has_owner_only_permissions() {
    use oxdock_fs::PathResolver;
    use std::os::unix::fs::PermissionsExt;
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-key-perms");
    run_script_with(registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-perms", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect("boot creates the key");
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    let key_path = root.join("ssh_host_key").unwrap();
    let mode = resolver
        .metadata(&key_path)
        .expect("key stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "created key must be owner-only, got 0{mode:o}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn invalid_host_key_file_bails() {
    use oxdock_fs::PathResolver;
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-key-invalid");
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    let key_path = root.join("ssh_host_key").unwrap();
    resolver.write_file(&key_path, b"not-a-key").unwrap();
    // Owner-only permissions so the test reaches the parse stage (the
    // permission gate runs first on Unix).
    resolver
        .set_permissions_mode_unix(&key_path, 0o600)
        .unwrap();
    let err = run_script_with(registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-invalid", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("invalid key must fail");
    assert!(err.to_string().contains("cannot parse host key"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
#[cfg(unix)]
fn world_readable_host_key_bails() {
    use oxdock_fs::PathResolver;
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-key-world");
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    let key_path = root.join("ssh_host_key").unwrap();
    run_script_with(registry.clone(),
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-world", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect("first boot creates the key");
    resolver
        .set_permissions_mode_unix(&key_path, 0o644)
        .unwrap();
    let err = run_script_with(registry.clone(),
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-world", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("world-readable key must fail");
    let text = format!("{err:#}");
    assert!(
        text.contains("insecure permissions on host key file"),
        "{text}"
    );
    assert!(text.contains("expected 0600"), "{text}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn blank_key_path_keeps_ephemeral_key() {
    use oxdock_fs::PathResolver;
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-key-blank");
    run_script_with(registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-blank", {username: "guest", password: "key-pass", key_path: ""})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect("blank key_path serves");
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    assert!(
        !resolver.exists(&root.join("ssh_host_key").unwrap()),
        "blank key_path must leave no trace"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn escaping_key_path_bails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-key-escape");
    let err = run_script_with(registry,
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-escape", {username: "guest", password: "key-pass", key_path: "../escape_key"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("escaping key_path must fail");
    assert!(err.to_string().contains("escapes the workspace"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn leading_slash_key_path_anchors_to_workspace_root() {
    // A leading `/` means the workspace root (like WRITE), not the
    // filesystem root: both boots share `<root>/slash_key`.
    use oxdock_fs::PathResolver;
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-key-slash");
    let serve_first = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-slash", {username: "guest", password: "key-pass", key_path: "/slash_key"})
        SSH_CLOSE($m.server)
    "#};
    let serve_second = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-key-slash", {username: "guest", password: "key-pass", key_path: "/slash_key"})
        SSH_CLOSE($m.server)
    "#};
    run_script_with(registry.clone(), &root, serve_first).expect("first boot creates the key");
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    let key_path = root.join("slash_key").unwrap();
    let first = resolver.read_file(&key_path).expect("key file exists");
    run_script_with(registry.clone(), &root, serve_second).expect("second boot loads the key");
    let second = resolver.read_file(&key_path).expect("key file still there");
    assert_eq!(first, second, "restart must reuse the key, not regenerate");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn dequeue_exposes_metadata_before_pump() {
    // Control plane before byte plane: the exec command, username, and
    // peer address are visible the moment the request arrives, with no
    // pump started.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-dequeue");
    let addr = service_addr(&registry, "test-ssh-dequeue");
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-dequeue", {username: "guest", password: "pass"})
        LET $t: HANDLE = ASYNC {
            LET $sess: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            WRITE meta.txt "{{ $sess.command }}|{{ $sess.username }}|{{ $sess.addr }}"
        }
        AWAIT $t
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let client = TestClient::connect_exec(addr, "guest", "pass", "health-check")
        .expect("client exec connects");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("script completes after dequeue");
    let meta = read_trimmed(&probe.join("meta.txt").unwrap());
    let mut parts = meta.split('|');
    assert_eq!(
        parts.next(),
        Some("health-check"),
        "exec command routed: {meta}"
    );
    assert_eq!(parts.next(), Some("guest"), "username visible: {meta}");
    let addr = parts.next().unwrap_or_default();
    assert!(
        addr.starts_with("127.0.0.1:"),
        "peer addr is loopback with client port: {meta}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn virtual_command_router() {
    // The router shape: route on the dequeued command, serve a synthetic
    // reply from pure DSL logic (no OS binaries), pump it.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-router");
    let addr = service_addr(&registry, "test-ssh-router");
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-router", {username: "guest", password: "pass"})
        LET $t: HANDLE = ASYNC {
            LET $sess: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            IF $sess.command == "health-check" {
                LET $in: PIPE
                LET $out: PIPE
                LET $p: HANDLE = ASYNC { WITH_IO [stdout=$in] ECHO "System Status: OK" }
                SSH_PUMP_CHANNEL($sess.session, $in, $out)
                AWAIT $p
                WRITE routed.txt "health-check"
            } ELSE {
                WRITE routed.txt "rejected:{{ $sess.command }}"
            }
        }
        AWAIT $t
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let mut client = TestClient::connect_exec(addr, "guest", "pass", "health-check")
        .expect("client exec connects");
    let reply = client
        .read_until(b"System Status: OK", Duration::from_secs(10))
        .expect("virtual command replies");
    assert!(
        reply
            .windows(17)
            .any(|window| window == b"System Status: OK"),
        "synthetic reply must stream"
    );
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("script completes after disconnect");
    assert_eq!(
        read_trimmed(&probe.join("routed.txt").unwrap()),
        "health-check",
        "router took the command branch"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn unknown_virtual_command_rejected_cleanly() {
    // No pump starts for unknown commands: the ELSE branch records the
    // rejection and the script completes without streaming anything.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-reject");
    let addr = service_addr(&registry, "test-ssh-reject");
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-reject", {username: "guest", password: "pass"})
        LET $t: HANDLE = ASYNC {
            LET $sess: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            IF $sess.command == "health-check" {
                WRITE routed.txt "health-check"
            } ELSE {
                WRITE rejected.txt "rejected:{{ $sess.command }}"
            }
        }
        AWAIT $t
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let client = TestClient::connect_exec(addr, "guest", "pass", "frobnicate")
        .expect("client exec connects");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("script completes without pumping");
    assert_eq!(
        read_trimmed(&probe.join("rejected.txt").unwrap()),
        "rejected:frobnicate",
        "unknown command took the ELSE branch"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn session_value_crosses_await_boundary() {
    // Probe: a dequeued session travels through task RETURN + AWAIT
    // binding without hanging or losing its ends.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-session");
    let addr = service_addr(&registry, "test-ssh-session");
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-session", {username: "guest", password: "pass"})
        LET $t: HANDLE = ASYNC {
            LET $sess: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            RETURN $sess.session
        }
        LET $s: SSH_SESSION = AWAIT $t
        WRITE stage.txt "bound"
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let client =
        TestClient::connect_exec(addr, "guest", "pass", "x").expect("client exec connects");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("session crosses await");
    assert_eq!(
        read_trimmed(&probe.join("stage.txt").unwrap()),
        "bound",
        "session value bound after await"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn pump_channel_second_pump_bails() {
    // Session ends are take-once: a second pump on the same session
    // bails instead of splitting bytes across pumps.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-pump-twice");
    let addr = service_addr(&registry, "test-ssh-pump-twice");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-pump-twice", {username: "guest", password: "pass"})
        LET $t: HANDLE = ASYNC {
            LET $sess: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            RETURN $sess.session
        }
        LET $s: SSH_SESSION = AWAIT $t
        LET $in: PIPE
        LET $out: PIPE
        LET $prod: HANDLE = ASYNC { WITH_IO [stdout=$in] ECHO "in-bytes" }
        LET $p1: HANDLE = ASYNC { SSH_PUMP_CHANNEL($s, $in, $out) }
        AWAIT $prod
        AWAIT $p1
        LET $p2: HANDLE = ASYNC { SSH_PUMP_CHANNEL($s, $in, $out) }
        AWAIT $p2
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let client =
        TestClient::connect_exec(addr, "guest", "pass", "x").expect("client exec connects");
    client.close();
    let err = handle
        .join()
        .expect("script thread joins")
        .expect_err("second pump must fail");
    assert!(err.to_string().contains("already pumped"), "{err:#}");
}

#[test]
fn worker_pool_script_parses() {
    // The multi-connection proto shape (LIST_APPEND handles in a WHILE loop, one
    // group AWAIT) must parse with the SSH module registered. Parse-only:
    // the script itself runs forever by design.
    let mut engine = Engine::new();
    engine.register_module(oxdock_ssh_plugin::module());
    let table = engine.module_table();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        WORKSPACE LOCAL

        LET $server: SSH_SERVE_INFO = SSH_SERVE(
            "test-ssh-pool-parse", {
                username: "test",
                password: "test123",
                key_path: "test_key"
            }
        )

        ECHO "ssh proxy listening on {{ $server.addr }} (login test)"

        LET $workers: LIST<ANY> = []
        LET $w: INT = 0
        WHILE $w < 4 {
            LET $h: HANDLE = ASYNC {
                LET $run: BOOL = true
                WHILE $run {
                    LET $c_in: PIPE
                    LET $c_out: PIPE
                    LET $acc: HANDLE = ASYNC { SSH_ACCEPT($server.server, $c_in, $c_out) }

                    LET $proc: HANDLE = ASYNC {
                        WITH_IO [stdin=$c_out, stdout=$c_in] RUN ["ssh", "-o", "BatchMode=yes", "orb"]
                    }

                    AWAIT $acc
                    AWAIT $proc
                }
            }
            LIST_APPEND $workers $h
            $w = $w + 1
        }

        # Block main thread so background workers run indefinitely
        AWAIT $workers
    "#};
    let steps = oxdock_core::parse_script_with_modules(script, table).expect("pool parses");
    // IMPORT is a directive, not a step: INHERIT_ENV, WORKSPACE, LET,
    // ECHO, LET, LET, WHILE, AWAIT.
    assert_eq!(steps.len(), 8, "all pool steps lower");
    assert!(
        matches!(&steps[7].kind, oxdock_core::StepKind::Await { var } if var == "workers"),
        "pool ends with the group await, got {:?}",
        steps[7].kind
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn finite_worker_pool_runs_to_completion() {
    // Same constructs as the infinite proto pool, but workers exit so the
    // group AWAIT terminates: proves LIST_APPEND-in-loop plus LIST AWAIT with the
    // SSH module registered.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $workers: LIST<ANY> = []
            LET $w: INT = 0
            WHILE $w < 4 {
                LET $h: HANDLE = ASYNC { ECHO "w{{ $w }}" }
                LIST_APPEND $workers $h
                $w = $w + 1
            }
            AWAIT $workers
            WRITE done.txt "ok"
        "#},
    )
    .expect("finite pool runs");
    assert_eq!(read_trimmed(&root.join("done.txt").unwrap()), "ok");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn pool_accept_round_robins_across_workers() {
    // Multi-connection shape: 4 workers ACCEPTing on one server, joined by
    // one group AWAIT. Four sequential clients must all round-trip; every
    // leg here is a DSL pump (script pipes), so teardown propagates EOF.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-pool");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-pool", {username: "test", password: "test123"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $workers: LIST<ANY> = []
        LET $w: INT = 0
        WHILE $w < 4 {
            LET $h: HANDLE = ASYNC {
                LET $in: PIPE
                LET $out: PIPE
                LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
                LET $echo: HANDLE = ASYNC { SSH_PUMP($out, $in) }
                AWAIT $acc
                AWAIT $echo
            }
            LIST_APPEND $workers $h
            $w = $w + 1
        }
        AWAIT $workers
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr_path = temp.as_guarded_path().join("addr.txt").unwrap();
    let addr_str = {
        use oxdock_fs::PathResolver;
        let resolver = PathResolver::new(addr_path.root(), addr_path.root()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(s) = resolver.read_to_string(&addr_path) {
                let s = s.trim().to_string();
                if !s.is_empty() {
                    break s;
                }
            }
            assert!(
                Instant::now() < deadline,
                "server never wrote its addr (script hung before serving?)"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let addr: SocketAddr = addr_str.parse().expect("addr parses");
    for i in 0..4 {
        let needle = format!("echo4-ping-{i}");
        let mut client =
            TestClient::connect(addr, "test", "test123").expect("pool client connects");
        client.send(needle.as_bytes()).expect("client sends");
        let echoed = client
            .read_until(needle.as_bytes(), Duration::from_secs(15))
            .expect("pool echo returns");
        assert!(
            echoed.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "session {i} must round-trip"
        );
        client.close();
    }
    handle
        .join()
        .expect("script thread joins")
        .expect("pool script completes after 4 sessions");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
#[cfg(unix)]
fn pool_pty_run_companions_terminate() {
    // The supported multi-connection companion shape: each worker dequeues
    // its own session, pumps its bytes, and runs its companion command
    // against that session's size cell. Per-session work goes through the
    // native PTY runner, never WITH_IO RUN. Every leg stays a
    // script-backed DSL pump, so session teardown propagates EOF and the
    // group AWAIT joins. (Pairing ACCEPT with WITH_IO RUN over the same
    // pipes OS-promotes them: force-close stops working and teardown
    // hangs. That engine trap is tracked separately.)
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-pty-pool");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-pty-pool", {username: "test", password: "test123"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $workers: LIST<ANY> = []
        LET $w: INT = 0
        WHILE $w < 2 {
            LET $h: HANDLE = ASYNC {
                LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
                LET $c_in: PIPE
                LET $c_out: PIPE
                LET $acc: HANDLE = ASYNC { SSH_PUMP_CHANNEL($dq.session, $c_in, $c_out) }
                LET $pty: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["cat"], 0, 0, $c_out, $c_in) }
                AWAIT $acc
                AWAIT $pty
            }
            LIST_APPEND $workers $h
            $w = $w + 1
        }
        AWAIT $workers
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr_path = temp.as_guarded_path().join("addr.txt").unwrap();
    let addr_str = {
        use oxdock_fs::PathResolver;
        let resolver = PathResolver::new(addr_path.root(), addr_path.root()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(s) = resolver.read_to_string(&addr_path) {
                let s = s.trim().to_string();
                if !s.is_empty() {
                    break s;
                }
            }
            assert!(
                Instant::now() < deadline,
                "server never wrote its addr (script hung before serving?)"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let addr: SocketAddr = addr_str.parse().expect("addr parses");
    for i in 0..2 {
        let needle = format!("pty-ping-{i}");
        let mut client =
            TestClient::connect(addr, "test", "test123").expect("pool client connects");
        client.send(needle.as_bytes()).expect("client sends");
        let echoed = client
            .read_until(needle.as_bytes(), Duration::from_secs(15))
            .expect("pool echo returns");
        assert!(
            echoed.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "session {i} must round-trip"
        );
        client.close();
    }
    handle
        .join()
        .expect("script thread joins")
        .expect("pty pool script completes after 2 sessions");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn serve_reports_virtual_echo() {
    // Mapped services: the result MAP echoes the service name alongside
    // the runner-assigned bind.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-virtual-echo");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-virtual-echo", {username: "guest", password: "pass"})
        WRITE addr.txt "{{ $m.addr }}"
        WRITE virt.txt "{{ $m.virtual }}"
        SSH_CLOSE($m.server)
    "#};
    run_script_with(registry, &root, script).expect("serve runs");
    let addr = read_trimmed(&root.join("addr.txt").unwrap());
    assert!(
        addr.starts_with("127.0.0.1:") && !addr.ends_with(":0"),
        "mapped service reports its real bind: {addr}"
    );
    assert_eq!(
        read_trimmed(&root.join("virt.txt").unwrap()),
        "test-ssh-virtual-echo"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs a Tokio runtime for the SSH module")]
fn physical_serve_binds_rejected_in_script() {
    // Scripts carry zero bind authority: physical shapes bail before any
    // key, socket, or thread work.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("127.0.0.1:23348", {username: "guest", password: "pass"})
    "#};
    let err = run_script(&root, script).expect_err("physical bind must fail");
    assert!(err.to_string().contains("logical endpoints"), "{err:#}");
}

/// The isolation mandate for SSH, proved end to end: a service named
/// like a port number never binds the host network. SERVE bails with
/// the CLI mapping hint, and the harness binds the number afterwards
/// to prove nothing took it.
#[test]
#[cfg_attr(miri, ignore = "needs a Tokio runtime for the SSH module")]
fn numeric_serve_never_binds() {
    let (held, number) = hold_numeric_name();
    drop(held);
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = format!(
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $m: SSH_SERVE_INFO = SSH_SERVE("{number}", {{username: "guest", password: "pass"}})
        "#},
        number = number,
    );
    let err = run_script(&root, &script).expect_err("numeric serve must fail");
    assert!(
        err.to_string().contains(&format!(
            "SSH_SERVE service '{number}' has no CLI port mapping; pass -p <host_port>:{number} at invocation"
        )),
        "{err:#}"
    );
    // Nothing bound it: the harness retakes the number. Retry against
    // parallel tests; a plugin theft fails every attempt.
    let mut retaken = false;
    for _ in 0..50 {
        if std::net::TcpListener::bind(format!("127.0.0.1:{number}")).is_ok() {
            retaken = true;
            break;
        }
    }
    assert!(retaken, "number stayed free");
}

/// The dial half: a numeric service never dials either. The harness
/// holds the number open (bound, never accepting) while the script
/// CONNECTs; resolution bails before any socket work, and the harness
/// saw zero inbound connections.
#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn numeric_connect_never_dials() {
    let (held, number) = hold_numeric_name();
    held.set_nonblocking(true).expect("nonblocking");
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = format!(
        indoc! {r#"
            IMPORT [STD, SSH]
         INHERIT_ENV [PATH]
            LET $in: PIPE
            LET $out: PIPE
            LET $c: HANDLE = ASYNC {{
                SSH_CONNECT("{number}", "guest", "pass", $in, $out)
            }}
            AWAIT $c
        "#},
        number = number,
    );
    let err = run_script(&root, &script).expect_err("numeric connect must fail");
    assert!(
        err.to_string().contains(&format!(
            "SSH_CONNECT service '{number}' has no CLI port mapping"
        )),
        "{err:#}"
    );
    // The script finished: any dial it made would sit in the backlog.
    // WouldBlock proves zero inbound connections were ever attempted.
    match held.accept() {
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
        accepted => panic!("numeric service touched the host network: {accepted:?}"),
    }
}

#[test]
#[cfg_attr(miri, ignore = "needs a Tokio runtime for the SSH module")]
fn memory_service_serve_rejected() {
    // SSH needs a TCP socket: serving a memory rendezvous bails loudly
    // instead of hanging a socketless server.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("ssh-mem-svc", {username: "guest", password: "pass"})
    "#};
    let err = run_script(&root, script).expect_err("memory serve must fail");
    assert!(
        err.to_string().contains("has no CLI port mapping"),
        "{err:#}"
    );
}

/// A memory-mapped service fails SSH before execution, not at the
/// serve step: the pre-run check enforces the socket requirement, so
/// even the preceding WRITE never happens.
#[test]
#[cfg_attr(miri, ignore = "needs a Tokio runtime for the SSH module")]
fn memory_mapped_serve_fails_before_execution() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = memory_registry(&["test-ssh-mem-mapped"]);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        WRITE early.txt "must-not-exist"
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-mem-mapped", {username: "guest", password: "pass"})
        SSH_CLOSE($m.server)
    "#};
    let err = run_script_with(registry, &root, script).expect_err("memory serve must fail pre-run");
    assert!(
        err.to_string()
            .contains("SSH_SERVE service 'test-ssh-mem-mapped' has no CLI port mapping"),
        "{err:#}"
    );
    assert!(
        read_trimmed(&root.join("early.txt").unwrap()).is_empty(),
        "no step executed"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs a Tokio runtime for the SSH module")]
fn serve_reclaim_after_close_rebinds() {
    // Close-and-reclaim loops work: release frees the claim, so the next
    // SERVE on the same service re-claims the runner's socket.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-reclaim");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $a: SSH_SERVE_INFO = SSH_SERVE("test-ssh-reclaim", {username: "guest", password: "pass"})
        SSH_CLOSE($a.server)
        LET $b: SSH_SERVE_INFO = SSH_SERVE("test-ssh-reclaim", {username: "guest", password: "pass"})
        ASSERT_EQ $a.addr $b.addr
        SSH_CLOSE($b.server)
    "#};
    run_script_with(registry, &root, script).expect("re-serve after close runs");
}

#[test]
#[cfg_attr(miri, ignore = "needs a Tokio runtime for the SSH module")]
fn offline_serve_and_close_opens_no_socket() {
    // `--offline` runs: SERVE returns a live handle echoing the virtual
    // endpoint with no thread and no socket; CLOSE tears it down.
    let registry = Arc::new(oxdock_net_plugin::EndpointRegistry::new(true));
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-offline-serve", {username: "guest", password: "pass"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $closed: BOOL = SSH_CLOSE($m.server)
        WRITE closed.txt "{{ $closed }}"
    "#};
    run_script_with(registry, &root, script).expect("offline serve runs");
    assert_eq!(
        read_trimmed(&root.join("addr.txt").unwrap()),
        "test-ssh-offline-serve"
    );
    assert_eq!(read_trimmed(&root.join("closed.txt").unwrap()), "true");
}

#[test]
#[cfg_attr(miri, ignore = "needs a Tokio runtime for the SSH module")]
fn offline_connect_bails_before_sockets() {
    // The sandbox gate fires before DNS or dial: a literal loopback dial
    // bails under `--offline`.
    let registry = Arc::new(oxdock_net_plugin::EndpointRegistry::new(true));
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $in: PIPE
        LET $out: PIPE
        LET $c: HANDLE = ASYNC {
            SSH_CONNECT("test-ssh-offline-connect", "guest", "pass", $in, $out)
        }
        AWAIT $c
    "#};
    let err = run_script_with(registry, &root, script).expect_err("offline dial must fail");
    assert!(err.to_string().contains("--offline mode"), "{err:#}");
}

#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus forkpty plus subprocesses"
)]
fn concurrent_guests_keep_their_own_pty_size() {
    // The reported bug: two guests with different terminal sizes must
    // each see their own dimensions. Sizes ride per-session cells from
    // pty-request to pump; a shared cell would let the second guest's
    // request clobber the first (last-writer-wins).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-two-guests");
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-two-guests", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $a: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $b: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $a_in: PIPE
            LET $a_out: PIPE
            LET $b_in: PIPE
            LET $b_out: PIPE
            LET $ta: HANDLE = ASYNC { SSH_PTY_RUN($a.session, ["sh", "-c", "sleep 3; stty size"], 0, 0, $a_in, $a_out) }
            LET $tb: HANDLE = ASYNC { SSH_PTY_RUN($b.session, ["sh", "-c", "sleep 3; stty size"], 0, 0, $b_in, $b_out) }
            AWAIT $ta
            AWAIT $tb
            ASSERT_CONTAINS $a_out "40 100"
            ASSERT_CONTAINS $b_out "50 120"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client_a =
        TestClient::connect_with_pty_dims(addr, "u", "p", 100, 40).expect("guest A connects");
    std::thread::sleep(Duration::from_secs(1));
    let client_b =
        TestClient::connect_with_pty_dims(addr, "u", "p", 120, 50).expect("guest B connects");
    handle
        .join()
        .expect("script thread joins")
        .expect("isolated sizes report per guest");
    client_a.close();
    client_b.close();
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn logical_port_connect_roundtrip() {
    // The client dials a service name, never a physical address: the
    // runner mapped it to loopback before the run. The server sends
    // first: the client side never EOFs its input, so the reply cannot
    // race teardown.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-mapped-roundtrip");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-mapped-roundtrip", {username: "guest", password: "echo-pass"})
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $cin: PIPE
        LET $cout: PIPE
        LET $c: HANDLE = ASYNC { SSH_CONNECT("test-ssh-mapped-roundtrip", "guest", "echo-pass", $cin, $cout) }
        WITH_IO [stdout=$in] ECHO "server-greeting"
        LET $info: MAP<ANY> = INSPECT($cout)
        LET $empty: BOOL = $info.buffer_bytes == 0
        LET $tries: INT = 0
        WHILE $empty {
            SLEEP 100ms
            $info = INSPECT($cout)
            $empty = $info.buffer_bytes == 0
            $tries = $tries + 1
            IF $tries > 100 {
                EXIT 1
            }
        }
        ASSERT_CONTAINS $cout "server-greeting"
        AWAIT $acc
        CANCEL $c
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    handle
        .join()
        .expect("script thread joins")
        .expect("logical connect script completes end to end");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn mapped_name_connect_roundtrip() {
    // Pattern 1 with a CLI-style mapping: the service name resolves to
    // a pre-bound ephemeral socket, so neither side names a physical
    // address or port. The connect side proves the TcpBound arm.
    let registry = Arc::new(oxdock_net_plugin::EndpointRegistry::new(false));
    registry
        .add_mapping(
            &oxdock_net_plugin::EndpointKey::tcp(oxdock_net_plugin::VirtualEndpoint::Name(
                "doc-demo".to_string(),
            )),
            oxdock_net_plugin::BindingSpec::Exposed {
                addr: "127.0.0.1:0".parse().expect("loopback ephemeral"),
            },
        )
        .expect("mapping");
    registry.bind_all().expect("bind_all resolves ephemeral");
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("doc-demo", {username: "guest", password: "echo-pass"})
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $cin: PIPE
        LET $cout: PIPE
        LET $c: HANDLE = ASYNC { SSH_CONNECT("doc-demo", "guest", "echo-pass", $cin, $cout) }
        WITH_IO [stdout=$in] ECHO "server-greeting"
        LET $info: MAP<ANY> = INSPECT($cout)
        LET $empty: BOOL = $info.buffer_bytes == 0
        LET $tries: INT = 0
        WHILE $empty {
            SLEEP 100ms
            $info = INSPECT($cout)
            $empty = $info.buffer_bytes == 0
            $tries = $tries + 1
            IF $tries > 100 {
                EXIT 1
            }
        }
        ASSERT_CONTAINS $cout "server-greeting"
        AWAIT $acc
        CANCEL $c
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &probe, script));
    handle
        .join()
        .expect("script thread joins")
        .expect("mapped connect script completes end to end");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a terminal plus subprocesses"
)]
fn proxy_client_lingers_for_reply_and_status() {
    // Client-side pumps must not end with their own stdin EOF: the
    // echo reply and the exit-status arrive after it. Ending early
    // returns first and disconnects, killing the server mid-reply.
    // Same shape as the README proxy fence: each child reads a stdin
    // line and exits by itself (`cat` on Unix; a console line read on
    // Windows, where children never observe stdin EOF), and the client
    // sends CR with the line because consoles submit on CR only.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-proxy-linger");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-proxy-linger", {username: "guest", password: "pass"})
        LET $child: LIST<STRING> = ["cat"]
        [family:windows] {
            $child = ["cmd", "/v:on", "/c", "(set /p line= && echo got:!line!)"]
        }
        LET $srv: HANDLE = ASYNC {
            LET $sess: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $c_in: PIPE
            LET $c_out: PIPE
            LET $pty: HANDLE = ASYNC { SSH_PTY_RUN($sess.session, $child, 0, 0, $c_out, $c_in) }
            LET $pump: HANDLE = ASYNC { SSH_PUMP_CHANNEL($sess.session, $c_in, $c_out) }
            LET $code: INT = AWAIT $pty
            ASSERT_EQ $code 0
            AWAIT $pump
        }
        LET $cin: PIPE
        LET $cout: PIPE
        LET $c: HANDLE = ASYNC { SSH_CONNECT("test-ssh-proxy-linger", "guest", "pass", $cin, $cout) }
        WITH_IO [stdout=$cin] ECHO "linger-ping\r"
        LET $cDone: MAP<ANY> = AWAIT $c
        ASSERT_EQ $cDone.closed true
        ASSERT_EQ $cDone.exit_status 0
        ASSERT_CONTAINS $cout "linger-ping"
        AWAIT $srv
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    handle
        .join()
        .expect("script thread joins")
        .expect("proxy linger completes");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a terminal plus subprocesses"
)]
fn pty_session_opens_cleared() {
    // Win32-OpenSSH parity: every console session opens cleared and
    // homed, so the child renders onto a known blank screen. The clear
    // leads the stream (written synchronously before the pump starts);
    // `[2J` is plain ASCII, so a contains-assert pins it without an
    // ESC-capable matcher.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-pty-clear");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-pty-clear", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $child: LIST<STRING> = ["sh", "-c", "exit 0"]
        [family:windows] {
            $child = ["cmd", "/c", "exit 0"]
        }
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, $child, 0, 0, $in, $out) }
            LET $code: INT = AWAIT $t
            ASSERT_EQ $code 0
            ASSERT_CONTAINS $out "[2J"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client = TestClient::connect(addr, "u", "p").expect("client connects");
    handle
        .join()
        .expect("script thread joins")
        .expect("cleared session opens");
    client.close();
}

#[test]
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn conhost_setup_query_never_reaches_client_windows() {
    // A real terminal answers `ESC[6n` itself, and that answer lands in
    // the shell as stray input eating the first keystroke. The pump
    // answers conhost locally, so the query must never reach the wire:
    // read through the prompt and assert the whole stream is clean.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-no-dsr-leak");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-no-dsr-leak", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $to_child: PIPE
            LET $from_child: PIPE
            LET $pty: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["powershell.exe", "-NoProfile"], 0, 0, $to_child, $from_child) }
            LET $pump: HANDLE = ASYNC { SSH_PUMP_CHANNEL($dq.session, $from_child, $to_child) }
            LET $code: INT = AWAIT $pty
            AWAIT $pump
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let mut client = TestClient::connect_with_pty(addr, "u", "p").expect("client connects");
    let out = client
        .read_until(b"> ", Duration::from_secs(20))
        .expect("prompt renders");
    assert!(
        !out.windows(3).any(|window| window == b"[6n"),
        "conhost setup query must not reach the client"
    );
    client.send(b"exit\r").expect("client exits shell");
    client
        .wait_close(Duration::from_secs(20))
        .expect("server closes");
    handle
        .join()
        .expect("script thread joins")
        .expect("shell session completes");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a real subprocess"
)]
fn run_exec_binary_roundtrip() {
    // SSH_RUN is the binary-safe leg: bytes travel untouched (no init
    // sequence, no newline translation), stdout and stderr stay split,
    // and stdin EOF closes the child stdin so a stdin-driven child
    // exits on its own (the zero exit-status below proves it).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-run-bin");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-run-bin", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $child: LIST<STRING> = ["sh", "-c", "cat"]
        [family:windows] {
            $child = ["powershell.exe", "-NoProfile", "-Command", "$i=[Console]::OpenStandardInput(); $o=[Console]::OpenStandardOutput(); $i.CopyTo($o)"]
        }
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $to_child: PIPE
            LET $from_child: PIPE
            LET $err: PIPE
            LET $run: HANDLE = ASYNC { SSH_RUN($dq.session, $child, $to_child, $from_child, $err) }
            LET $pump: HANDLE = ASYNC { SSH_PUMP_CHANNEL($dq.session, $from_child, $to_child) }
            LET $code: INT = AWAIT $run
            AWAIT $pump
            ASSERT_EQ $code 0
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let mut client = TestClient::connect(addr, "u", "p").expect("client connects");
    // NUL, high bytes, and a bare LF: any translation layer mangles one
    // of these, and the trailing marker bounds the read.
    let blob: Vec<u8> = b"bin\x00\xffary\nbare-lf\r\ncrlf\r\nEND\r\n".to_vec();
    client.send(&blob).expect("client sends blob");
    let echoed = client
        .read_until(b"END\r\n", Duration::from_secs(15))
        .expect("blob round-trips");
    assert_eq!(echoed, blob, "bytes must travel untouched");
    client.eof().expect("client half-closes");
    assert_eq!(
        client
            .wait_exit_status(Duration::from_secs(15))
            .expect("exit-status arrives"),
        0,
        "stdin EOF must close the child stdin so cat exits 0"
    );
    client
        .wait_close(Duration::from_secs(15))
        .expect("server closes");
    handle
        .join()
        .expect("script thread joins")
        .expect("run session completes");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a real subprocess"
)]
fn run_exec_reports_exit_code() {
    // A fast child must not lose its status to the close: the wire
    // report queues the instant the exit is observed, ahead of the EOF
    // pipe closes.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-run-code");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-run-code", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $child: LIST<STRING> = ["sh", "-c", "exit 3"]
        [family:windows] {
            $child = ["cmd", "/c", "exit 3"]
        }
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $to_child: PIPE
            LET $from_child: PIPE
            LET $err: PIPE
            LET $run: HANDLE = ASYNC { SSH_RUN($dq.session, $child, $to_child, $from_child, $err) }
            LET $pump: HANDLE = ASYNC { SSH_PUMP_CHANNEL($dq.session, $from_child, $to_child) }
            LET $code: INT = AWAIT $run
            AWAIT $pump
            ASSERT_EQ $code 3
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let mut client = TestClient::connect(addr, "u", "p").expect("client connects");
    assert_eq!(
        client
            .wait_exit_status(Duration::from_secs(15))
            .expect("exit-status arrives"),
        3,
        "fast child must report its real code, not 255"
    );
    client
        .wait_close(Duration::from_secs(15))
        .expect("server closes");
    handle
        .join()
        .expect("script thread joins")
        .expect("run session completes");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a real subprocess"
)]
fn run_exec_splits_stderr() {
    // Diagnostics ride their own pipe: stderr bytes must never appear
    // in the stdout stream the client observes.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-run-err");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-run-err", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $child: LIST<STRING> = ["sh", "-c", "echo out; echo oops >&2"]
        [family:windows] {
            $child = ["cmd", "/c", "echo out & echo oops 1>&2"]
        }
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $to_child: PIPE
            LET $from_child: PIPE
            LET $err: PIPE
            LET $run: HANDLE = ASYNC { SSH_RUN($dq.session, $child, $to_child, $from_child, $err) }
            LET $pump: HANDLE = ASYNC { SSH_PUMP_CHANNEL($dq.session, $from_child, $to_child) }
            LET $code: INT = AWAIT $run
            AWAIT $pump
            ASSERT_EQ $code 0
            ASSERT_CONTAINS $err "oops"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let probe = root.clone();
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let mut client = TestClient::connect(addr, "u", "p").expect("client connects");
    let out = client
        .read_until(b"out", Duration::from_secs(15))
        .expect("stdout arrives");
    assert!(
        !out.windows(4).any(|window| window == b"oops"),
        "stderr must never pollute the stdout stream"
    );
    client.eof().expect("client half-closes");
    client
        .wait_close(Duration::from_secs(15))
        .expect("server closes");
    handle
        .join()
        .expect("script thread joins")
        .expect("run session completes");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn pump_channel_close_reaches_client_while_session_held() {
    // Regression: the exit-status control sender must not outlive the
    // pump. The client half-closes, the pump drains, and the server
    // must close the channel while the script still holds the session
    // (parked in SLEEP, long past the drain): a close that waits for
    // the last session clone strands the client.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-pump-close");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-pump-close", {username: "guest", password: "pass"})
        LET $t: HANDLE = ASYNC {
            LET $sess: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            RETURN $sess.session
        }
        LET $s: SSH_SESSION = AWAIT $t
        LET $in: PIPE
        LET $out: PIPE
        LET $p: HANDLE = ASYNC { SSH_PUMP_CHANNEL($s, $in, $out) }
        LET $e: HANDLE = ASYNC { SSH_PUMP($out, $in) }
        SLEEP 12s
        AWAIT $p
        AWAIT $e
        SSH_CLOSE($m.server)
    "#};
    let addr = service_addr(&registry, "test-ssh-pump-close");
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let mut client = TestClient::connect(addr, "guest", "pass").expect("client connects");
    client.send(b"close-me").expect("client sends");
    let echoed = client
        .read_until(b"close-me", Duration::from_secs(10))
        .expect("echo returns");
    assert!(
        echoed.windows(8).any(|window| window == b"close-me"),
        "echoed bytes must round-trip"
    );
    client.eof().expect("client half-closes");
    client
        .wait_close(Duration::from_secs(10))
        .expect("server closes the drained channel");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("close script completes");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn send_exit_status_reaches_exec_client() {
    // No byte pump involved: the status queues on the control sender and
    // the wire writer delivers it, so a client learns the code even when
    // the session never pumps.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-exit-status-raw");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-exit-status-raw", {username: "guest", password: "pass"})
        LET $h: HANDLE = ASYNC {
            LET $sess: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            SSH_SEND_EXIT_STATUS($sess.session, 42)
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let addr = service_addr(&registry, "test-ssh-exit-status-raw");
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let mut client =
        TestClient::connect_exec(addr, "guest", "pass", "probe").expect("client exec connects");
    let code = client
        .wait_exit_status(Duration::from_secs(10))
        .expect("exit-status arrives");
    assert_eq!(code, 42, "client must observe the sent status");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("status script completes");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus a terminal plus subprocesses"
)]
fn pty_child_code_reaches_exec_client() {
    // The proxy shape from the issue: the `SSH_PTY_RUN` exit code is
    // reported on the wire at child exit (ahead of the pump drain), so
    // stock clients report the real code instead of 255. No explicit
    // send: the terminal run reports by itself.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-exit-status-pty");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-exit-status-pty", {username: "guest", password: "pass"})
        LET $child: LIST<STRING> = ["sh", "-c", "exit 7"]
        [family:windows] {
            $child = ["cmd", "/c", "exit 7"]
        }
        LET $h: HANDLE = ASYNC {
            LET $dq: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            LET $c_in: PIPE
            LET $c_out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, $child, 0, 0, $c_out, $c_in) }
            LET $p: HANDLE = ASYNC { SSH_PUMP_CHANNEL($dq.session, $c_in, $c_out) }
            LET $code: INT = AWAIT $t
            ASSERT_EQ $code 7
            AWAIT $p
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let addr = service_addr(&registry, "test-ssh-exit-status-pty");
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let mut client =
        TestClient::connect_exec(addr, "guest", "pass", "probe").expect("client exec connects");
    let code = client
        .wait_exit_status(Duration::from_secs(10))
        .expect("exit-status arrives");
    assert_eq!(code, 7, "client must observe the child exit code");
    client.close();
    handle
        .join()
        .expect("script thread joins")
        .expect("proxy status script completes");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn send_exit_status_rejects_negative_code() {
    // Exit codes are unsigned on the wire: a negative code bails
    // instead of wrapping.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-ssh-exit-status-neg");
    let addr = service_addr(&registry, "test-ssh-exit-status-neg");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        INHERIT_ENV [PATH]
        LET $m: SSH_SERVE_INFO = SSH_SERVE("test-ssh-exit-status-neg", {username: "guest", password: "pass"})
        LET $h: HANDLE = ASYNC {
            LET $sess: SSH_DEQUEUE_INFO = SSH_DEQUEUE($m.server)
            SSH_SEND_EXIT_STATUS($sess.session, -1)
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    std::thread::sleep(Duration::from_secs(2));
    let client =
        TestClient::connect_exec(addr, "guest", "pass", "probe").expect("client exec connects");
    client.close();
    let err = handle
        .join()
        .expect("script thread joins")
        .expect_err("negative code must fail");
    assert!(err.to_string().contains("must not be negative"), "{err:#}");
}
