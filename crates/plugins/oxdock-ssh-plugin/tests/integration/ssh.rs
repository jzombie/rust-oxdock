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

fn read_trimmed(path: &GuardedPath) -> String {
    use oxdock_fs::PathResolver;
    let resolver = PathResolver::new(path.root(), path.root()).unwrap();
    resolver
        .read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
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
            channel.request_shell(true).await?;
            let (reader, writer) = channel.split();
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
        LET $m: MAP = SSH_SERVE("127.0.0.1:0", "guest", "")
        WRITE addr.txt "{{ $m.addr }}"
        LET $closed: BOOL = SSH_CLOSE($m.server)
        WRITE closed.txt "{{ $closed }}"
    "#};
    run_script(&root, script).expect("serve and close runs");
    let addr = read_trimmed(&root.join("addr.txt").unwrap());
    assert!(addr.starts_with("127.0.0.1:"), "{addr}");
    assert!(!addr.ends_with(":0"), "ephemeral port must resolve: {addr}");
    assert_eq!(read_trimmed(&root.join("closed.txt").unwrap()), "true");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads plus a Tokio runtime")]
fn wrong_password_rejected() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("127.0.0.1:23221", "guest", "right-pass")
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
        LET $m: MAP = SSH_SERVE("127.0.0.1:23222", "guest", "echo-pass")
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
fn main_thread_accept_bails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, SSH]
        LET $m: MAP = SSH_SERVE("127.0.0.1:0", "guest", "pass")
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
        LET $m: MAP = SSH_SERVE("127.0.0.1:0", "guest", "pass")
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
        LET $m: MAP = SSH_SERVE("127.0.0.1:0", "test", "test123")
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
        LET $o: MAP = SSH_SERVE("127.0.0.1:23223", "outer", "outer-pass")
        LET $i: MAP = SSH_SERVE("127.0.0.1:23224", "inner", "inner-pass")
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
