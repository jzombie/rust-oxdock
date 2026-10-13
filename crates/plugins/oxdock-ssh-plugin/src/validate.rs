//! Pure validation helpers for SSH endpoint shapes.
//!
//! No I/O happens here: everything is string validation, so this module is
//! unit-testable without a network. Resolution lives in
//! `oxdock-net-plugin` (one shared resolver, no per-plugin fork); only
//! the serve wrapper with its SSH caller label stays here.

use anyhow::Result;
use oxdock_net_plugin::validate as net_validate;

/// Validate an `SSH_SERVE` endpoint without touching the network. Scripts
/// declare service names; physical mapping belongs to the host runner.
/// Delegates to the shared NET parser so shapes stay identical
/// everywhere; only the caller label is SSH's.
pub fn parse_serve_endpoint(raw: &str) -> Result<net_validate::VirtualEndpoint> {
    net_validate::parse_virtual_endpoint(raw, "SSH_SERVE")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxdock_net_plugin::{
        EndpointKey, EndpointRegistry, ServiceTarget, SocketRequirement, resolve_service_target,
    };

    #[test]
    fn serve_endpoint_delegates_to_shared_parser() {
        assert_eq!(
            parse_serve_endpoint("2222").unwrap(),
            net_validate::VirtualEndpoint::Port(2222)
        );
        assert_eq!(
            parse_serve_endpoint("demo-proxy").unwrap(),
            net_validate::VirtualEndpoint::Name("demo-proxy".to_string())
        );
        // Physical binds and ephemeral 0 are rejected in-script.
        for raw in ["127.0.0.1:2222", "0.0.0.0:2222", "0", ""] {
            parse_serve_endpoint(raw).expect_err("non-logical endpoint must fail");
        }
    }

    fn fresh_registry() -> std::sync::Arc<EndpointRegistry> {
        std::sync::Arc::new(EndpointRegistry::new(false))
    }

    fn tcp_key(endpoint: net_validate::VirtualEndpoint) -> EndpointKey {
        EndpointKey::tcp(endpoint)
    }

    /// The mandate, pinned at unit level: a numeric-looking service never
    /// resolves to loopback, mapped or not. The shared resolver is the
    /// only path, so one assertion set covers serve and dial alike.
    #[test]
    fn numeric_services_never_resolve_to_loopback() {
        let registry = fresh_registry();
        for target in ["23471", "ssh-unit-probe"] {
            let endpoint =
                net_validate::parse_virtual_endpoint(target, "SSH_CONNECT").expect("parses");
            let err = resolve_service_target(
                &registry,
                &tcp_key(endpoint),
                "SSH_CONNECT",
                SocketRequirement::SocketRequired,
            )
            .expect_err("unmapped service must bail");
            assert!(
                err.to_string().contains("has no CLI port mapping"),
                "{err:#}"
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "binds loopback TCP")]
    fn mapped_tcp_service_resolves() {
        use oxdock_net_plugin::BindingSpec;
        let registry = fresh_registry();
        registry
            .add_mapping(
                &tcp_key(net_validate::VirtualEndpoint::Name("ssh-unit".to_string())),
                BindingSpec::Exposed {
                    addr: "127.0.0.1:0".parse().expect("addr"),
                },
            )
            .expect("mapping");
        registry.bind_all().expect("bind");
        let key = tcp_key(net_validate::VirtualEndpoint::Name("ssh-unit".to_string()));
        let target = resolve_service_target(
            &registry,
            &key,
            "SSH_CONNECT",
            SocketRequirement::SocketRequired,
        )
        .expect("mapped service resolves");
        let ServiceTarget::Tcp(addr) = target else {
            panic!("mapped service must resolve TCP");
        };
        assert_ne!(addr.port(), 0, "ephemeral resolved");
    }
}
