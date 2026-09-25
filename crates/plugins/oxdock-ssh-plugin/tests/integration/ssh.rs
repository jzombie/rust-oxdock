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
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn serve_and_close_roundtrip() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23331", {username: "guest", password: "close-pass"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $closed: BOOL = SSH_CLOSE($m.server)
        WRITE closed.txt "{{ $closed }}"
    "#};
    run_script(&root, script).expect("serve and close runs");
    let addr = read_trimmed(&root.join("addr.txt").unwrap());
    assert_eq!(
        addr, "127.0.0.1:23331",
        "fixed virtual port binds loopback: {addr}"
    );
    assert_eq!(read_trimmed(&root.join("closed.txt").unwrap()), "true");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn wrong_password_rejected() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23221", {username: "guest", password: "right-pass"})
        SLEEP 8s
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    std::thread::sleep(Duration::from_secs(2));
    let addr: SocketAddr = "127.0.0.1:23221".parse().unwrap();
    let wrong = TestClient::connect(addr, "guest", "wrong-pass");
    assert!(wrong.is_err(), "wrong password must be rejected");
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
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23222", {username: "guest", password: "echo-pass"})
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $echo: HANDLE = ASYNC { SSH_PUMP($out, $in) }
        AWAIT $acc
        AWAIT $echo
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    std::thread::sleep(Duration::from_secs(2));
    let addr: SocketAddr = "127.0.0.1:23222".parse().unwrap();
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
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23225", {username: "guest", password: "pty-pass"})
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $echo: HANDLE = ASYNC { SSH_PUMP($out, $in) }
        AWAIT $acc
        AWAIT $echo
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    std::thread::sleep(Duration::from_secs(2));
    let addr: SocketAddr = "127.0.0.1:23225".parse().unwrap();
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
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus forkpty plus subprocesses"
)]
fn pty_explicit_size_unix() {
    // A pty sized explicitly must report exactly that size: `stty size`
    // prints "rows cols". The session comes from DEQUEUE; explicit dims
    // override whatever the client requested.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23334", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: MAP = SSH_DEQUEUE($m.server)
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["sh", "-c", "stty size"], 40, 100, $in, $out) }
            AWAIT $t
            ASSERT_CONTAINS $out "40 100"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
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
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn pty_explicit_size_windows() {
    // Same contract through ConPTY: `mode con` reports the sized buffer.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23335", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: MAP = SSH_DEQUEUE($m.server)
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["cmd", "/c", "mode con"], 40, 100, $in, $out) }
            AWAIT $t
            ASSERT_CONTAINS $out "Lines:"
            ASSERT_CONTAINS $out "40"
            ASSERT_CONTAINS $out "Columns:"
            ASSERT_CONTAINS $out "100"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
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
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus forkpty plus subprocesses"
)]
fn pty_child_sees_session_env_unix() {
    // Block-scoped ENV rides into the pty spawn: the child observes the
    // dequeued session's identity (user, client and server addresses,
    // requested command) through SSH_* vars.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23353", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        ENV PROBE_TOP="top"
        LET $h: HANDLE = ASYNC {
            LET $dq: MAP = SSH_DEQUEUE($m.server)
            ENV SSH_USER="{{ $dq.username }}"
            ENV SSH_CLIENT="{{ $dq.addr }}"
            ENV SSH_SERVER="{{ $m.addr }}"
            ENV SSH_COMMAND="{{ $dq.command }}"
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["sh", "-c", "echo relay:$SSH_USER:$SSH_CLIENT:$SSH_SERVER:$SSH_COMMAND:$PROBE_TOP"], 0, 0, $in, $out) }
            AWAIT $t
            ASSERT_CONTAINS $out "relay:u:127.0.0.1:"
            ASSERT_CONTAINS $out "127.0.0.1:23353"
            ASSERT_CONTAINS $out "probe-command"
            ASSERT_CONTAINS $out ":top"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
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
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn pty_child_sees_session_env_windows() {
    // Same contract through ConPTY: `cmd` expands the relayed vars.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23354", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: MAP = SSH_DEQUEUE($m.server)
            ENV SSH_USER="{{ $dq.username }}"
            ENV SSH_CLIENT="{{ $dq.addr }}"
            ENV SSH_SERVER="{{ $m.addr }}"
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["cmd", "/c", "echo relay:%SSH_USER%:%SSH_CLIENT%:%SSH_SERVER%"], 0, 0, $in, $out) }
            AWAIT $t
            ASSERT_CONTAINS $out "relay:u:127.0.0.1:"
            ASSERT_CONTAINS $out "127.0.0.1:23354"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client = TestClient::connect(addr, "u", "p").expect("client connects");
    handle
        .join()
        .expect("script thread joins")
        .expect("session env relay runs");
    client.close();
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
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23355", {username: "guest", password: "pass"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $sem: SEMAPHORE = SEMAPHORE_NEW(1)
        LET $w: HANDLE = ASYNC {
            LET $s1: MAP = SSH_DEQUEUE($m.server)
            LET $a1: MAP = SEMAPHORE_TRY_ACQUIRE($sem)
            LET $held1: INT = $a1.held
            ASSERT_EQ $held1 1
            LET $permit: PERMIT = $a1.permit
            WRITE stage1.txt "admitted"
            LET $hold: HANDLE = ASYNC {
                LET $s2: MAP = SSH_DEQUEUE($m.server)
                LET $a2: MAP = SEMAPHORE_TRY_ACQUIRE($sem)
                WRITE rejected.txt "{{ $a2.held }}"
                RETURN $a2.held
            }
            LET $r: INT = AWAIT $hold
            ASSERT_EQ $r 0
            RETURN $r
        }
        LET $res: INT = AWAIT $w
        ASSERT_EQ $res 0
        LET $t3: HANDLE = ASYNC { SSH_DEQUEUE($m.server) }
        LET $s3: MAP = AWAIT $t3
        LET $a3: MAP = SEMAPHORE_TRY_ACQUIRE($sem)
        WRITE admitted.txt "{{ $a3.held }}"
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    let addr = read_addr(&probe);
    let addr: SocketAddr = addr.parse().expect("addr parses");
    let client_a = TestClient::connect(addr, "guest", "pass").expect("first client connects");
    poll_file(&probe, "stage1.txt");
    let client_b = TestClient::connect(addr, "guest", "pass").expect("second client connects");
    assert_eq!(poll_file(&probe, "rejected.txt"), "0");
    let client_c = TestClient::connect(addr, "guest", "pass").expect("third client connects");
    handle
        .join()
        .expect("script thread joins")
        .expect("guarded accept loop runs");
    assert_eq!(
        read_trimmed(&probe.join("admitted.txt").unwrap()),
        "1",
        "cap frees after worker return"
    );
    client_a.close();
    client_b.close();
    client_c.close();
}

#[test]
#[cfg(unix)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus forkpty plus subprocesses"
)]
fn pty_live_resize_unix() {
    // An outer window-change mid-session must resize the local terminal:
    // `stty size` read after the change reports the new dimensions. The
    // session's own cell carries the resize; other sessions never see it.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23336", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: MAP = SSH_DEQUEUE($m.server)
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["sh", "-c", "sleep 4; stty size"], 0, 0, $in, $out) }
            AWAIT $t
            ASSERT_CONTAINS $out "50 120"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
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
#[cfg(windows)]
#[cfg_attr(
    miri,
    ignore = "needs loopback TCP plus threads plus ConPTY plus subprocesses"
)]
fn pty_live_resize_windows() {
    // Same contract through ConPTY: `mode con` read after the change
    // reports the new buffer. `ping` is the delay hack (`timeout` can
    // interact with stdin).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23337", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $dq: MAP = SSH_DEQUEUE($m.server)
            LET $in: PIPE
            LET $out: PIPE
            LET $t: HANDLE = ASYNC { SSH_PTY_RUN($dq.session, ["cmd", "/c", "ping -n 5 127.0.0.1 >nul & mode con"], 0, 0, $in, $out) }
            AWAIT $t
            ASSERT_CONTAINS $out "Lines:"
            ASSERT_CONTAINS $out "50"
            ASSERT_CONTAINS $out "Columns:"
            ASSERT_CONTAINS $out "120"
        }
        AWAIT $h
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
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
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23338", {username: "guest", password: "pass"})
        LET $in: PIPE
        LET $out: PIPE
        SSH_ACCEPT($m.server, $in, $out)
    "#};
    let err = run_script(&root, script).expect_err("main-thread ACCEPT must bail");
    assert!(format!("{err:#}").contains("requires ASYNC"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn teardown_wakes_blocked_accept() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23339", {username: "guest", password: "pass"})
        LET $in: PIPE
        LET $out: PIPE
        LET $t: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        SLEEP 2s
        SSH_CLOSE($m.server)
        AWAIT $t
    "#};
    let start = Instant::now();
    let err = run_script(&root, script).expect_err("ACCEPT must fail after close");
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
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23340", {username: "test", password: "test123"})
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
        let result = run_script(&root, script);
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
    let script = indoc! {r##"
        IMPORT [STD, SSH]
        WRITE askpass.sh "#!/bin/sh\necho test123\n"
        RUN ["chmod", "+x", "askpass.sh"]
        LET $m: MAP = SSH_SERVE("23341", {username: "test", password: "test123"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $echo: HANDLE = ASYNC { SSH_PUMP($out, $in) }
        AWAIT $acc
        AWAIT $echo
        SSH_CLOSE($m.server)
    "##};
    let handle = std::thread::spawn(move || run_script(&root, script));
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
        LET $o: MAP = SSH_SERVE("23223", {username: "outer", password: "outer-pass"})
        LET $i: MAP = SSH_SERVE("23224", {username: "inner", password: "inner-pass"})
        LET $j_in: PIPE
        LET $j_out: PIPE
        LET $h_inner: HANDLE = ASYNC { SSH_ACCEPT($i.server, $j_in, $j_out) }
        LET $h_iecho: HANDLE = ASYNC { SSH_PUMP($j_out, $j_in) }
        LET $c_in: PIPE
        LET $c_out: PIPE
        LET $i_in: PIPE
        LET $i_out: PIPE
        LET $h_conn: HANDLE = ASYNC { SSH_CONNECT("127.0.0.1:23224", "inner", "inner-pass", $i_in, $i_out) }
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
    let handle = std::thread::spawn(move || run_script(&root, script));
    std::thread::sleep(Duration::from_secs(3));
    let addr: SocketAddr = "127.0.0.1:23223".parse().unwrap();
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
    let err = run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23342", {username: "guest", password: "pass", frobnicate: 1})
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
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23343", "nope")
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("non-map options must fail");
    assert!(err.to_string().contains("options must be a MAP"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn missing_username_bails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23323", {password: "pass"})
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
    let err = run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23324", {username: "guest", password: ""})
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
    let err = run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23325", {username: "guest"})
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
    let err = run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23326", {username: 1, password: "pass"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("non-string username must fail");
    assert!(
        err.to_string()
            .contains("option 'username' must be a STRING"),
        "{err:#}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn unknown_option_names_all_valid_keys() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23327", {username: "guest", password: "pass", frobnicate: 1})
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
    let err = run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23344", {username: "guest", password: "pass", key_path: 1})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect_err("non-string key_path must fail");
    assert!(err.to_string().contains("must be a STRING"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn stable_host_key_reused_across_restarts() {
    use oxdock_fs::PathResolver;
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let serve_first = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23311", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
        SSH_CLOSE($m.server)
    "#};
    let serve_second = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23312", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
        SSH_CLOSE($m.server)
    "#};
    run_script(&root, serve_first).expect("first boot creates the key");
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    let key_path = root.join("ssh_host_key").unwrap();
    let first = resolver.read_file(&key_path).expect("key file exists");
    assert!(
        first.starts_with(b"-----BEGIN OPENSSH PRIVATE KEY-----"),
        "key file is OpenSSH PEM"
    );
    run_script(&root, serve_second).expect("second boot loads the key");
    let second = resolver.read_file(&key_path).expect("key file still there");
    assert_eq!(first, second, "restart must reuse the key, not regenerate");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn loaded_host_key_serves_clients() {
    // Behavioral proof the loaded key is a working host key: create it on
    // one boot, then run a full echo roundtrip on the next.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23313", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect("first boot creates the key");
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23314", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $echo: HANDLE = ASYNC { SSH_PUMP($out, $in) }
        AWAIT $acc
        AWAIT $echo
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    std::thread::sleep(Duration::from_secs(2));
    let addr: SocketAddr = "127.0.0.1:23314".parse().unwrap();
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
    run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23315", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
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
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    let key_path = root.join("ssh_host_key").unwrap();
    resolver.write_file(&key_path, b"not-a-key").unwrap();
    // Owner-only permissions so the test reaches the parse stage (the
    // permission gate runs first on Unix).
    resolver
        .set_permissions_mode_unix(&key_path, 0o600)
        .unwrap();
    let err = run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23316", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
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
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    let key_path = root.join("ssh_host_key").unwrap();
    run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23317", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
            SSH_CLOSE($m.server)
        "#},
    )
    .expect("first boot creates the key");
    resolver
        .set_permissions_mode_unix(&key_path, 0o644)
        .unwrap();
    let err = run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23318", {username: "guest", password: "key-pass", key_path: "ssh_host_key"})
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
    run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23319", {username: "guest", password: "key-pass", key_path: ""})
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
    let err = run_script(
        &root,
        indoc! {r#"
            IMPORT [STD, SSH]
            LET $m: MAP = SSH_SERVE("23320", {username: "guest", password: "key-pass", key_path: "../escape_key"})
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
    let serve_first = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23321", {username: "guest", password: "key-pass", key_path: "/slash_key"})
        SSH_CLOSE($m.server)
    "#};
    let serve_second = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23322", {username: "guest", password: "key-pass", key_path: "/slash_key"})
        SSH_CLOSE($m.server)
    "#};
    run_script(&root, serve_first).expect("first boot creates the key");
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    let key_path = root.join("slash_key").unwrap();
    let first = resolver.read_file(&key_path).expect("key file exists");
    run_script(&root, serve_second).expect("second boot loads the key");
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
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23328", {username: "guest", password: "pass"})
        LET $t: HANDLE = ASYNC {
            LET $sess: MAP = SSH_DEQUEUE($m.server)
            WRITE meta.txt "{{ $sess.command }}|{{ $sess.username }}|{{ $sess.addr }}"
        }
        AWAIT $t
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    std::thread::sleep(Duration::from_secs(2));
    let addr: SocketAddr = "127.0.0.1:23328".parse().unwrap();
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
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23329", {username: "guest", password: "pass"})
        LET $t: HANDLE = ASYNC {
            LET $sess: MAP = SSH_DEQUEUE($m.server)
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
    let handle = std::thread::spawn(move || run_script(&root, script));
    std::thread::sleep(Duration::from_secs(2));
    let addr: SocketAddr = "127.0.0.1:23329".parse().unwrap();
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
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23330", {username: "guest", password: "pass"})
        LET $t: HANDLE = ASYNC {
            LET $sess: MAP = SSH_DEQUEUE($m.server)
            IF $sess.command == "health-check" {
                WRITE routed.txt "health-check"
            } ELSE {
                WRITE rejected.txt "rejected:{{ $sess.command }}"
            }
        }
        AWAIT $t
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    std::thread::sleep(Duration::from_secs(2));
    let addr: SocketAddr = "127.0.0.1:23330".parse().unwrap();
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
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23332", {username: "guest", password: "pass"})
        LET $t: HANDLE = ASYNC {
            LET $sess: MAP = SSH_DEQUEUE($m.server)
            RETURN $sess.session
        }
        LET $s: SSH_SESSION = AWAIT $t
        WRITE stage.txt "bound"
        SSH_CLOSE($m.server)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    std::thread::sleep(Duration::from_secs(2));
    let addr: SocketAddr = "127.0.0.1:23332".parse().unwrap();
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
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23333", {username: "guest", password: "pass"})
        LET $t: HANDLE = ASYNC {
            LET $sess: MAP = SSH_DEQUEUE($m.server)
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
    let handle = std::thread::spawn(move || run_script(&root, script));
    std::thread::sleep(Duration::from_secs(2));
    let addr: SocketAddr = "127.0.0.1:23333".parse().unwrap();
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
        WORKSPACE LOCAL

        LET $server: MAP = SSH_SERVE(
            "127.0.0.1:2241", {
                username: "test",
                password: "test123",
                key_path: "test_key"
            }
        )

        ECHO "ssh proxy listening on {{ $server.addr }} (login test)"

        LET $workers: LIST = []
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
    // IMPORT is a directive, not a step: WORKSPACE, LET, ECHO, LET, LET,
    // WHILE, AWAIT.
    assert_eq!(steps.len(), 7, "all pool steps lower");
    assert!(
        matches!(&steps[6].kind, oxdock_core::StepKind::Await { var } if var == "workers"),
        "pool ends with the group await, got {:?}",
        steps[6].kind
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
            LET $workers: LIST = []
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
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23345", {username: "test", password: "test123"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $workers: LIST = []
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
    let handle = std::thread::spawn(move || run_script(&root, script));
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
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23346", {username: "test", password: "test123"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $workers: LIST = []
        LET $w: INT = 0
        WHILE $w < 2 {
            LET $h: HANDLE = ASYNC {
                LET $dq: MAP = SSH_DEQUEUE($m.server)
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
    let handle = std::thread::spawn(move || run_script(&root, script));
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
    // Virtual endpoints: the result MAP echoes the logical endpoint
    // alongside the physical bind.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23347", {username: "guest", password: "pass"})
        WRITE addr.txt "{{ $m.addr }}"
        WRITE virt.txt "{{ $m.virtual }}"
        SSH_CLOSE($m.server)
    "#};
    run_script(&root, script).expect("serve runs");
    assert_eq!(
        read_trimmed(&root.join("addr.txt").unwrap()),
        "127.0.0.1:23347"
    );
    assert_eq!(read_trimmed(&root.join("virt.txt").unwrap()), "23347");
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
        LET $m: MAP = SSH_SERVE("127.0.0.1:23348", {username: "guest", password: "pass"})
    "#};
    let err = run_script(&root, script).expect_err("physical bind must fail");
    assert!(err.to_string().contains("logical endpoints"), "{err:#}");
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
        LET $m: MAP = SSH_SERVE("ssh-mem-svc", {username: "guest", password: "pass"})
    "#};
    let err = run_script(&root, script).expect_err("memory serve must fail");
    assert!(err.to_string().contains("needs a TCP socket"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs a Tokio runtime for the SSH module")]
fn serve_reclaim_after_close_rebinds() {
    // Close-and-reserve loops work: release frees the slot for re-serve.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $a: MAP = SSH_SERVE("23351", {username: "guest", password: "pass"})
        SSH_CLOSE($a.server)
        LET $b: MAP = SSH_SERVE("23351", {username: "guest", password: "pass"})
        WRITE addr.txt "{{ $b.addr }}"
        SSH_CLOSE($b.server)
    "#};
    run_script(&root, script).expect("re-serve after close runs");
    assert_eq!(
        read_trimmed(&root.join("addr.txt").unwrap()),
        "127.0.0.1:23351"
    );
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
        LET $m: MAP = SSH_SERVE("23349", {username: "guest", password: "pass"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $closed: BOOL = SSH_CLOSE($m.server)
        WRITE closed.txt "{{ $closed }}"
    "#};
    run_script_with(registry, &root, script).expect("offline serve runs");
    assert_eq!(read_trimmed(&root.join("addr.txt").unwrap()), "23349");
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
        LET $in: PIPE
        LET $out: PIPE
        LET $c: HANDLE = ASYNC {
            SSH_CONNECT("127.0.0.1:23350", "guest", "pass", $in, $out)
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
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23352", {username: "u", password: "p"})
        WRITE addr.txt "{{ $m.addr }}"
        LET $h: HANDLE = ASYNC {
            LET $a: MAP = SSH_DEQUEUE($m.server)
            LET $b: MAP = SSH_DEQUEUE($m.server)
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
    let handle = std::thread::spawn(move || run_script(&root, script));
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
    // Pattern 1: the client dials the logical port, never a physical
    // address. Unmapped ports default to loopback, so no `127.0.0.1`
    // appears anywhere in this script. The server sends first: the
    // client side never EOFs its input, so the reply cannot race
    // teardown.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("23541", {username: "guest", password: "echo-pass"})
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $cin: PIPE
        LET $cout: PIPE
        LET $c: HANDLE = ASYNC { SSH_CONNECT("23541", "guest", "echo-pass", $cin, $cout) }
        WITH_IO [stdout=$in] ECHO "server-greeting"
        LET $info: MAP = INSPECT($cout)
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
    let handle = std::thread::spawn(move || run_script(&root, script));
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
            &oxdock_net_plugin::EndpointKey::tcp(oxdock_net_plugin::VirtualEndpoint::Name("doc-demo".to_string())),
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
        LET $m: MAP = SSH_SERVE("doc-demo", {username: "guest", password: "echo-pass"})
        LET $in: PIPE
        LET $out: PIPE
        LET $acc: HANDLE = ASYNC { SSH_ACCEPT($m.server, $in, $out) }
        LET $cin: PIPE
        LET $cout: PIPE
        LET $c: HANDLE = ASYNC { SSH_CONNECT("doc-demo", "guest", "echo-pass", $cin, $cout) }
        WITH_IO [stdout=$in] ECHO "server-greeting"
        LET $info: MAP = INSPECT($cout)
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
