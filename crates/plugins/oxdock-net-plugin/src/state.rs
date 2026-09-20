//! Listener lifetime state: shared handle, atomic shutdown, inner Drop.
//!
//! There is deliberately no global socket registry: every `NET_LISTENER`
//! DSL value carries an `Arc<ListenerState>`, so listener lifetime follows
//! handle lifetime. The virtual [`EndpointRegistry`] slot (when the
//! listener was acquired through one) releases on close/drop so re-bind
//! loops can reclaim the service. Teardown lives on the INNER state only
//! (never on the tag): `NET_CLOSE` sets the shutdown flag and drops the
//! socket, and dropping the last handle signals shutdown the same way.
//! Accept loops observe the flag per tick, so a blocked `NET_ACCEPT`
//! returns promptly without relying on cross-thread socket closure.
//!
//! [`EndpointRegistry`]: crate::endpoints::EndpointRegistry

use std::net::{SocketAddr, TcpListener};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crate::endpoints::EndpointRegistry;
use crate::validate::VirtualEndpoint;

/// What one listener pumps: a TCP socket, a memory service, or an offline
/// handle with no socket at all.
#[derive(Debug)]
pub enum ListenerKind {
    /// Physical socket, taken by `NET_CLOSE`. Accept loops work on a
    /// `try_clone` so close never races an in-flight `accept` call:
    /// every loop is non-blocking with a tick, and the shutdown flag
    /// below is the wakeup mechanism.
    Tcp {
        listener: Mutex<Option<TcpListener>>,
    },
    /// In-process service: sessions arrive as pipe pairs queued by
    /// `NET_CONNECT` on the registry slot.
    Memory,
    /// Socketless handle (`--offline`): accepts wait for close/cancel.
    Offline,
}

/// Lifetime state behind one `NET_LISTENER` value.
#[derive(Debug)]
pub struct ListenerState {
    id: String,
    local_addr: SocketAddr,
    /// Display/MAP address text: the physical bind for TCP, the virtual
    /// endpoint echo when no socket exists (memory/offline).
    addr_text: String,
    kind: ListenerKind,
    /// The endpoint this listener was acquired for, plus the registry
    /// holding its slot: `ACCEPT` resolves memory sessions through it,
    /// close/drop frees the slot through it.
    registry: Arc<EndpointRegistry>,
    endpoint: VirtualEndpoint,
    /// Set by `NET_CLOSE` and by last-handle drop; observed per
    /// accept-loop tick.
    shutdown: AtomicBool,
}

impl ListenerState {
    pub fn new_tcp(
        id: String,
        registry: Arc<EndpointRegistry>,
        endpoint: VirtualEndpoint,
        local_addr: SocketAddr,
        listener: TcpListener,
    ) -> Self {
        let addr_text = local_addr.to_string();
        Self {
            id,
            local_addr,
            addr_text,
            kind: ListenerKind::Tcp {
                listener: Mutex::new(Some(listener)),
            },
            registry,
            endpoint,
            shutdown: AtomicBool::new(false),
        }
    }

    pub fn new_memory(
        id: String,
        registry: Arc<EndpointRegistry>,
        endpoint: VirtualEndpoint,
    ) -> Self {
        let addr_text = endpoint.to_string();
        Self {
            id,
            // No socket exists: keep a null address for the typed accessor
            // and echo the virtual endpoint for display and result MAPs.
            local_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            addr_text,
            kind: ListenerKind::Memory,
            registry,
            endpoint,
            shutdown: AtomicBool::new(false),
        }
    }

    pub fn new_offline(
        id: String,
        registry: Arc<EndpointRegistry>,
        endpoint: VirtualEndpoint,
    ) -> Self {
        let addr_text = endpoint.to_string();
        Self {
            id,
            local_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            addr_text,
            kind: ListenerKind::Offline,
            registry,
            endpoint,
            shutdown: AtomicBool::new(false),
        }
    }

    /// Registry holding this listener's slot (memory dequeue, release).
    pub fn registry(&self) -> &Arc<EndpointRegistry> {
        &self.registry
    }

    /// Virtual endpoint this listener was acquired for.
    pub fn endpoint(&self) -> &VirtualEndpoint {
        &self.endpoint
    }

    /// True for socketless memory/offline listeners.
    pub fn is_memory(&self) -> bool {
        matches!(self.kind, ListenerKind::Memory)
    }

    /// True for socketless offline listeners.
    pub fn is_offline(&self) -> bool {
        matches!(self.kind, ListenerKind::Offline)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Address text for display and result MAPs: the physical bind for
    /// TCP, the virtual endpoint echo for memory/offline.
    pub fn addr_text(&self) -> &str {
        &self.addr_text
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Accept-loop socket: a clone sharing the same kernel backlog, so
    /// `NET_CLOSE` can drop the original without racing the loop. `None`
    /// once closed, and always `None` for memory/offline listeners.
    pub fn try_clone_listener(&self) -> Option<TcpListener> {
        let ListenerKind::Tcp { listener } = &self.kind else {
            return None;
        };
        listener
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .and_then(|listener| listener.try_clone().ok())
    }

    /// Best-effort shutdown: flag waiters, drop the socket, and free the
    /// registry slot. Shared by `NET_CLOSE` and `Drop`; never blocks,
    /// never joins (the accept loop runs on the caller's task thread).
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let ListenerKind::Tcp { listener } = &self.kind
            && let Ok(mut slot) = listener.lock()
        {
            let _ = slot.take();
        }
        self.registry.release(&self.endpoint);
    }
}

impl Drop for ListenerState {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}
