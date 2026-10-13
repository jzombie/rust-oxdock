//! End-to-end NET plugin tests over loopback TCP and memory sessions.
//!
//! Socket tests need real networking, so all of them skip under Miri
//! with a reason. Validation-only, offline, and memory tests run
//! everywhere: no sockets, no threads beyond engine tasks.
//!
//! Socket tests run against runner-mapped services (`mapped_registry`):
//! scripts name services, the harness binds ephemeral loopback, so no
//! fixed ports appear anywhere and parallel runs never collide.
//! Validation-only, offline, and memory tests keep fixed virtual names:
//! pure identifiers, never bound. The anti-binding proofs at the bottom
//! are the only tests naming numbers, and they assert those numbers
//! never touch the host network.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use indoc::indoc;
use oxdock_core::{Engine, EngineOutput};
use oxdock_fs::{GuardedPath, GuardedTempDir};
use oxdock_net_plugin::{BindingSpec, EndpointKey, EndpointRegistry, module_with_endpoints};

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

/// Runner-mapped memory registry for services: in-process rendezvous
/// by explicit runner designation, never by fallback.
fn memory_registry(services: &[&str]) -> Arc<EndpointRegistry> {
    let registry = Arc::new(EndpointRegistry::new(false));
    for service in services {
        registry
            .add_mapping(
                &EndpointKey::tcp(oxdock_net_plugin::VirtualEndpoint::Name(
                    service.to_string(),
                )),
                BindingSpec::Memory,
            )
            .expect("map memory service");
    }
    registry
}

/// Runner-mapped registry for one service: the harness binds ephemeral
/// loopback, the script only ever names the service. Mirrors a CLI
/// `-p 127.0.0.1:0:<service>` invocation.
fn mapped_registry(service: &str) -> Arc<EndpointRegistry> {
    let registry = Arc::new(EndpointRegistry::new(false));
    registry
        .add_cli_mapping(service, "127.0.0.1:0")
        .expect("map service");
    registry.bind_all().expect("bind service");
    registry
}

/// Map one service to an explicit address (a harness-held listener).
/// The slot is dial-only: resolvable, never bound, never claimable.
fn mapped_registry_to(service: &str, addr: &str) -> Arc<EndpointRegistry> {
    let registry = Arc::new(EndpointRegistry::new(false));
    registry
        .add_mapping(
            &EndpointKey::tcp(oxdock_net_plugin::VirtualEndpoint::Name(
                service.to_string(),
            )),
            BindingSpec::DialOnly {
                addr: addr.parse().expect("dial addr"),
            },
        )
        .expect("map service");
    registry
}

/// Hold a numeric service name nobody else holds: bind ephemeral, read
/// the port back, and rebind it, retrying against parallel tests racing
/// for the same port. Used ONLY by the anti-binding proofs, where the
/// script must name a number the host never bound. A plugin that stole
/// the number fails every retry, so theft still fails deterministically.
fn hold_numeric_name() -> (TcpListener, String) {
    for _ in 0..50 {
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("ephemeral bind")
            .local_addr()
            .expect("addr")
            .port();
        match TcpListener::bind(format!("127.0.0.1:{port}")) {
            Ok(held) => return (held, port.to_string()),
            Err(_) => continue,
        }
    }
    panic!("could not hold a numeric service name");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP")]
fn listen_reports_loopback_addr_and_virtual_echo() {
    // A runner-mapped service reports its real bind; the result MAP
    // echoes the virtual name alongside.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-listen-echo");
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: NET_LISTEN_INFO = NET_LISTEN("test-listen-echo", {})
        WRITE addr.txt "{{ $l.addr }}"
        WRITE virt.txt "{{ $l.virtual }}"
        NET_CLOSE($l.listener)
    "#};
    run_script_with(registry, &root, script).expect("loopback listen runs");
    let addr = read_trimmed(&root.join("addr.txt").unwrap());
    assert!(
        addr.starts_with("127.0.0.1:") && !addr.ends_with(":0"),
        "mapped service reports its real bind: {addr}"
    );
    assert_eq!(
        read_trimmed(&root.join("virt.txt").unwrap()),
        "test-listen-echo"
    );
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
                LET $l: NET_LISTEN_INFO = NET_LISTEN("{bind}", {{}})
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
        LET $l: NET_LISTEN_INFO = NET_LISTEN("0", {})
    "#};
    let err = run_script(&root, script).expect_err("port 0 must fail");
    assert!(err.to_string().contains("-p 0:"), "{err:#}");
}

/// The isolation mandate, proved end to end: a service named like a
/// port number is a hard error before execution, never a bind or dial.
/// The harness holds the number open (bound, never accepting) while the
/// script fails the pre-run check; then it asserts the harness saw zero
/// inbound connections, the script wrote zero output files, and the
/// number stayed free.
#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn numeric_service_names_never_touch_the_host() {
    let (held, name) = hold_numeric_name();
    let number = name.clone();
    held.set_nonblocking(true).expect("nonblocking");
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: NET_LISTEN_INFO = NET_LISTEN("__SVC__", {})
        LET $sin: PIPE
        LET $sout: PIPE
        LET $t: HANDLE = ASYNC { NET_ACCEPT($l.listener, $sin, $sout, {}) }
        LET $e: HANDLE = ASYNC {
            WITH_IO [stdin=$sout] READ_LINE $line
            WITH_IO [stdout=$sin] ECHO "{{ $line }}"
        }
        LET $cin: PIPE
        LET $cout: PIPE
        LET $c: HANDLE = ASYNC { NET_CONNECT("__SVC__", $cin, $cout, {}) }
        WITH_IO [stdout=$cin] ECHO "number-echo"
        WITH_IO [stdin=$cout] READ_LINE $got
        AWAIT $c
        AWAIT $t
        AWAIT $e
        NET_CLOSE($l.listener)
        WRITE out.txt "{{ $got }}"
    "#}
    .replace("__SVC__", &name);
    let err = run_script(&root, &script).expect_err("unmapped numeric must fail pre-run");
    assert!(
        err.to_string().contains(&format!(
            "NET_LISTEN service '{name}' has no CLI port mapping"
        )),
        "{err:#}"
    );
    // Pre-run means pre-execution: the echo write never happened.
    assert!(
        read_trimmed(&root.join("out.txt").unwrap()).is_empty(),
        "no step executed"
    );
    // The script finished failing: any dial it made would sit in the
    // backlog. WouldBlock proves zero inbound connections.
    match held.accept() {
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
        accepted => panic!("numeric service touched the host network: {accepted:?}"),
    }
    // Nothing bound it either: the harness retakes the number. Retry
    // against parallel tests; a plugin theft fails every attempt.
    drop(held);
    let mut retaken = false;
    for _ in 0..50 {
        if TcpListener::bind(format!("127.0.0.1:{number}")).is_ok() {
            retaken = true;
            break;
        }
    }
    assert!(retaken, "number stayed free");
}

/// Introspection on a numeric service reports no physical port: serve
/// and dial flavors fail pre-run with the CLI mapping hint, while
/// `NET_PORT`/`NET_ADDR` alone fail with the not-mapped message. Either
/// way no host number is ever observed.
#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP")]
fn numeric_introspection_reports_no_physical_port() {
    let (_held, number) = hold_numeric_name();
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = format!(
        indoc! {r#"
            IMPORT [STD, NET]
            LET $l: NET_LISTEN_INFO = NET_LISTEN("{number}", {{}})
        "#},
        number = number,
    );
    let err = run_script(&root, &script).expect_err("unmapped serve must fail pre-run");
    assert!(
        err.to_string().contains(&format!(
            "NET_LISTEN service '{number}' has no CLI port mapping"
        )),
        "{err:#}"
    );
    for (func, ty) in [("NET_PORT", "INT"), ("NET_ADDR", "STRING")] {
        let script = format!(
            indoc! {r#"
                IMPORT [STD, NET]
                LET $x: {ty} = {func}("{number}")
            "#},
            number = number,
            ty = ty,
            func = func,
        );
        let err = run_script(&root, &script).expect_err("unmapped lookup must fail pre-run");
        assert!(
            err.to_string().contains(&format!(
                "{func} service '{number}' is not mapped by the runner"
            )),
            "{err:#}"
        );
    }
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn accept_connect_roundtrip() {
    // Explicit-pipe echo: the script reads one client line and writes it
    // back; the pump carries both directions.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let probe = root.clone();
    let registry = mapped_registry("test-accept-echo");
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: NET_LISTEN_INFO = NET_LISTEN("test-accept-echo", {})
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
    // the script dials it through a runner-mapped service name and
    // captures the reply.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let server_addr = listener.local_addr().expect("addr").to_string();
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
    let registry = mapped_registry_to("test-real-dial", &server_addr);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $req: PIPE
        LET $resp: PIPE
        LET $t: HANDLE = ASYNC { NET_CONNECT("test-real-dial", $req, $resp, {}) }
        WITH_IO [stdout=$req] ECHO "hello"
        WITH_IO [stdin=$resp] READ_LINE $got
        AWAIT $t
        WRITE out.txt "{{ $got }}"
    "#};
    run_script_with(registry, &root, script).expect("connect roundtrip runs");
    assert_eq!(read_trimmed(&root.join("out.txt").unwrap()), "world");
    server.join().expect("server thread");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn no_half_close_defers_fin() {
    // With `no_half_close`, stdin EOF leaves the socket write side open:
    // the peer sees no FIN and must close first.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let server_addr = listener.local_addr().expect("addr").to_string();
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
    let registry = mapped_registry_to("test-no-fin", &server_addr);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $req: PIPE
        LET $resp: PIPE
        LET $t: HANDLE = ASYNC { NET_CONNECT("test-no-fin", $req, $resp, {no_half_close: true}) }
        WITH_IO [stdout=$req] ECHO "hi"
        WITH_IO [stdin=$resp] READ_LINE $got
        AWAIT $t
        WRITE out.txt "{{ $got }}"
    "#};
    run_script_with(registry, &root, script).expect("no-half-close runs");
    assert_eq!(read_trimmed(&root.join("out.txt").unwrap()), "bye");
    server.join().expect("server thread");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP")]
fn listen_refuses_while_serving() {
    // Claiming the same service twice fails fast on the second call.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-double-claim");
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $a: NET_LISTEN_INFO = NET_LISTEN("test-double-claim", {})
        LET $b: NET_LISTEN_INFO = NET_LISTEN("test-double-claim", {})
        NET_CLOSE($a.listener)
        NET_CLOSE($b.listener)
    "#};
    let err = run_script_with(registry, &root, script).expect_err("second claim must fail");
    assert!(err.to_string().contains("already claimed"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP")]
fn reclaim_after_close_rebinds() {
    // Close-and-reclaim loops work: release frees the claim, so the next
    // LISTEN on the same service re-claims the runner's socket (same
    // address both times; the slot-take starvation fix: slots are
    // claimed, never consumed).
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-reclaim");
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $a: NET_LISTEN_INFO = NET_LISTEN("test-reclaim", {})
        NET_CLOSE($a.listener)
        LET $b: NET_LISTEN_INFO = NET_LISTEN("test-reclaim", {})
        ASSERT_EQ $a.addr $b.addr
        NET_CLOSE($b.listener)
    "#};
    run_script_with(registry, &root, script).expect("re-claim after close runs");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn close_during_accept_returns() {
    // A blocked NET_ACCEPT observes the shutdown flag on its next tick
    // and returns instead of hanging: close from the main flow while a
    // task waits with no client in flight.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-close-accept");
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: NET_LISTEN_INFO = NET_LISTEN("test-close-accept", {})
        LET $in: PIPE
        LET $out: PIPE
        LET $t: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in, $out, {}) }
        SLEEP 500ms
        NET_CLOSE($l.listener)
        AWAIT $t
    "#};
    let err = run_script_with(registry, &root, script).expect_err("closed accept must fail");
    assert!(err.to_string().contains("listener is closed"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP")]
fn main_thread_accept_bails() {
    // The ASYNC gate fires before any accept loop work.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-main-accept");
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: NET_LISTEN_INFO = NET_LISTEN("test-main-accept", {})
        LET $in: PIPE
        LET $out: PIPE
        NET_ACCEPT($l.listener, $in, $out, {})
    "#};
    let err = run_script_with(registry, &root, script).expect_err("main-thread accept must fail");
    assert!(format!("{err:#}").contains("requires ASYNC"), "{err:#}");
}

#[test]
fn main_thread_connect_bails() {
    // The ASYNC gate fires before parsing, dialing, or sockets: runs
    // under Miri too. Memory-mapped so pre-run passes the service
    // through to the gate instead of failing it unmapped.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = memory_registry(&["test-gate-connect"]);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $in: PIPE
        LET $out: PIPE
        NET_CONNECT("test-gate-connect", $in, $out, {})
    "#};
    let err = run_script_with(registry, &root, script).expect_err("main-thread connect must fail");
    assert!(format!("{err:#}").contains("requires ASYNC"), "{err:#}");
}

#[test]
fn unknown_options_bail() {
    // Option validation precedes all socket work: no Miri ignore needed.
    // The service is memory-mapped so validation (not binding) fails.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = memory_registry(&["test-bad-option"]);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: NET_LISTEN_INFO = NET_LISTEN("test-bad-option", {backlog: 5})
    "#};
    let err =
        run_script_with(registry, &root, script).expect_err("unknown listen option must fail");
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
    let upstream_addr = upstream.local_addr().expect("addr").to_string();
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
    // Both ends are runner-mapped services: the downstream listener gets
    // ephemeral loopback, the upstream dial target points at the harness
    // listener (dial-only: the runner never binds what it does not own).
    // The downstream addr comes back by file.
    let registry = Arc::new(EndpointRegistry::new(false));
    registry
        .add_cli_mapping("test-proxy-down", "127.0.0.1:0")
        .expect("map downstream");
    registry
        .add_mapping(
            &EndpointKey::tcp(oxdock_net_plugin::VirtualEndpoint::Name(
                "test-proxy-up".to_string(),
            )),
            BindingSpec::DialOnly {
                addr: upstream_addr.parse().expect("upstream addr"),
            },
        )
        .expect("map upstream");
    registry.bind_all().expect("bind downstream");
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: NET_LISTEN_INFO = NET_LISTEN("test-proxy-down", {})
        WRITE addr.txt "{{ $l.addr }}"
        LET $s2c: PIPE
        LET $c2s: PIPE
        LET $ls: HANDLE = ASYNC { NET_ACCEPT($l.listener, $s2c, $c2s, {}) }
        LET $up: HANDLE = ASYNC { NET_CONNECT("test-proxy-up", $c2s, $s2c, {}) }
        AWAIT $up
        AWAIT $ls
        WRITE done.txt "both-tasks-completed"
        NET_CLOSE($l.listener)
    "#};
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
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
    let server_addr = listener.local_addr().expect("addr").to_string();
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
    let registry = mapped_registry_to("test-peer-fin", &server_addr);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $req: PIPE
        LET $resp: PIPE
        LET $t: HANDLE = ASYNC { NET_CONNECT("test-peer-fin", $req, $resp, {}) }
        SLEEP 300ms
        WITH_IO [stdout=$req] ECHO "late"
        AWAIT $t
    "#};
    run_script_with(registry, &root, script).expect("late producer runs");
    server.join().expect("server thread");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn cancel_accept_blocked_returns_promptly() {
    // CANCEL of an accept-blocked task returns promptly: the accept
    // loop observes the task token per tick.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = mapped_registry("test-cancel-accept");
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: NET_LISTEN_INFO = NET_LISTEN("test-cancel-accept", {})
        LET $in: PIPE
        LET $out: PIPE
        LET $t: HANDLE = ASYNC { NET_ACCEPT($l.listener, $in, $out, {}) }
        CANCEL $t
        NET_CLOSE($l.listener)
    "#};
    let start = Instant::now();
    run_script_with(registry, &root, script).expect("cancel returns");
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
    let registry = mapped_registry("test-second-accept");
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: NET_LISTEN_INFO = NET_LISTEN("test-second-accept", {})
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
    let handle = std::thread::spawn(move || run_script_with(registry, &root, script));
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
    // and real TCP clients reach it. The script names a service; the
    // runner owns the bind.
    let registry = mapped_registry("test-prebound");
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let probe = root.clone();
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: NET_LISTEN_INFO = NET_LISTEN("test-prebound", {})
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
    // In-process IPC on an explicitly-mapped memory service: CONNECT
    // and ACCEPT pump through it like a TCP session. No Miri ignore:
    // pure pipe backends.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = memory_registry(&["mem-echo"]);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $l: NET_LISTEN_INFO = NET_LISTEN("mem-echo", {})
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
    run_script_with(registry, &root, script).expect("memory session runs");
    assert_eq!(read_trimmed(&root.join("virt.txt").unwrap()), "mem-echo");
    assert_eq!(read_trimmed(&root.join("out.txt").unwrap()), "hello-memory");
}

#[test]
fn memory_client_before_server_rendezvous() {
    // CONNECT before LISTEN on an explicitly-mapped memory service: the
    // client queues; the later ACCEPT pops it. Order-independent
    // rendezvous, still by runner designation.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let registry = memory_registry(&["mem-early"]);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $cin: PIPE
        LET $cout: PIPE
        LET $c: HANDLE = ASYNC { NET_CONNECT("mem-early", $cin, $cout, {}) }
        SLEEP 500ms
        LET $l: NET_LISTEN_INFO = NET_LISTEN("mem-early", {})
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
    run_script_with(registry, &root, script).expect("early client runs");
    assert_eq!(read_trimmed(&root.join("out.txt").unwrap()), "early-bird");
}

#[test]
fn memory_queue_full_bails_through_connect() {
    // A missing consumer must fail loudly, not leak: with 64 sessions
    // queued and none accepted, the next CONNECT bails before pumping
    // (so the task cannot hang). No sockets involved.
    use oxdock_net_plugin::MemoryPipePair;
    let registry = Arc::new(EndpointRegistry::new(false));
    let endpoint = EndpointKey::tcp(oxdock_net_plugin::VirtualEndpoint::Name(
        "mem-full".to_string(),
    ));
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
        LET $l: NET_LISTEN_INFO = NET_LISTEN("test-offline-echo", {})
        WRITE addr.txt "{{ $l.addr }}"
        WRITE virt.txt "{{ $l.virtual }}"
        NET_CLOSE($l.listener)
    "#};
    run_script_with(registry, &root, script).expect("offline listen runs");
    assert_eq!(
        read_trimmed(&root.join("addr.txt").unwrap()),
        "test-offline-echo"
    );
    assert_eq!(
        read_trimmed(&root.join("virt.txt").unwrap()),
        "test-offline-echo"
    );
}

#[test]
fn offline_connect_bails_before_sockets() {
    // The sandbox gate fires before parsing, DNS, or dial: even a
    // loopback literal and a virtual port both bail. Runs under Miri.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for target in ["test-offline-dial-a", "test-offline-dial-b"] {
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

#[test]
fn fetch_cleartext_to_real_host_refuses_without_dialing() {
    // The scheme gate fires before DNS or dial: no sockets open, so this
    // runs under Miri.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $body: STRING = NET_FETCH("http://example.com/doc.json")
    "#};
    let err = run_script(&root, script).expect_err("cleartext must fail");
    assert!(err.to_string().contains("cleartext"), "{err:#}");
}

#[test]
fn fetch_in_placeholder_stays_unknown() {
    // Stateful entries never reach the pure table behind `{{ }}`: the
    // placeholder fails listing the known pure names, and `NET_FETCH`
    // is not among them. No sockets open, so this runs under Miri.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        WRITE tmpl.txt "body \{{ NET::NET_FETCH(\"http://127.0.0.1:9/x\") }}"
        LET $buf: PIPE
        WITH_IO [stdout=$buf] EXPAND tmpl.txt
    "#};
    let err = run_script(&root, script).expect_err("placeholder fetch must fail");
    assert!(
        err.to_string().contains("unknown placeholder function"),
        "{err:#}"
    );
    let known = err
        .to_string()
        .split("known functions:")
        .nth(1)
        .unwrap_or_default()
        .to_string();
    assert!(
        !known.contains("NET_FETCH"),
        "pure listing must not offer NET_FETCH: {err:#}"
    );
}

/// Serve one canned HTTP/1.1 response on loopback, then exit. Returns
/// the bound address for `NET_FETCH` URLs.
fn serve_http_once(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        while seen.len() < 65536 {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    seen.push(byte[0]);
                    if seen.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).expect("head");
        stream.write_all(&body).expect("body");
    });
    addr.to_string()
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn fetch_feeds_parse_json() {
    // The stream shape end to end: bytes off the wire flow into the
    // pure parser with no bespoke conversion. Ephemeral bind, so no
    // fixed-port coordination.
    let addr = serve_http_once(br#"{"name": "loopback"}"#.to_vec());
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = format!(
        indoc! {r#"
            IMPORT [STD, NET]
            LET $body: STRING = NET_FETCH("http://{addr}/doc.json", {{}})
            LET $doc: MAP<ANY> = PARSE_JSON($body)
            ASSERT_EQ $doc.name "loopback"
        "#},
        addr = addr
    );
    run_script(&root, &script).expect("fetch into parse runs");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP plus threads")]
fn fetch_sends_custom_headers() {
    // Headers from the options MAP reach the wire: the fixture
    // captures the request head and the script asserts on it through
    // a second fetch of the captured bytes. Ephemeral bind, so no
    // fixed-port coordination.
    use std::sync::{Arc, Mutex};
    let seen: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured = Arc::clone(&seen);
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while head.len() < 65536 {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        *captured.lock().expect("lock") = head;
        let body = b"ok";
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).expect("head");
        stream.write_all(body).expect("body");
    });
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = format!(
        indoc! {r#"
            IMPORT [STD, NET]
            LET $body: STRING = NET_FETCH("http://{addr}/doc.json", {{headers: {{x-test-name: "fetch-headers"}}}})
            ASSERT_EQ $body "ok"
        "#},
        addr = addr
    );
    run_script(&root, &script).expect("fetch with headers runs");
    let head = String::from_utf8_lossy(&seen.lock().expect("lock")).to_string();
    assert!(
        head.to_lowercase().contains("x-test-name: fetch-headers"),
        "custom header must reach the wire: {head}"
    );
}

#[test]
fn fetch_arity_names_its_optional_range() {
    // `$options` is omittable: zero and three args fail naming the
    // 1-to-2 range, all before any socket opens, so this runs under
    // Miri.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let err = run_script(&root, "IMPORT [STD, NET]\nLET $b: STRING = NET_FETCH()\n")
        .expect_err("zero args must fail");
    assert!(
        err.to_string()
            .contains("expects 1 to 2 argument(s), got 0"),
        "{err:#}"
    );
    // Three args against a 1-to-2 range: the fillers never
    // evaluate (the arity gate fires first), so their values are
    // irrelevant, only their count.
    let err = run_script(
        &root,
        "IMPORT [STD, NET]\nLET $b: STRING = NET_FETCH(\"http://example.com/x\", {}, {})\n",
    )
    .expect_err("three args must fail");
    assert!(
        err.to_string()
            .contains("expects 1 to 2 argument(s), got 3"),
        "{err:#}"
    );
}

#[test]
fn fetch_rejects_unknown_options_without_dialing() {
    // The key check fires before any socket opens: example.com never
    // dials, so this runs under Miri.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $body: STRING = NET_FETCH("http://example.com/doc.json", {bogus: 1})
    "#};
    let err = run_script(&root, script).expect_err("unknown option must fail");
    assert!(err.to_string().contains("unknown option"), "{err:#}");
}

#[test]
fn fetch_rejects_non_string_header_values_without_dialing() {
    // Value shape is checked before any socket opens, so this runs
    // under Miri.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $body: STRING = NET_FETCH("http://example.com/doc.json", {headers: {x-n: 1}})
    "#};
    let err = run_script(&root, script).expect_err("non-string header must fail");
    assert!(
        err.to_string().contains("expected STRING, got INT"),
        "{err:#}"
    );
}

#[test]
fn fetch_rejects_int_timeout_without_dialing() {
    // The declared `DURATION|STRING` union fails an INT before any
    // socket opens, so this runs under Miri.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $body: STRING = NET_FETCH("http://example.com/doc.json", {timeout: 5})
    "#};
    let err = run_script(&root, script).expect_err("int timeout must fail");
    assert!(
        err.to_string()
            .contains("expected DURATION|STRING, got INT"),
        "{err:#}"
    );
}

#[test]
fn offline_connect_reads_both_timeout_shapes() {
    // Both `DURATION|STRING` members read before the sandbox gate:
    // neither dials, so this runs under Miri.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    for (label, options) in [
        ("string", "{timeout: \"10s\"}"),
        ("duration", "{timeout: $d}"),
    ] {
        let registry = Arc::new(EndpointRegistry::new(true));
        let script = format!(
            indoc! {r#"
                IMPORT [STD, NET]
                LET $d: DURATION = "5s"
                LET $in: PIPE
                LET $out: PIPE
                LET $c: HANDLE = ASYNC {{
                    NET_CONNECT("test-offline-timeout", $in, $out, {options})
                }}
                AWAIT $c
            "#},
            options = options,
        );
        let err = run_script_with(registry, &root, &script).expect_err("offline dial must fail");
        assert!(
            err.to_string().contains("--offline mode"),
            "{label} timeout reads past validation: {err:#}"
        );
    }
}

#[test]
#[cfg_attr(
    miri,
    ignore = "blind sampling drives registry claim paths outside Miri isolation"
)]
fn returns_gate_covers_net_functions() {
    // Same returns contract as STD builtins, over this module's own
    // registrations: sampled calls that reach `Ok` satisfy the slot,
    // and anything sampling cannot reach must carry examples. Examples
    // name `doc-net-*` services, so the harness pre-maps them the way a
    // CLI runner would: scripts declare, the runner binds.
    let registry = Arc::new(EndpointRegistry::new(false));
    for service in [
        "doc-net-listen",
        "doc-net-demo",
        "doc-net-connect",
        "doc-net-observe",
        "doc-net-dial",
        "doc-net-close",
        "doc-net-fetch",
    ] {
        registry
            .add_cli_mapping(service, "127.0.0.1:0")
            .expect("map doc service");
    }
    registry.bind_all().expect("bind doc services");
    let module = module_with_endpoints(registry);
    let proc = oxdock_process::default_process_manager();
    oxdock_core::assert_module_returns_conformance(
        "NET",
        &module.funcs,
        vec![module.clone()],
        proc,
    );
}

#[test]
fn fetch_rejects_bad_timeout_without_dialing() {
    // Duration parsing precedes the dial, so this runs under Miri.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $body: STRING = NET_FETCH("http://example.com/doc.json", {timeout: "soon"})
    "#};
    let err = run_script(&root, script).expect_err("bad timeout must fail");
    assert!(err.to_string().contains("must be a duration"), "{err:#}");
}

#[test]
fn fetch_rejects_negative_counts_without_dialing() {
    // Count validation precedes the dial, so this runs under Miri.
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $body: STRING = NET_FETCH("http://example.com/doc.json", {max_redirects: -1})
    "#};
    let err = run_script(&root, script).expect_err("negative count must fail");
    assert!(err.to_string().contains("non-negative"), "{err:#}");
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP")]
fn port_and_addr_observe_prebound_mapping() {
    // The -p path: a pre-bound ephemeral outer socket is observable
    // in-script without claiming the slot, then routable into RUN via
    // normal expansion. No fixed ports: the outer bind is ephemeral.
    use oxdock_net_plugin::VirtualEndpoint;
    let registry = Arc::new(EndpointRegistry::new(false));
    registry
        .add_mapping(
            &EndpointKey::tcp(VirtualEndpoint::Name("resolve-demo".to_string())),
            BindingSpec::Exposed {
                addr: "127.0.0.1:0".parse().expect("addr"),
            },
        )
        .expect("mapping");
    registry.bind_all().expect("bind_all resolves ephemeral");
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $port: INT = NET_PORT("resolve-demo")
        WRITE port.txt "{{ $port }}"
        LET $addr: STRING = NET_ADDR("resolve-demo")
        WRITE addr.txt "{{ $addr }}"
    "#};
    run_script_with(registry, &root, script).expect("resolve runs");
    let port: u16 = read_trimmed(&root.join("port.txt").unwrap())
        .parse()
        .expect("resolved port parses");
    assert_ne!(port, 0, "ephemeral outer port must resolve");
    let addr = read_trimmed(&root.join("addr.txt").unwrap());
    assert!(
        addr.ends_with(&format!(":{port}")),
        "dial string must carry the resolved port, got {addr:?}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "needs loopback TCP")]
fn port_and_addr_fail_loudly_when_unbound() {
    // No sentinel returns: unmapped names, memory-only slots, and
    // ambiguous bare names all bail with explicit context.
    use oxdock_net_plugin::VirtualEndpoint;
    let temp = GuardedPath::tempdir().unwrap();
    let root = guard_root(&temp);
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $port: INT = NET_PORT("never-mapped-xyz")
    "#};
    let err = run_script(&root, script).expect_err("unmapped lookup must fail");
    assert!(err.to_string().contains("is not mapped"), "{err:#}");

    let registry = Arc::new(EndpointRegistry::new(false));
    registry
        .add_mapping(
            &EndpointKey::tcp(VirtualEndpoint::Name("both-proto".to_string())),
            BindingSpec::Loopback { port: 0 },
        )
        .expect("mapping");
    registry
        .add_mapping(
            &EndpointKey::udp(VirtualEndpoint::Name("both-proto".to_string())),
            BindingSpec::Loopback { port: 0 },
        )
        .expect("mapping");
    registry.bind_all().expect("bind_all");
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $port: INT = NET_PORT("both-proto")
    "#};
    let err = run_script_with(registry.clone(), &root, script).expect_err("ambiguous must fail");
    assert!(err.to_string().contains("ambiguous"), "{err:#}");
    let script = indoc! {r#"
        IMPORT [STD, NET]
        LET $tcp: INT = NET_PORT("tcp/both-proto")
        LET $udp: STRING = NET_ADDR("udp/both-proto")
        WRITE tcp.txt "{{ $tcp }}"
        WRITE udp.txt "{{ $udp }}"
    "#};
    run_script_with(registry, &root, script).expect("qualified lookups resolve");
    let tcp: u16 = read_trimmed(&root.join("tcp.txt").unwrap())
        .parse()
        .expect("tcp port parses");
    assert_ne!(tcp, 0);
    let udp = read_trimmed(&root.join("udp.txt").unwrap());
    let udp_port: u16 = udp
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .parse()
        .expect("udp dial string carries a port");
    assert_ne!(
        udp_port, 0,
        "udp dial string must carry a port, got {udp:?}"
    );
}
