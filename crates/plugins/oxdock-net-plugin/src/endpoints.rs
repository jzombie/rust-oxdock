//! Virtual endpoint registry: logical service keys to physical bindings.
//!
//! Isolation mandate: scripts declare service names, the runner binds
//! ports. Nothing in a script ever produces a physical bind (except
//! what we directly cannot control via `RUN`): `SERVE`/`LISTEN` on a
//! CLI-mapped key [`claim`] a pre-bound socket (a `try_clone` sharing
//! the kernel backlog). Memory rendezvous needs an explicit runner
//! mapping too; unmapped keys are a hard error raised by the pre-run
//! check before the first step executes.
//!
//! Dials resolve the same way through one shared resolver. Bare numbers
//! stay parseable as port keys so CLI-mapped slots (`-p 2222:2251`,
//! `--listen 2251`) remain claimable, but an unmapped number never binds
//! or dials from a script.
//!
//! Memory slots hold bounded queues of pipe pairs for in-process IPC with
//! zero sockets; the global offline flag turns every acquisition socketless
//! and gates all dial-out (see the `--offline` sandbox invariant).
//!
//! [`claim`]: EndpointRegistry::claim

use std::collections::{BTreeMap, VecDeque};
use std::net::{SocketAddr, TcpListener};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, bail};
use oxdock_core::PreRunCheck;
use oxdock_parser::{Expr, Step, StepKind};
use oxdock_pipe::ScriptPipe;

use crate::validate::{EndpointKey, VirtualEndpoint};

/// Cap on queued memory sessions per service: a client loop with no
/// `ACCEPT` consumer must fail loudly instead of leaking pipe backends
/// without bound. Mirrors the SSH `CHANNEL_CAPACITY = 64` precedent.
pub const MEMORY_QUEUE_CAP: usize = 64;

/// Bind a loopback listener with `SO_REUSEADDR` so close-and-rebind
/// loops survive a still-closing previous socket: rebind succeeds while
/// the old FD drains instead of flaking `EADDRINUSE`. No `SO_REUSEPORT`,
/// so a live listener still blocks a second claim (the double-claim
/// refusal test pins that).
fn bind_reusable(addr: SocketAddr) -> Result<TcpListener> {
    let domain = match addr {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))
        .with_context(|| format!("NET socket for {addr} failed"))?;
    socket
        .set_reuse_address(true)
        .with_context(|| format!("NET reuse flag for {addr} failed"))?;
    socket
        .bind(&addr.into())
        .with_context(|| format!("NET bind {addr} failed"))?;
    socket
        .listen(128)
        .with_context(|| format!("NET listen {addr} failed"))?;
    Ok(socket.into())
}

/// Physical binding for one virtual endpoint, decided by the host runner.
#[derive(Debug, Clone)]
pub enum BindingSpec {
    /// Loopback bind on the given port (`127.0.0.1:port`).
    Loopback { port: u16 },
    /// Bind exactly this address. The only place a wildcard or a
    /// non-loopback interface may appear: the CLI `--listen`/`-p` flags.
    Exposed { addr: SocketAddr },
    /// Dial-only target owned outside the registry (a harness-held test
    /// listener, a proxied upstream): resolvable for dials, never bound
    /// by `bind_all`, never claimable for LISTEN.
    DialOnly { addr: SocketAddr },
    /// No socket. Serves return live handles; accepts wait for close.
    Offline,
    /// In-process IPC: no socket, sessions queue as pipe pairs.
    Memory,
}

/// One memory session: two in-memory pipe backends, one per direction.
/// `client_to_server` carries CONNECT-to-ACCEPT bytes, `server_to_client`
/// the reverse. Single-use: each pair pumps exactly one session.
#[derive(Clone)]
pub struct MemoryPipePair {
    pub client_to_server: Arc<oxdock_pipe::PipeInner>,
    pub server_to_client: Arc<oxdock_pipe::PipeInner>,
}

impl MemoryPipePair {
    /// Mint a fresh pair. No I/O, no sockets, no task affinity: safe to
    /// share across the CONNECT and ACCEPT tasks.
    pub fn fresh() -> Self {
        Self {
            client_to_server: ScriptPipe::new().pipe_inner(),
            server_to_client: ScriptPipe::new().pipe_inner(),
        }
    }
}

/// What a `SERVE`/`LISTEN` call holds after acquiring its endpoint.
#[derive(Debug, Clone)]
pub enum AcquiredListener {
    /// Pre-bound (CLI) or inline fallback socket, plus its real address
    /// for the result MAP.
    Tcp {
        listener: Arc<TcpListener>,
        addr: SocketAddr,
    },
    /// Memory slot: sessions arrive via [`EndpointRegistry::dequeue_memory_session`].
    ///
    /// [`EndpointRegistry::dequeue_memory_session`]: EndpointRegistry::dequeue_memory_session
    Memory,
    /// Offline: no socket, no sessions; accepts wait for close/cancel.
    Offline,
}

/// What a CONNECT target resolves to (see [`EndpointRegistry::slot_kind`]).
///
/// [`EndpointRegistry::slot_kind`]: EndpointRegistry::slot_kind
#[derive(Debug, Clone)]
pub enum SlotKind {
    /// No slot mapped: serve and dial bail everywhere (memory needs an
    /// explicit runner mapping). Never a bind.
    Unmapped,
    /// Bound physical address: dial it.
    TcpBound(SocketAddr),
    /// TCP spec with no socket (runner skipped `bind_all`): bail.
    TcpUnbound,
    /// Memory rendezvous: queue/consume a pipe pair.
    Memory,
    /// Offline service: bail.
    Offline,
}

/// Whether a serve/dial path tolerates socketless operation. NET paths
/// pass `SocketlessOk` (explicitly-mapped memory services rendezvous
/// in-process); SSH paths pass `SocketRequired` (SSH needs a TCP
/// socket, so anything socketless bails with the CLI mapping hint).
/// Unmapped keys bail under both policies. One enum, one match, both
/// plugins: resolution never forks per plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketRequirement {
    SocketlessOk,
    SocketRequired,
}

/// What a service key resolves to for dialing: a bound physical
/// address, or an in-process memory rendezvous. Returned by the single
/// shared resolver both plugins call.
#[derive(Debug, Clone, Copy)]
pub enum ServiceTarget {
    Tcp(SocketAddr),
    Memory,
}

/// The one CLI-mapping bail text. Both plugins, serve and dial: the
/// wording lives in this function and nowhere else. Exported so the SSH
/// defensive arms report it without restating it.
pub fn no_cli_mapping(func: &str, endpoint: &VirtualEndpoint) -> anyhow::Error {
    anyhow::anyhow!(
        "{func} service '{endpoint}' has no CLI port mapping; pass -p <host_port>:{endpoint} at invocation"
    )
}

struct ListenerSlot {
    spec: BindingSpec,
    socket: Option<Arc<TcpListener>>,
    /// Bound by `bind_all` (host-owned, survives release) vs memory
    /// (dropped on release so the slot frees).
    persistent: bool,
    claimed: AtomicBool,
    memory: VecDeque<MemoryPipePair>,
}

impl std::fmt::Debug for ListenerSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ListenerSlot")
            .field("spec", &self.spec)
            .field("has_socket", &self.socket.is_some())
            .field("claimed", &self.claimed.load(Ordering::SeqCst))
            .field("queued", &self.memory.len())
            .finish()
    }
}

/// Logical-to-physical endpoint map for one run. Clone shares the map
/// (closures capture an `Arc`); slots mutate under one mutex.
#[derive(Debug, Default)]
pub struct EndpointRegistry {
    offline: bool,
    slots: Mutex<BTreeMap<EndpointKey, ListenerSlot>>,
}

impl EndpointRegistry {
    /// Empty registry. `offline` true makes every acquisition socketless
    /// and gates all dial-out: the `--offline` sandbox.
    pub fn new(offline: bool) -> Self {
        Self {
            offline,
            slots: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether this registry runs socketless (`--offline`).
    pub fn is_offline(&self) -> bool {
        self.offline
    }

    /// Runner-side mapping for one service name to a physical bind (`-p`
    /// semantics without flag parsing): the single constructor test
    /// harnesses use to pre-map services to ephemeral loopback before
    /// running scripts. The runner binds, the script declares.
    pub fn add_cli_mapping(&self, service: &str, addr: &str) -> Result<()> {
        let addr: SocketAddr = addr.parse().with_context(|| {
            format!("CLI mapping for '{service}' needs a host:port address, got {addr:?}")
        })?;
        self.add_mapping(
            &EndpointKey::tcp(VirtualEndpoint::Name(service.to_string())),
            BindingSpec::Exposed { addr },
        )
    }

    /// Stage a pre-run service check for one module: every service key
    /// the steps name must be fully bound (runner-mapped socket or
    /// explicitly-mapped memory) before the first step executes.
    /// `module` selects the function set (`"NET"` or `"SSH"`). The runner
    /// discovers the check by type, so this travels in module states next
    /// to the registry itself.
    pub fn pre_run_check(self: &Arc<Self>, module: &'static str) -> PreRunCheck {
        let registry = Arc::clone(self);
        PreRunCheck::new(move |steps| assert_services_bound(&registry, module, steps))
    }

    fn lock_slots<'a>(&'a self) -> std::sync::MutexGuard<'a, BTreeMap<EndpointKey, ListenerSlot>> {
        self.slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Map one virtual endpoint to its physical spec. Duplicate keys bail:
    /// the CLI rejects them at flag parse time, so a dup here is an
    /// internal wiring bug.
    pub fn add_mapping(&self, key: &EndpointKey, spec: BindingSpec) -> Result<()> {
        let mut slots = self.lock_slots();
        if slots.contains_key(key) {
            bail!("duplicate endpoint mapping for '{key}'");
        }
        slots.insert(
            key.clone(),
            ListenerSlot {
                spec,
                socket: None,
                persistent: false,
                claimed: AtomicBool::new(false),
                memory: VecDeque::new(),
            },
        );
        Ok(())
    }

    /// Whether `key` has a mapped slot.
    pub fn has(&self, key: &EndpointKey) -> bool {
        self.lock_slots().contains_key(key)
    }

    /// Open every mapped TCP socket. The CLI calls this before parsing so
    /// `EADDRINUSE` fails fast, never parse-then-fail-on-bind. Offline and
    /// memory slots need no socket. Idempotent: bound slots are skipped.
    pub fn bind_all(&self) -> Result<()> {
        if self.offline {
            return Ok(());
        }
        let mut slots = self.lock_slots();
        for (key, slot) in slots.iter_mut() {
            if slot.socket.is_some() {
                continue;
            }
            let addr = match &slot.spec {
                BindingSpec::Loopback { port } => SocketAddr::from(([127, 0, 0, 1], *port)),
                BindingSpec::Exposed { addr } => *addr,
                BindingSpec::DialOnly { .. } | BindingSpec::Offline | BindingSpec::Memory => {
                    continue;
                }
            };
            let listener = bind_reusable(addr)
                .with_context(|| format!("bind {addr} for virtual '{key}' failed"))?;
            slot.socket = Some(Arc::new(listener));
            slot.persistent = true;
        }
        Ok(())
    }

    /// Real address of a bound slot, if any. Reports ephemeral resolutions
    /// (CLI `-p 0:<endpoint>`) back to the runner.
    pub fn bound_addr(&self, key: &EndpointKey) -> Option<SocketAddr> {
        let slots = self.lock_slots();
        let slot = slots.get(key)?;
        let socket = slot.socket.as_ref()?;
        socket.local_addr().ok()
    }

    /// Bound address of a slot, or a `not bound` error naming `key`.
    /// Unmapped slots, never-bound TCP specs, memory rendezvous, and
    /// offline slots all report the same error: no `0`/empty sentinels.
    pub fn resolve_endpoint(&self, key: &EndpointKey) -> Result<SocketAddr> {
        self.bound_addr(key)
            .ok_or_else(|| anyhow::anyhow!("virtual endpoint '{key}' is not bound"))
    }

    /// Resolve caller-facing endpoint text (`NET_PORT`/`NET_ADDR`): an
    /// explicit qualifier (`tcp/web`, `udp/dns`, `-p`-style `dns/udp`)
    /// resolves strictly, while bare text defaults to TCP with
    /// single-protocol fallback (bare `dns` finds lone `udp/dns`). Bare
    /// text bound under both protocols fails fast as ambiguous. Anything
    /// without a bound socket bails: memory and offline services report
    /// the virtual-channel message, unmapped services the mapping hint.
    /// `noun` is `"port"` (`NET_PORT`) or `"address"` (`NET_ADDR`).
    pub fn resolve_ref(&self, raw: &str, func: &str, noun: &str) -> Result<SocketAddr> {
        let text = raw.trim();
        let (protocol, endpoint) = crate::validate::parse_endpoint_ref(text, func)?;
        let resolve_one = |key: &EndpointKey, kind: &SlotKind| -> Result<SocketAddr> {
            match kind {
                SlotKind::TcpBound(addr) => Ok(*addr),
                SlotKind::Memory | SlotKind::Offline => bail!(
                    "{func} service '{}' is running as an in-memory virtual channel with no physical {noun}",
                    key.endpoint
                ),
                SlotKind::Unmapped => bail!(
                    "{func} service '{}' is not mapped by the runner (map it with -p/--listen)",
                    key.endpoint
                ),
                SlotKind::TcpUnbound => {
                    bail!("{func}: '{key}' was never bound (the runner must call bind_all)")
                }
            }
        };
        match protocol {
            Some(protocol) => {
                let key = EndpointKey { protocol, endpoint };
                let kind = self.slot_kind(&key);
                resolve_one(&key, &kind)
            }
            None => {
                let tcp = EndpointKey::tcp(endpoint.clone());
                let udp = EndpointKey::udp(endpoint);
                let tcp_kind = self.slot_kind(&tcp);
                let udp_kind = self.slot_kind(&udp);
                match (&tcp_kind, &udp_kind) {
                    (SlotKind::TcpBound(_), SlotKind::TcpBound(_)) => bail!(
                        "ambiguous virtual endpoint '{text}': both TCP and UDP bindings exist; qualify as 'tcp/{text}' or 'udp/{text}'"
                    ),
                    // A known slot's verdict beats an absent one; TCP
                    // first preserves the bare-text default.
                    _ => {
                        if matches!(tcp_kind, SlotKind::Unmapped)
                            && !matches!(udp_kind, SlotKind::Unmapped)
                        {
                            resolve_one(&udp, &udp_kind)
                        } else {
                            resolve_one(&tcp, &tcp_kind)
                        }
                    }
                }
            }
        }
    }

    /// Claim a mapped slot: the second claim while busy bails
    /// (`already claimed`), so concurrent listeners on one service fail
    /// loudly; claim-after-release succeeds for re-bind loops. Offline
    /// registries always acquire socketless.
    pub fn claim(&self, key: &EndpointKey, func: &str) -> Result<AcquiredListener> {
        if self.offline {
            return Ok(AcquiredListener::Offline);
        }
        let mut slots = self.lock_slots();
        let Some(slot) = slots.get_mut(key) else {
            bail!("{func}: unknown service '{key}' (map it with -p/--listen)");
        };
        if slot.claimed.swap(true, Ordering::SeqCst) {
            bail!("{func}: '{key}' is already claimed");
        }
        if let Some(socket) = slot.socket.as_ref() {
            let addr = socket
                .local_addr()
                .with_context(|| format!("{func} cannot read its bound address"))?;
            return Ok(AcquiredListener::Tcp {
                listener: Arc::clone(socket),
                addr,
            });
        }
        match slot.spec {
            BindingSpec::Memory => Ok(AcquiredListener::Memory),
            BindingSpec::Offline => Ok(AcquiredListener::Offline),
            BindingSpec::DialOnly { .. } => {
                slot.claimed.store(false, Ordering::SeqCst);
                bail!(
                    "{func}: '{key}' is a dial-only mapping (the runner owns the socket; LISTEN elsewhere)"
                )
            }
            BindingSpec::Loopback { .. } | BindingSpec::Exposed { .. } => {
                slot.claimed.store(false, Ordering::SeqCst);
                bail!("{func}: '{key}' was never bound (the runner must call bind_all)")
            }
        }
    }

    /// Free a slot for the next claimant. Missing slots are ignored
    /// (socketless offline acquisitions never register); closing twice
    /// stays silent. Memory slots vanish entirely, with their sockets and
    /// any queued memory pairs; host-bound (persistent) slots keep their
    /// socket and just unclaim for re-claim.
    pub fn release(&self, key: &EndpointKey) {
        let mut slots = self.lock_slots();
        let inline = slots.get(key).is_some_and(|slot| !slot.persistent);
        if inline {
            slots.remove(key);
        } else if let Some(slot) = slots.get_mut(key) {
            slot.claimed.store(false, Ordering::SeqCst);
        }
    }

    /// Ensure a memory rendezvous slot exists for `key`, creating an
    /// unclaimed one for unmapped names (client-before-server order).
    /// Present slots pass through untouched: the caller decides (dial the
    /// bound address, or bail offline).
    pub fn ensure_memory_slot(&self, key: &EndpointKey) {
        let mut slots = self.lock_slots();
        slots.entry(key.clone()).or_insert_with(|| ListenerSlot {
            spec: BindingSpec::Memory,
            socket: None,
            persistent: false,
            claimed: AtomicBool::new(false),
            memory: VecDeque::new(),
        });
    }

    /// What a CONNECT target resolves to: a bound physical address, a
    /// memory rendezvous, an offline dead-end, or nothing mapped (plain
    /// loopback dial for ports, auto-memory for names).
    pub fn slot_kind(&self, key: &EndpointKey) -> SlotKind {
        let slots = self.lock_slots();
        let Some(slot) = slots.get(key) else {
            return SlotKind::Unmapped;
        };
        match &slot.spec {
            BindingSpec::Memory => SlotKind::Memory,
            BindingSpec::Offline => SlotKind::Offline,
            BindingSpec::DialOnly { addr } => SlotKind::TcpBound(*addr),
            BindingSpec::Loopback { .. } | BindingSpec::Exposed { .. } => {
                match slot
                    .socket
                    .as_ref()
                    .and_then(|socket| socket.local_addr().ok())
                {
                    Some(addr) => SlotKind::TcpBound(addr),
                    None => SlotKind::TcpUnbound,
                }
            }
        }
    }

    /// Queue a CONNECT-side memory pair for a later ACCEPT. Bails past
    /// [`MEMORY_QUEUE_CAP`] instead of leaking backends when no consumer
    /// accepts.
    pub fn enqueue_memory_session(&self, key: &EndpointKey, pair: MemoryPipePair) -> Result<()> {
        let mut slots = self.lock_slots();
        let Some(slot) = slots.get_mut(key) else {
            bail!("NET_CONNECT: unknown service '{key}' (map it with -p/--listen)");
        };
        if slot.memory.len() >= MEMORY_QUEUE_CAP {
            bail!("NET_CONNECT: memory connection queue for '{key}' is full");
        }
        slot.memory.push_back(pair);
        Ok(())
    }

    /// Pop the oldest queued memory pair, if any. ACCEPT polls this per
    /// tick alongside cancel/shutdown, mirroring the TCP accept loop.
    pub fn dequeue_memory_session(&self, key: &EndpointKey) -> Option<MemoryPipePair> {
        self.lock_slots()
            .get_mut(key)
            .and_then(|slot| slot.memory.pop_front())
    }
}

/// Fail a run before its first step when a named service is not fully
/// bound: the runner never mapped it, or mapped it without binding.
/// Offline registries pass trivially (socketless by runner decree).
/// Enforcement behind the isolation mandate; call-time resolution
/// repeats the same verdicts for defense in depth.
pub fn assert_services_bound(
    registry: &EndpointRegistry,
    module: &str,
    steps: &[Step],
) -> Result<()> {
    if registry.is_offline() {
        return Ok(());
    }
    let mut calls = Vec::new();
    for step in steps {
        inspect_kind(module, &step.kind, &mut calls);
    }
    for (func, service) in calls {
        let Some(service) = service else {
            bail!(
                "{func} service names must be string literals so the runner can verify bindings before execution"
            );
        };
        check_service_bound(registry, func, &service)?;
    }
    Ok(())
}

/// Inspect one step kind and everything under it for service calls.
/// Statement-position calls (`StepKind::Call`) name services exactly
/// like expression calls, but the walker only visits argument exprs,
/// never the head: without the head check a bare `NET_LISTEN("x")`
/// step sails through pre-run and fails at call time instead.
fn inspect_kind(module: &str, kind: &StepKind, calls: &mut Vec<(&'static str, Option<String>)>) {
    if let StepKind::Call { name, args } = kind
        && let Some(inspected) = inspect_call(module, name, args)
    {
        calls.push(inspected);
    }
    kind.walk_child_kinds(&mut |child| inspect_kind(module, child, calls));
    // Shallow exprs only: `walk_step_exprs` would re-descend into the
    // children the line above already recurses to, double-counting
    // nested calls. Verdicts are idempotent, but the duplicate work
    // would lie about how many services a script names.
    kind.walk_exprs(&mut |expr| {
        expr.walk(&mut |inner| {
            if let Expr::Call { name, args } = inner
                && let Some(inspected) = inspect_call(module, name, args)
            {
                calls.push(inspected);
            }
        });
    });
}

/// Classify one call by head name, capturing the first argument when
/// it is a service-name literal. Non-literals surface downstream as
/// the string-literal requirement, never as silent passes.
fn inspect_call(module: &str, name: &str, args: &[Expr]) -> Option<(&'static str, Option<String>)> {
    let func = classify_service_call(module, name)?;
    let service = match args.first() {
        Some(Expr::Literal(value)) => value.as_str().map(str::to_string),
        _ => None,
    };
    Some((func, service))
}

/// Map a lowered call name to the canonical function it enforces, for
/// one module's checker. Lowered names are fully qualified
/// (`NET::NET_LISTEN`); bare canonical spellings match too. Anything
/// else is not this module's business. Public because the closed-world
/// enforcement test pins this map against every registered function;
/// the map itself stays single-sourced here.
pub fn classify_service_call(module: &str, name: &str) -> Option<&'static str> {
    let (mod_part, base) = match name.split_once("::") {
        Some((module, base)) => (Some(module), base),
        None => (None, name),
    };
    match (module, mod_part, base) {
        ("NET", _, "NET_LISTEN") | ("NET", Some("NET"), "LISTEN") => Some("NET_LISTEN"),
        ("NET", _, "NET_CONNECT") | ("NET", Some("NET"), "CONNECT") => Some("NET_CONNECT"),
        ("NET", _, "NET_PORT") | ("NET", Some("NET"), "PORT") => Some("NET_PORT"),
        ("NET", _, "NET_ADDR") | ("NET", Some("NET"), "ADDR") => Some("NET_ADDR"),
        ("SSH", _, "SSH_SERVE") | ("SSH", Some("SSH"), "SERVE") => Some("SSH_SERVE"),
        ("SSH", _, "SSH_CONNECT") | ("SSH", Some("SSH"), "CONNECT") => Some("SSH_CONNECT"),
        _ => None,
    }
}

/// Verify one named service is fully bound: a runner-mapped socket, an
/// explicitly-mapped memory slot, or an offline designation. Anything
/// else is a hard error naming the fix. Introspection targets reuse
/// their own verdicts (same text as the call path).
fn check_service_bound(registry: &EndpointRegistry, func: &str, service: &str) -> Result<()> {
    if func == "NET_PORT" || func == "NET_ADDR" {
        let noun = if func == "NET_ADDR" {
            "address"
        } else {
            "port"
        };
        return registry.resolve_ref(service, func, noun).map(|_| ());
    }
    let endpoint = crate::validate::parse_virtual_endpoint(service, func)?;
    let key = EndpointKey::tcp(endpoint);
    let sockets = match func {
        "SSH_SERVE" | "SSH_CONNECT" => SocketRequirement::SocketRequired,
        _ => SocketRequirement::SocketlessOk,
    };
    match registry.slot_kind(&key) {
        SlotKind::TcpBound(_) | SlotKind::Offline => Ok(()),
        SlotKind::Memory => match sockets {
            SocketRequirement::SocketlessOk => Ok(()),
            SocketRequirement::SocketRequired => Err(no_cli_mapping(func, &key.endpoint)),
        },
        SlotKind::TcpUnbound => {
            bail!("{func}: '{key}' was never bound (the runner must call bind_all)")
        }
        SlotKind::Unmapped => Err(no_cli_mapping(func, &key.endpoint)),
    }
}

/// Acquire a listener for `SERVE`/`LISTEN`: claim a runner-mapped slot.
/// Unmapped keys are a hard error everywhere (memory rendezvous needs an
/// explicit runner mapping; see `assert_services_bound`, which fails the
/// run before execution reaches here). Callers needing a socket (SSH)
/// additionally bail on mapped-memory slots with the CLI mapping hint.
/// Offline registries acquire socketless without touching the network.
/// One function, both plugins: serve resolution never forks per plugin,
/// and no script string ever binds a physical port.
pub fn acquire_listener(
    registry: &Arc<EndpointRegistry>,
    key: &EndpointKey,
    func: &str,
    sockets: SocketRequirement,
) -> Result<(AcquiredListener, Arc<EndpointRegistry>)> {
    if registry.is_offline() {
        return Ok((AcquiredListener::Offline, Arc::clone(registry)));
    }
    if !registry.has(key) {
        return Err(no_cli_mapping(func, &key.endpoint));
    }
    let (acquired, registry) = registry
        .claim(key, func)
        .map(|got| (got, Arc::clone(registry)))?;
    match (acquired, sockets) {
        (AcquiredListener::Memory, SocketRequirement::SocketRequired) => {
            Err(no_cli_mapping(func, &key.endpoint))
        }
        (got, _) => Ok((got, registry)),
    }
}

/// Resolve a dial target for `CONNECT`: a runner-mapped socket address,
/// or an explicitly-mapped memory rendezvous when the caller tolerates
/// socketless operation (NET). Unmapped keys are a hard error everywhere
/// (see `assert_services_bound`, which fails the run first); callers
/// needing a socket (SSH) additionally bail on mapped-memory slots.
/// One function, both plugins: dial resolution never forks per plugin,
/// and no script string ever dials a physical address.
pub fn resolve_service_target(
    registry: &Arc<EndpointRegistry>,
    key: &EndpointKey,
    func: &str,
    sockets: SocketRequirement,
) -> Result<ServiceTarget> {
    if registry.is_offline() {
        bail!("{func}: '{key}' is offline")
    }
    match registry.slot_kind(key) {
        SlotKind::TcpBound(addr) => Ok(ServiceTarget::Tcp(addr)),
        SlotKind::Memory => match sockets {
            SocketRequirement::SocketlessOk => Ok(ServiceTarget::Memory),
            SocketRequirement::SocketRequired => Err(no_cli_mapping(func, &key.endpoint)),
        },
        SlotKind::Unmapped => Err(no_cli_mapping(func, &key.endpoint)),
        SlotKind::TcpUnbound => {
            bail!("{func}: '{key}' was never bound (the runner must call bind_all)")
        }
        SlotKind::Offline => bail!("{func}: '{key}' is offline"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp_key(port: u16) -> EndpointKey {
        EndpointKey::tcp(VirtualEndpoint::Port(port))
    }

    fn named_key(name: &str) -> EndpointKey {
        EndpointKey::tcp(VirtualEndpoint::Name(name.to_string()))
    }

    #[test]
    fn duplicate_mapping_bails() {
        let registry = EndpointRegistry::new(false);
        registry
            .add_mapping(&tcp_key(2251), BindingSpec::Memory)
            .expect("first mapping");
        registry
            .add_mapping(&tcp_key(2251), BindingSpec::Memory)
            .expect_err("duplicate mapping must fail");
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn claim_release_reclaim_cycle() {
        // Host-bound (persistent) slots unclaim on release: re-bind loops
        // reclaim the same socket.
        let registry = EndpointRegistry::new(false);
        registry
            .add_mapping(&tcp_key(2251), BindingSpec::Loopback { port: 0 })
            .expect("mapping");
        registry.bind_all().expect("bind_all");
        assert!(registry.claim(&tcp_key(2251), "NET_LISTEN").is_ok());
        let err = format!(
            "{:#}",
            registry.claim(&tcp_key(2251), "NET_LISTEN").unwrap_err()
        );
        assert!(err.contains("already claimed"), "{err}");
        registry.release(&tcp_key(2251));
        assert!(registry.claim(&tcp_key(2251), "NET_LISTEN").is_ok());
    }

    #[test]
    fn release_drops_inline_slots() {
        // Inline (non-persistent) slots vanish on release: the next
        // claimant starts clean instead of inheriting state.
        let registry = EndpointRegistry::new(false);
        registry
            .add_mapping(&tcp_key(2251), BindingSpec::Memory)
            .expect("mapping");
        assert!(registry.claim(&tcp_key(2251), "NET_LISTEN").is_ok());
        registry.release(&tcp_key(2251));
        let err = format!(
            "{:#}",
            registry.claim(&tcp_key(2251), "NET_LISTEN").unwrap_err()
        );
        assert!(err.contains("unknown service"), "{err}");
    }

    #[test]
    fn unknown_service_bails() {
        let registry = EndpointRegistry::new(false);
        let err = format!(
            "{:#}",
            registry.claim(&tcp_key(9999), "NET_LISTEN").unwrap_err()
        );
        assert!(err.contains("unknown service"), "{err}");
    }

    #[test]
    fn offline_registry_acquires_socketless() {
        let registry = EndpointRegistry::new(true);
        let got = registry
            .claim(&tcp_key(2251), "SSH_SERVE")
            .expect("offline claim");
        assert!(matches!(got, AcquiredListener::Offline));
        assert!(registry.is_offline());
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn bind_all_claim_serves_pre_bound_socket() {
        // The CLI maps a fixed virtual port to an ephemeral bind
        // (`-p 0:2261`): bind_all resolves it, claim serves the clone.
        let registry = EndpointRegistry::new(false);
        registry
            .add_mapping(&tcp_key(2261), BindingSpec::Loopback { port: 0 })
            .expect("mapping");
        registry.bind_all().expect("bind_all binds ephemeral");
        let addr = registry.bound_addr(&tcp_key(2261)).expect("bound addr");
        assert_ne!(addr.port(), 0, "ephemeral resolved");
        let got = registry.claim(&tcp_key(2261), "NET_LISTEN").expect("claim");
        let AcquiredListener::Tcp { addr: claimed, .. } = got else {
            panic!("expected a TCP acquisition");
        };
        assert_eq!(claimed, addr);
    }

    #[test]
    fn memory_queue_caps_at_64() {
        let registry = EndpointRegistry::new(false);
        registry
            .add_mapping(&tcp_key(2251), BindingSpec::Memory)
            .expect("mapping");
        for _ in 0..MEMORY_QUEUE_CAP {
            registry
                .enqueue_memory_session(&tcp_key(2251), MemoryPipePair::fresh())
                .expect("enqueue within cap");
        }
        let err = format!(
            "{:#}",
            registry
                .enqueue_memory_session(&tcp_key(2251), MemoryPipePair::fresh())
                .unwrap_err()
        );
        assert!(err.contains("is full"), "{err}");
        // Draining one slot admits exactly one more.
        assert!(registry.dequeue_memory_session(&tcp_key(2251)).is_some());
        registry
            .enqueue_memory_session(&tcp_key(2251), MemoryPipePair::fresh())
            .expect("enqueue after drain");
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn resolve_endpoint_reports_bound_port() {
        let registry = EndpointRegistry::new(false);
        registry
            .add_mapping(&tcp_key(2261), BindingSpec::Loopback { port: 0 })
            .expect("mapping");
        registry.bind_all().expect("bind_all binds ephemeral");
        let addr = registry.resolve_endpoint(&tcp_key(2261)).expect("resolve");
        assert_ne!(addr.port(), 0, "ephemeral resolved");
        let err = format!(
            "{:#}",
            registry
                .resolve_endpoint(&named_key("missing"))
                .unwrap_err()
        );
        assert!(err.contains("is not bound"), "{err}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn resolve_ref_handles_protocols() {
        let registry = EndpointRegistry::new(false);
        // TCP-only name: bare and explicit TCP resolve, UDP is not bound.
        registry
            .add_mapping(
                &EndpointKey::tcp(VirtualEndpoint::Name("web".to_string())),
                BindingSpec::Loopback { port: 0 },
            )
            .expect("mapping");
        // UDP-only name: bare falls back to the single bound protocol.
        registry
            .add_mapping(
                &EndpointKey::udp(VirtualEndpoint::Name("dns".to_string())),
                BindingSpec::Loopback { port: 0 },
            )
            .expect("mapping");
        registry.bind_all().expect("bind_all");
        let web = registry
            .resolve_ref("web", "NET_PORT", "port")
            .expect("bare tcp");
        assert_ne!(web.port(), 0);
        assert_eq!(
            registry
                .resolve_ref("tcp/web", "NET_PORT", "port")
                .expect("explicit tcp"),
            web
        );
        let dns = registry
            .resolve_ref("dns", "NET_PORT", "port")
            .expect("bare udp fallback");
        assert_ne!(dns.port(), 0);
        assert_eq!(
            registry
                .resolve_ref("udp/dns", "NET_PORT", "port")
                .expect("explicit udp"),
            dns
        );
        assert_eq!(
            registry
                .resolve_ref("dns/udp", "NET_PORT", "port")
                .expect("suffix form"),
            dns
        );
        // Wrong protocol on a bound name: not mapped under that key.
        let err = format!(
            "{:#}",
            registry
                .resolve_ref("tcp/dns", "NET_PORT", "port")
                .unwrap_err()
        );
        assert!(err.contains("is not mapped"), "{err}");
        // Unknown names never report a sentinel.
        let err = format!(
            "{:#}",
            registry
                .resolve_ref("nope", "NET_PORT", "port")
                .unwrap_err()
        );
        assert!(err.contains("is not mapped"), "{err}");
        // Memory slots have no socket: the virtual-channel message, with
        // the caller's noun.
        registry
            .add_mapping(&named_key("mem"), BindingSpec::Memory)
            .expect("mapping");
        let err = format!(
            "{:#}",
            registry.resolve_ref("mem", "NET_PORT", "port").unwrap_err()
        );
        assert!(
            err.contains("is running as an in-memory virtual channel with no physical port"),
            "{err}"
        );
        let err = format!(
            "{:#}",
            registry
                .resolve_ref("mem", "NET_ADDR", "address")
                .unwrap_err()
        );
        assert!(
            err.contains("is running as an in-memory virtual channel with no physical address"),
            "{err}"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "needs loopback TCP")]
    fn resolve_ref_rejects_ambiguity() {
        let registry = EndpointRegistry::new(false);
        registry
            .add_mapping(
                &EndpointKey::tcp(VirtualEndpoint::Name("dns".to_string())),
                BindingSpec::Loopback { port: 0 },
            )
            .expect("mapping");
        registry
            .add_mapping(
                &EndpointKey::udp(VirtualEndpoint::Name("dns".to_string())),
                BindingSpec::Loopback { port: 0 },
            )
            .expect("mapping");
        registry.bind_all().expect("bind_all");
        let err = format!(
            "{:#}",
            registry.resolve_ref("dns", "NET_PORT", "port").unwrap_err()
        );
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("tcp/dns"), "{err}");
        assert!(err.contains("udp/dns"), "{err}");
        // Qualified lookups stay independent despite the collision.
        let tcp = registry
            .resolve_ref("tcp/dns", "NET_PORT", "port")
            .expect("tcp");
        let udp = registry
            .resolve_ref("udp/dns", "NET_PORT", "port")
            .expect("udp");
        assert_ne!(tcp.port(), 0);
        assert_ne!(udp.port(), 0);
    }
}
