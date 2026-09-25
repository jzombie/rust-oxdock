//! Virtual endpoint registry: logical service keys to physical bindings.
//!
//! Scripts declare a [`VirtualEndpoint`] (a fixed port or a service name)
//! and never touch physical binds. The host runner (CLI) maps each key to
//! a [`BindingSpec`] before the run; [`EndpointRegistry::bind_all`] opens
//! the sockets up front so `EADDRINUSE` fails fast, before any script
//! parses. `SERVE`/`LISTEN` calls then [`claim`] a pre-bound socket (a
//! `try_clone` sharing the kernel backlog) instead of taking ownership, so
//! close-and-rebind loops keep working; [`release`] on close/drop frees
//! the slot for the next claimant.
//!
//! Memory slots hold bounded queues of pipe pairs for in-process IPC with
//! zero sockets; the global offline flag turns every acquisition socketless
//! and gates all dial-out (see the `--offline` sandbox invariant).
//!
//! [`claim`]: EndpointRegistry::claim
//! [`release`]: EndpointRegistry::release

use std::collections::{BTreeMap, VecDeque};
use std::net::{SocketAddr, TcpListener};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, bail};
use oxdock_pipe::ScriptPipe;

use crate::validate::{EndpointKey, VirtualEndpoint};

/// Cap on queued memory sessions per service: a client loop with no
/// `ACCEPT` consumer must fail loudly instead of leaking pipe backends
/// without bound. Mirrors the SSH `CHANNEL_CAPACITY = 64` precedent.
pub const MEMORY_QUEUE_CAP: usize = 64;

/// Physical binding for one virtual endpoint, decided by the host runner.
#[derive(Debug, Clone)]
pub enum BindingSpec {
    /// Loopback bind on the given port (`127.0.0.1:port`).
    Loopback { port: u16 },
    /// Bind exactly this address. The only place a wildcard or a
    /// non-loopback interface may appear: the CLI `--listen`/`-p` flags.
    Exposed { addr: SocketAddr },
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
    /// No slot mapped: ports dial loopback, names auto-create memory.
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

struct ListenerSlot {
    spec: BindingSpec,
    socket: Option<Arc<TcpListener>>,
    /// Bound by `bind_all` (host-owned, survives release) vs inline by a
    /// plugin (dropped on release so the port frees).
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
                BindingSpec::Offline | BindingSpec::Memory => continue,
            };
            let listener = TcpListener::bind(addr)
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
    /// text bound under both protocols fails fast as ambiguous; anything
    /// unbound fails as not bound, echoing the caller's text.
    pub fn resolve_ref(&self, raw: &str, func: &str) -> Result<SocketAddr> {
        let text = raw.trim();
        let (protocol, endpoint) = crate::validate::parse_endpoint_ref(text, func)?;
        match protocol {
            Some(protocol) => {
                let key = EndpointKey {
                    protocol,
                    endpoint,
                };
                self.bound_addr(&key)
                    .ok_or_else(|| anyhow::anyhow!("virtual endpoint '{text}' is not bound"))
            }
            None => {
                let tcp = EndpointKey::tcp(endpoint.clone());
                let udp = EndpointKey::udp(endpoint);
                match (self.bound_addr(&tcp), self.bound_addr(&udp)) {
                    (Some(_), Some(_)) => bail!(
                        "ambiguous virtual endpoint '{text}': both TCP and UDP bindings exist; qualify as 'tcp/{text}' or 'udp/{text}'"
                    ),
                    (Some(addr), None) | (None, Some(addr)) => Ok(addr),
                    (None, None) => {
                        bail!("virtual endpoint '{text}' is not bound")
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
            BindingSpec::Loopback { .. } | BindingSpec::Exposed { .. } => {
                slot.claimed.store(false, Ordering::SeqCst);
                bail!("{func}: '{key}' was never bound (the runner must call bind_all)")
            }
        }
    }

    /// Register an inline fallback bind (unmapped bare port, bound by the
    /// plugin itself on loopback). The slot starts claimed; a racing twin
    /// that claimed first makes this bail `already claimed` and the fresh
    /// socket drops unlistened.
    pub fn register_inline(
        &self,
        key: &EndpointKey,
        listener: TcpListener,
        func: &str,
    ) -> Result<Arc<TcpListener>> {
        let mut slots = self.lock_slots();
        if slots.contains_key(key) {
            bail!("{func}: '{key}' is already claimed");
        }
        let shared = Arc::new(listener);
        slots.insert(
            key.clone(),
            ListenerSlot {
                spec: BindingSpec::Loopback {
                    port: shared.local_addr().map(|addr| addr.port()).unwrap_or(0),
                },
                socket: Some(Arc::clone(&shared)),
                persistent: false,
                claimed: AtomicBool::new(true),
                memory: VecDeque::new(),
            },
        );
        Ok(shared)
    }

    /// Free a slot for the next claimant. Missing slots are ignored
    /// (socketless offline acquisitions never register); closing twice
    /// stays silent. Inline (non-persistent) slots vanish entirely, with
    /// their sockets and any queued memory pairs, so re-bind loops start
    /// clean and ports free promptly; host-bound (persistent) slots keep
    /// their socket and just unclaim for re-claim.
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
    pub fn enqueue_memory_session(
        &self,
        key: &EndpointKey,
        pair: MemoryPipePair,
    ) -> Result<()> {
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

/// Acquire a listener for `SERVE`/`LISTEN`: claim a mapped slot, bind the
/// loopback fallback inline for unmapped bare ports, or open a memory
/// rendezvous for unmapped service names (in-process IPC needs no host
/// mapping). Unmapped names auto-register so client-before-server order
/// works from either side. Offline registries acquire socketless without
/// touching the network. Shared by the NET and SSH plugins so the
/// fallback shape stays identical.
pub fn acquire_listener(
    registry: &Arc<EndpointRegistry>,
    key: &EndpointKey,
    func: &str,
) -> Result<(AcquiredListener, Arc<EndpointRegistry>)> {
    if registry.is_offline() {
        return Ok((AcquiredListener::Offline, Arc::clone(registry)));
    }
    if registry.has(key) {
        return registry
            .claim(key, func)
            .map(|got| (got, Arc::clone(registry)));
    }
    match &key.endpoint {
        VirtualEndpoint::Port(port) => {
            let addr = SocketAddr::from(([127, 0, 0, 1], *port));
            let listener =
                TcpListener::bind(addr).with_context(|| format!("{func} bind {addr} failed"))?;
            let shared = registry.register_inline(key, listener, func)?;
            let bound = shared
                .local_addr()
                .with_context(|| format!("{func} cannot read its bound address"))?;
            Ok((
                AcquiredListener::Tcp {
                    listener: shared,
                    addr: bound,
                },
                Arc::clone(registry),
            ))
        }
        VirtualEndpoint::Name(_) => {
            registry.ensure_memory_slot(key);
            registry
                .claim(key, func)
                .map(|got| (got, Arc::clone(registry)))
        }
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
            registry
                .claim(&tcp_key(2251), "NET_LISTEN")
                .unwrap_err()
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
            registry
                .claim(&tcp_key(2251), "NET_LISTEN")
                .unwrap_err()
        );
        assert!(err.contains("unknown service"), "{err}");
    }

    #[test]
    fn unknown_service_bails() {
        let registry = EndpointRegistry::new(false);
        let err = format!(
            "{:#}",
            registry
                .claim(&tcp_key(9999), "NET_LISTEN")
                .unwrap_err()
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
        let addr = registry
            .bound_addr(&tcp_key(2261))
            .expect("bound addr");
        assert_ne!(addr.port(), 0, "ephemeral resolved");
        let got = registry
            .claim(&tcp_key(2261), "NET_LISTEN")
            .expect("claim");
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
        assert!(
            registry
                .dequeue_memory_session(&tcp_key(2251))
                .is_some()
        );
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
            registry.resolve_endpoint(&named_key("missing")).unwrap_err()
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
        let web = registry.resolve_ref("web", "NET_PORT").expect("bare tcp");
        assert_ne!(web.port(), 0);
        assert_eq!(
            registry.resolve_ref("tcp/web", "NET_PORT").expect("explicit tcp"),
            web
        );
        let dns = registry.resolve_ref("dns", "NET_PORT").expect("bare udp fallback");
        assert_ne!(dns.port(), 0);
        assert_eq!(
            registry.resolve_ref("udp/dns", "NET_PORT").expect("explicit udp"),
            dns
        );
        assert_eq!(
            registry.resolve_ref("dns/udp", "NET_PORT").expect("suffix form"),
            dns
        );
        let err = format!(
            "{:#}",
            registry.resolve_ref("tcp/dns", "NET_PORT").unwrap_err()
        );
        assert!(err.contains("is not bound"), "{err}");
        let err = format!(
            "{:#}",
            registry.resolve_ref("nope", "NET_PORT").unwrap_err()
        );
        assert!(err.contains("is not bound"), "{err}");
        // Memory slots have no socket: not bound, never a sentinel.
        registry
            .add_mapping(&named_key("mem"), BindingSpec::Memory)
            .expect("mapping");
        let err = format!(
            "{:#}",
            registry.resolve_ref("mem", "NET_PORT").unwrap_err()
        );
        assert!(err.contains("is not bound"), "{err}");
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
            registry.resolve_ref("dns", "NET_PORT").unwrap_err()
        );
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("tcp/dns"), "{err}");
        assert!(err.contains("udp/dns"), "{err}");
        // Qualified lookups stay independent despite the collision.
        let tcp = registry.resolve_ref("tcp/dns", "NET_PORT").expect("tcp");
        let udp = registry.resolve_ref("udp/dns", "NET_PORT").expect("udp");
        assert_ne!(tcp.port(), 0);
        assert_ne!(udp.port(), 0);
    }
}
