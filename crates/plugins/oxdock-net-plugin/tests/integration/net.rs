//! End-to-end NET plugin tests over loopback TCP and memory sessions.
//!
//! Socket tests need real networking, so all of them skip under Miri
//! with a reason. Validation-only, offline, and memory tests run
//! everywhere: no sockets, no threads beyond engine tasks.
//!
//! Fixed virtual ports (`23511`-`23530`) replace the old ephemeral binds:
//! scripts declare fixed logical endpoints now, and each test owns its
//! port so parallel runs never collide.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use indoc::indoc;
use oxdock_core::{Engine, EngineOutput};
use oxdock_fs::{GuardedPath, GuardedTempDir};
use oxdock_net_plugin::{BindingSpec, EndpointRegistry, VirtualEndpoint, module_with_endpoints};

fn guard_root(temp: &GuardedTempDir) -> GuardedPath {
    temp.as_guarded_path().clone()
}

fn run_script(root: &GuardedPath, script: &str) -> anyhow::Result<EngineOutput> {
    let mut engine = Engine::new();
    engine.register_module(oxdock_net_plugin::module());
    engine.run_script(root, script)
}

fn run_script_with(
    registry: Arc<EndpointRegistry>,
    root: &GuardedPath,
    script: &str,
) -> anyhow::Result<EngineOutput> {
    let mut engine = Engine::new();
    engine.register_module(module_with_endpoints(registry));
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

/// Connect with a deadline: the listener task binds synchronously at
/// spawn, but thread scheduling means the test must tolerate a slow
/// start.
fn connect_retry(addr: &str) -> TcpStream {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => return stream,
            Err(err) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
                let _ = err;
            }
            Err(err) => panic!("connect to test listener failed: {err}"),
        }
    }
}

/// Read the addr file the script writes after `NET_LISTEN`.
/// Polls with a deadline so a slow start fails loudly instead of racing
/// the write or hanging the suite.
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
/// Read one `\n`-terminated line with a deadline so helper failures error
/// instead of hanging the suite.
fn read_line_deadline(stream: &mut TcpStream, what: &str) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set timeout");
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => panic!("EOF waiting for {what}"),
            Ok(_) => {
                out.push(byte[0]);
                if byte[0] == b'\n' {
                    return out;
                }
            }
            Err(err) => panic!("read waiting for {what} failed: {err}"),
        }
    }
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP")]
fn listen_reports_loopback_addr_and_virtual_echo() {
    // Unmapped bare ports bind loopback inline: the default with no CLI
    // flags. The result MAP echoes the virtual endpoint alongside.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: MAP = NET_LISTEN("23511", {})
        WRITE addr.txt "{{ $l.addr }}"
        WRITE virt.txt "{{ $l.virtual }}"
        NET_CLOSE($l.listener)
    "#};
    run_script(&root, script).expect("loopback listen runs");
    assert_eq!(
        read_trimmed(&root.join("addr.txt").unwrap()),
        "127.0.0.1:23511"
    );
    assert_eq!(read_trimmed(&root.join("virt.txt").unwrap()), "23511");
}

#[test]
fn physical_binds_rejected_in_script() {
    // Scripts carry zero bind authority: any `host:port` shape bails
    // before any socket work, so this runs under Miri too.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for bind in ["127.0.0.1:23512", "0.0.0.0:23512", "localhost:23512"] {
        let script = format!(
            indoc! {r#"
                IMPORT [STD, NET]
                LET $l: MAP = NET_LISTEN("{bind}", {{}})
            "#},
            bind = bind
        );
        let err = run_script(&root, &script).expect_err("physical bind must fail");
        assert!(err.to_string().contains("logical endpoints"), "{err:#}");
    }
}

#[test]
fn ephemeral_zero_banned_in_script() {
    // Port 0 belongs exclusively to the CLI outer mapping: scripts
    // declare fixed ports or service names.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: MAP = NET_LISTEN("0", {})
    "#};
    let err = run_script(&root, script).expect_err("port 0 must fail");
    assert!(err.to_string().contains("-p 0:"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn accept_connect_roundtrip() {
    // Explicit-pipe echo: the script reads one client line and writes it
    // back; the pump carries both directions.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: MAP = NET_LISTEN("23513", {})
        WRITE addr.txt "{{ $l.addr }}"
        LET $in: PIPE
        LET $out: PIPE
        LET $t: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in, $out, {}) }
        LET $e: HANDLE = ASYNC {
            WITH_IO [stdin=$out] READ_LINE $line
            WITH_IO [stdout=$in] ECHO "{{ $line }}"
        }
        AWAIT $t
        AWAIT $e
        NET_CLOSE($l.listener)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    std::thread::sleep(Duration::from_secs(2));
    let addr = read_addr(&probe);
    let mut client = connect_retry(&addr);
    client.write_all(b"hello\n").expect("client sends");
    let echoed = read_line_deadline(&mut client, "echo");
    assert_eq!(echoed, b"hello\n", "echo must round-trip");
    // Signal EOF by dropping: a shutdown() race with the server's own
    // close trips ENOTCONN on some platforms, while drop always FINs.
    drop(client);
    handle
        .join()
        .expect("script thread joins")
        .expect("script completes after disconnect");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn connect_roundtrip_against_real_server() {
    // Dial-out: a real server thread answers one line and half-closes;
    // the script captures the reply. Raw `host:port` dials still work.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match conn.read(&mut byte).expect("read request") {
                0 => break,
                _ => {
                    request.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
            }
        }
        assert_eq!(request, b"hello\n");
        conn.write_all(b"world\n").expect("write response");
        conn.shutdown(Shutdown::Write).expect("half-close");
        let mut rest = Vec::new();
        conn.read_to_end(&mut rest).expect("drain");
    });
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = format!(
        indoc! {r#"
            IMPORT [STD, NET]
            LET $req: PIPE
            LET $resp: PIPE
            LET $t: HANDLE = ASYNC {{
                NET_CONNECT("127.0.0.1:{port}", $req, $resp, {{}})
            }}
            WITH_IO [stdout=$req] ECHO "hello"
            WITH_IO [stdin=$resp] READ_LINE $got
            AWAIT $t
            WRITE out.txt "{{{{ $got }}}}"
        "#},
        port = port
    );
    run_script(&root, &script).expect("connect roundtrip runs");
    assert_eq!(read_trimmed(&root.join("out.txt").unwrap()), "world");
    server.join().expect("server thread");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn no_half_close_defers_fin() {
    // With `no_half_close`, stdin EOF leaves the socket write side open:
    // the peer sees no FIN and must close first.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut byte = [0u8; 1];
        // The script sends one line then ends its producer; without a
        // FIN the read below must see the line, not EOF.
        let mut got = Vec::new();
        loop {
            match conn.read(&mut byte).expect("read") {
                0 => break,
                _ => {
                    got.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
            }
        }
        assert_eq!(got, b"hi\n");
        conn.write_all(b"bye\n").expect("reply");
    });
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = format!(
        indoc! {r#"
            IMPORT [STD, NET]
            LET $req: PIPE
            LET $resp: PIPE
            LET $t: HANDLE = ASYNC {{
                NET_CONNECT("127.0.0.1:{port}", $req, $resp, {{no_half_close: true}})
            }}
            WITH_IO [stdout=$req] ECHO "hi"
            WITH_IO [stdin=$resp] READ_LINE $got
            AWAIT $t
            WRITE out.txt "{{{{ $got }}}}"
        "#},
        port = port
    );
    run_script(&root, &script).expect("no-half-close runs");
    assert_eq!(read_trimmed(&root.join("out.txt").unwrap()), "bye");
    server.join().expect("server thread");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP")]
fn listen_refuses_while_serving() {
    // Claiming the same service twice fails fast on the second call.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $a: MAP = NET_LISTEN("23514", {})
        LET $b: MAP = NET_LISTEN("23514", {})
        NET_CLOSE($a.listener)
        NET_CLOSE($b.listener)
    "#};
    let err = run_script(&root, script).expect_err("second claim must fail");
    assert!(err.to_string().contains("already claimed"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP")]
fn reclaim_after_close_rebinds() {
    // Close-and-rebind loops work: release frees the slot, so the next
    // LISTEN on the same service binds clean (the slot-take starvation
    // fix: slots are claimed, never consumed).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $a: MAP = NET_LISTEN("23515", {})
        NET_CLOSE($a.listener)
        LET $b: MAP = NET_LISTEN("23515", {})
        WRITE addr.txt "{{ $b.addr }}"
        NET_CLOSE($b.listener)
    "#};
    run_script(&root, script).expect("re-bind after close runs");
    assert_eq!(
        read_trimmed(&root.join("addr.txt").unwrap()),
        "127.0.0.1:23515"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn close_during_accept_returns() {
    // A blocked NET_ACCEPT observes the shutdown flag on its next tick
    // and returns instead of hanging: close from the main flow while a
    // task waits with no client in flight.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: MAP = NET_LISTEN("23516", {})
        LET $in: PIPE
        LET $out: PIPE
        LET $t: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in, $out, {}) }
        SLEEP 500ms
        NET_CLOSE($l.listener)
        AWAIT $t
    "#};
    let err = run_script(&root, script).expect_err("closed accept must fail");
    assert!(err.to_string().contains("listener is closed"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP")]
fn main_thread_accept_bails() {
    // The ASYNC gate fires before any accept loop work.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: MAP = NET_LISTEN("23518", {})
        LET $in: PIPE
        LET $out: PIPE
        NET_ACCEPT($l.listener, $in, $out, {})
    "#};
    let err = run_script(&root, script).expect_err("main-thread accept must fail");
    assert!(format!("{err:#}").contains("requires ASYNC"), "{err:#}");
}

#[test]
fn main_thread_connect_bails() {
    // The ASYNC gate fires before parsing, dialing, or sockets: runs
    // under Miri too.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $in: PIPE
        LET $out: PIPE
        NET_CONNECT("127.0.0.1:9", $in, $out, {})
    "#};
    let err = run_script(&root, script).expect_err("main-thread connect must fail");
    assert!(format!("{err:#}").contains("requires ASYNC"), "{err:#}");
}

#[test]
fn unknown_options_bail() {
    // Option validation precedes all socket work: no Miri ignore needed.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: MAP = NET_LISTEN("23519", {backlog: 5})
    "#};
    let err = run_script(&root, script).expect_err("unknown listen option must fail");
    assert!(
        err.to_string().contains("unknown option 'backlog'"),
        "{err:#}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn proxy_terminates() {
    // Transparent-proxy topology on explicit pipes: downstream client
    // bytes flow up through CONNECT, upstream bytes flow down through
    // ACCEPT, and a client disconnect cascades through both tasks.
    let upstream = TcpListener::bind("127.0.0.1:0").expect("bind");
    let upstream_port = upstream.local_addr().expect("addr").port();
    let helper = std::thread::spawn(move || {
        let (mut conn, _) = upstream.accept().expect("accept");
        conn.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match conn.read(&mut byte).expect("read ping") {
                0 => panic!("EOF before ping"),
                _ => {
                    line.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
            }
        }
        assert_eq!(line, b"ping\n");
        conn.write_all(b"pong\n").expect("write reply");
    });
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let probe = root.clone();
    // The upstream port travels by file (written before the script
    // spawns, so no race); the downstream addr comes back the same way.
    {
        use oxdock_fs::PathResolver;
        let resolver = PathResolver::new(root.root(), root.root()).unwrap();
        let staged = root.join("up.txt").unwrap();
        resolver
            .write_file(&staged, upstream_port.to_string().as_bytes())
            .expect("stage upstream port");
    }
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $up_port: STRING = READ up.txt
        LET $l: MAP = NET_LISTEN("23520", {})
        WRITE addr.txt "{{ $l.addr }}"
        LET $s2c: PIPE
        LET $c2s: PIPE
        LET $ls: HANDLE = ASYNC { NET_ACCEPT($l.listener, $s2c, $c2s, {}) }
        LET $up: HANDLE = ASYNC { NET_CONNECT("127.0.0.1:{{ $up_port }}", $c2s, $s2c, {}) }
        AWAIT $up
        AWAIT $ls
        WRITE done.txt "both-tasks-completed"
        NET_CLOSE($l.listener)
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    let addr = read_addr(&probe);
    let mut client = connect_retry(&addr);
    client.write_all(b"ping\n").expect("write ping");
    let reply = read_line_deadline(&mut client, "pong line");
    assert_eq!(reply, b"pong\n");
    drop(client);
    handle
        .join()
        .expect("script thread joins")
        .expect("proxy completes after disconnect");
    assert_eq!(
        read_trimmed(&probe.join("done.txt").unwrap()),
        "both-tasks-completed"
    );
    helper.join().expect("upstream thread");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn peer_fin_then_late_producer_completes() {
    // The server half-closes immediately; the task must survive the
    // silent period (no premature exit on socket EOF) and still deliver
    // bytes the script produces afterwards.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        conn.shutdown(Shutdown::Write).expect("half-close");
        let mut rest = Vec::new();
        conn.read_to_end(&mut rest).expect("drain");
        assert_eq!(rest, b"late\n");
    });
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = format!(
        indoc! {r#"
            IMPORT [STD, NET]
            LET $req: PIPE
            LET $resp: PIPE
            LET $t: HANDLE = ASYNC {{
                NET_CONNECT("127.0.0.1:{port}", $req, $resp, {{}})
            }}
            SLEEP 300ms
            WITH_IO [stdout=$req] ECHO "late"
            AWAIT $t
        "#},
        port = port
    );
    run_script(&root, &script).expect("late producer runs");
    server.join().expect("server thread");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn cancel_accept_blocked_returns_promptly() {
    // CANCEL of an accept-blocked task returns promptly: the accept
    // loop observes the task token per tick.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: MAP = NET_LISTEN("23521", {})
        LET $in: PIPE
        LET $out: PIPE
        LET $t: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in, $out, {}) }
        CANCEL $t
        NET_CLOSE($l.listener)
    "#};
    let start = Instant::now();
    run_script(&root, script).expect("cancel returns");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "CANCEL of an accept-blocked task must return promptly"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn second_accept_serves_next_client() {
    // Persistent listeners outlive one pump: a second ACCEPT serves the
    // next client. The builtin accept-one shape could never do this.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: MAP = NET_LISTEN("23522", {})
        WRITE addr.txt "{{ $l.addr }}"
        LET $in1: PIPE
        LET $out1: PIPE
        LET $t1: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in1, $out1, {}) }
        LET $e1: HANDLE = ASYNC {
            WITH_IO [stdin=$out1] READ_LINE $line
            WITH_IO [stdout=$in1] ECHO "{{ $line }}"
        }
        AWAIT $t1
        AWAIT $e1
        LET $in2: PIPE
        LET $out2: PIPE
        LET $t2: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in2, $out2, {}) }
        LET $e2: HANDLE = ASYNC {
            WITH_IO [stdin=$out2] READ_LINE $line
            WITH_IO [stdout=$in2] ECHO "{{ $line }}"
        }
        AWAIT $t2
        AWAIT $e2
        NET_CLOSE($l.listener)
        WRITE done.txt "two-sessions"
    "#};
    let handle = std::thread::spawn(move || run_script(&root, script));
    let addr = read_addr(&probe);
    for want in [b"one\n".as_slice(), b"two\n".as_slice()] {
        let mut client = connect_retry(&addr);
        client.write_all(want).expect("client sends");
        let echoed = read_line_deadline(&mut client, "echo");
        assert_eq!(echoed.as_slice(), want, "session echo must round-trip");
        drop(client);
        std::thread::sleep(Duration::from_millis(200));
    }
    handle
        .join()
        .expect("script thread joins")
        .expect("both sessions complete");
    assert_eq!(
        read_trimmed(&probe.join("done.txt").unwrap()),
        "two-sessions"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn prebound_claim_serves_cli_mapped_socket() {
    // The CLI path: a pre-bound ephemeral socket is claimed by LISTEN,
    // and real TCP clients reach it. No script-declared port involved.
    let registry = Arc::new(EndpointRegistry::new(false));
    registry
        .add_mapping(
            &VirtualEndpoint::Port(23530),
            BindingSpec::Exposed {
                addr: "127.0.0.1:0".parse().expect("addr"),
            },
        )
        .expect("mapping");
    registry.bind_all().expect("bind_all resolves ephemeral");
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: MAP = NET_LISTEN("23530", {})
        WRITE addr.txt "{{ $l.addr }}"
        LET $in: PIPE
        LET $out: PIPE
        LET $t: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in, $out, {}) }
        LET $e: HANDLE = ASYNC {
            WITH_IO [stdin=$out] READ_LINE $line
            WITH_IO [stdout=$in] ECHO "{{ $line }}"
        }
        AWAIT $t
        AWAIT $e
        NET_CLOSE($l.listener)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
    let addr = read_addr(&probe);
    assert!(
        addr.starts_with("127.0.0.1:") && !addr.ends_with(":0"),
        "claimed socket reports its real bind: {addr}"
    );
    let mut client = connect_retry(&addr);
    client.write_all(b"mapped\n").expect("client sends");
    let echoed = read_line_deadline(&mut client, "echo");
    assert_eq!(echoed, b"mapped\n");
    drop(client);
    handle
        .join()
        .expect("script thread joins")
        .expect("mapped session completes");
}

#[test]
fn memory_session_roundtrips_with_zero_sockets() {
    // In-process IPC: a named service auto-opens a memory rendezvous with
    // no CLI mapping and no sockets. CONNECT and ACCEPT pump through it
    // like a TCP session. No Miri ignore: pure pipe backends.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: MAP = NET_LISTEN("mem-echo", {})
        WRITE virt.txt "{{ $l.virtual }}"
        LET $sin: PIPE
        LET $sout: PIPE
        LET $t: HANDLE = ASYNC { NET_ACCEPT($l.listener, $sin, $sout, {}) }
        LET $e: HANDLE = ASYNC {
            WITH_IO [stdin=$sout] READ_LINE $line
            WITH_IO [stdout=$sin] ECHO "{{ $line }}"
        }
        LET $cin: PIPE
        LET $cout: PIPE
        LET $c: HANDLE = ASYNC { NET_CONNECT("mem-echo", $cin, $cout, {}) }
        WITH_IO [stdout=$cin] ECHO "hello-memory"
        WITH_IO [stdin=$cout] READ_LINE $got
        AWAIT $c
        AWAIT $t
        AWAIT $e
        NET_CLOSE($l.listener)
        WRITE out.txt "{{ $got }}"
    "#};
    run_script(&root, script).expect("memory session runs");
    assert_eq!(read_trimmed(&root.join("virt.txt").unwrap()), "mem-echo");
    assert_eq!(read_trimmed(&root.join("out.txt").unwrap()), "hello-memory");
}

#[test]
fn memory_client_before_server_rendezvous() {
    // CONNECT before LISTEN: the client auto-creates the memory slot and
    // queues; the later ACCEPT pops it. Order-independent rendezvous.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $cin: PIPE
        LET $cout: PIPE
        LET $c: HANDLE = ASYNC { NET_CONNECT("mem-early", $cin, $cout, {}) }
        SLEEP 500ms
        LET $l: MAP = NET_LISTEN("mem-early", {})
        LET $sin: PIPE
        LET $sout: PIPE
        LET $t: HANDLE = ASYNC { NET_ACCEPT($l.listener, $sin, $sout, {}) }
        LET $e: HANDLE = ASYNC {
            WITH_IO [stdin=$sout] READ_LINE $line
            WITH_IO [stdout=$sin] ECHO "{{ $line }}"
        }
        WITH_IO [stdout=$cin] ECHO "early-bird"
        WITH_IO [stdin=$cout] READ_LINE $got
        AWAIT $c
        AWAIT $t
        AWAIT $e
        NET_CLOSE($l.listener)
        WRITE out.txt "{{ $got }}"
    "#};
    run_script(&root, script).expect("early client runs");
    assert_eq!(read_trimmed(&root.join("out.txt").unwrap()), "early-bird");
}

#[test]
fn memory_queue_full_bails_through_connect() {
    // A missing consumer must fail loudly, not leak: with 64 sessions
    // queued and none accepted, the next CONNECT bails before pumping
    // (so the task cannot hang). No sockets involved.
    use oxdock_net_plugin::MemoryPipePair;
    let registry = Arc::new(EndpointRegistry::new(false));
    let endpoint = VirtualEndpoint::Name("mem-full".to_string());
    registry.ensure_memory_slot(&endpoint);
    for _ in 0..64 {
        registry
            .enqueue_memory_session(&endpoint, MemoryPipePair::fresh())
            .expect("enqueue within cap");
    }
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $cin: PIPE
        LET $cout: PIPE
        LET $c: HANDLE = ASYNC { NET_CONNECT("mem-full", $cin, $cout, {}) }
        AWAIT $c
    "#};
    let err = run_script_with(registry, &root, script).expect_err("full queue must fail");
    assert!(err.to_string().contains("is full"), "{err:#}");
}

#[test]
fn offline_listen_and_close_opens_no_socket() {
    // `--offline` runs: LISTEN returns a live handle echoing the virtual
    // endpoint, CLOSE tears it down, and no socket ever exists. Runs
    // under Miri: zero I/O.
    let registry = Arc::new(EndpointRegistry::new(true));
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: MAP = NET_LISTEN("23531", {})
        WRITE addr.txt "{{ $l.addr }}"
        WRITE virt.txt "{{ $l.virtual }}"
        NET_CLOSE($l.listener)
    "#};
    run_script_with(registry, &root, script).expect("offline listen runs");
    assert_eq!(read_trimmed(&root.join("addr.txt").unwrap()), "23531");
    assert_eq!(read_trimmed(&root.join("virt.txt").unwrap()), "23531");
}

#[test]
fn offline_connect_bails_before_sockets() {
    // The sandbox gate fires before parsing, DNS, or dial: even a
    // loopback literal and a virtual port both bail. Runs under Miri.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for target in ["127.0.0.1:9", "23532"] {
        let registry = Arc::new(EndpointRegistry::new(true));
        let script = format!(
            indoc! {r#"
                IMPORT [STD, NET]
                LET $in: PIPE
                LET $out: PIPE
                LET $c: HANDLE = ASYNC {{
                    NET_CONNECT("{target}", $in, $out, {{}})
                }}
                AWAIT $c
            "#},
            target = target
        );
        let err = run_script_with(registry, &root, &script).expect_err("offline dial must fail");
        assert!(err.to_string().contains("--offline mode"), "{err:#}");
    }
}
