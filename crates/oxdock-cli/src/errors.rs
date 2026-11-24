//! CLI user-facing errors behind single constructors with stable keys.
//!
//! This module owns every CLI template and key (`cli.*`). It builds on
//! the generic [`oxdock_diagnostics::Diagnostic`] transport but owns its
//! wording: changing CLI text happens here, never in the leaf crate.
//! DSL diagnostics stay owned by the DSL crates.

pub use oxdock_diagnostics::Diagnostic;
use oxdock_macros::oxdock_embed;

// CLI usage bodies, one file per Cargo feature variant: statically
// compiled assets cannot branch on Cargo features, so the script
// renders both variants (version line included, through inherited
// Cargo metadata) and Rust `#[cfg]` picks the live one.
oxdock_embed! {
    name: UsageAssets,
    script: {
        INHERIT_ENV [CARGO_PKG_VERSION, CARGO_PKG_DESCRIPTION]
        FUNC EMIT_BODY($is_net: BOOL): VOID {
            LET $net_flags: STRING = """
              --offline          open no sockets (endpoint flags require the `net` feature)
            """

            LET $tail: STRING = """
            Endpoint flags (--listen/-p) require the `net` feature (rebuild with --features net).
            """

            IF $is_net {
                $net_flags = """
                  --listen <addr>    expose a logical service port ([host:]port, repeatable)
                  -p <[host:]outer:inner>  map outer port to an inner service port or name (repeatable; outer 0 is ephemeral; bare outer binds loopback, prefix 0.0.0.0: for all interfaces)
                  --offline          open no sockets (conflicts with --listen/-p)
                  --remote TARGET=CMD    bind a REMOTE target to a stdio transport command (repeatable)
                """
                $tail = "Scripts declare logical endpoints (a port like 2251); the flags above map them to interfaces."
            }

            ECHO """
            oxdock {{ env:CARGO_PKG_VERSION }} — {{ env:CARGO_PKG_DESCRIPTION }}

            Usage: oxdock [OPTIONS] [--] [SCRIPT] [-- script-args...]
              SCRIPT             script file path (same as `--script <file>`); `-` reads stdin
              --script <file|->  script file (relative resolves under the OxDock workspace root), or `-` for stdin
              --shell            run the script, then drop into an interactive shell (requires a TTY)
              --dry-run          print the transitive import closure (file:line CMD) instead of running
              --mcp              serve EXPORTed FUNCs as MCP tools over stdio (requires the `mcp` feature)
            {{ $net_flags }}
              --help, -h         print this help and exit
              --version, -V, -v   print the version and exit

            With no script given, reads the script from stdin (must be piped unless `--shell`).
            Everything after SCRIPT is a verbatim script arg (read via `STD::ARGV()`); one leading `--` there is a separator and dropped, so `--dry-run` after the script stays data.
            {{ $tail }}
            """
        }

        LET $p: PIPE
        WITH_IO [stdout=$p] {
            EMIT_BODY(true)
        }
        WITH_IO [stdin=$p] WRITE dist/usage-net.txt
        WITH_IO [stdout=$p] {
            EMIT_BODY(false)
        }
        WITH_IO [stdin=$p] WRITE dist/usage-lean.txt
    },
    out_dir: "usage-prebuilt",
}

/// Human-readable CLI usage, printed for `--help`/`-h`. The full
/// text, version line included, is rendered once at compile time by
/// the `UsageAssets` script above. Feature selection stays in Rust
/// `#[cfg]`: the DSL has no feature namespace, so each variant lives
/// in its own asset file.
pub fn usage() -> String {
    #[cfg(feature = "net")]
    let asset = "dist/usage-net.txt";
    #[cfg(not(feature = "net"))]
    let asset = "dist/usage-lean.txt";
    let file = UsageAssets::get(asset).expect("usage must be embedded");
    str::from_utf8(file.data.as_ref())
        .expect("usage is UTF-8")
        .to_string()
}

/// `--listen`/`-p` without the `net` feature.
pub fn feature_required(flag: &str) -> anyhow::Error {
    let message = format!("{flag} requires the `net` feature (rebuild with --features net)");
    let diag = Diagnostic::new(
        "cli.feature_required",
        message,
        Diagnostic::make_fields(&[("flag", flag)]),
    );
    anyhow::Error::new(diag)
}

/// `--offline` combined with socket flags.
pub fn offline_conflict() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.offline_conflict",
        "--offline conflicts with --listen/-p (offline runs open no sockets)".to_string(),
        Diagnostic::make_fields(&[]),
    );
    anyhow::Error::new(diag)
}

/// Same virtual endpoint mapped twice.
pub fn endpoint_duplicate_inner(endpoint: &str) -> anyhow::Error {
    let message = format!("virtual endpoint '{endpoint}' is mapped twice");
    let diag = Diagnostic::new(
        "cli.endpoint_duplicate_inner",
        message,
        Diagnostic::make_fields(&[("endpoint", endpoint)]),
    );
    anyhow::Error::new(diag)
}

/// Same outer socket address mapped twice.
pub fn endpoint_duplicate_outer(addr: &str) -> anyhow::Error {
    let message = format!("outer socket address {addr} is mapped twice");
    let diag = Diagnostic::new(
        "cli.endpoint_duplicate_outer",
        message,
        Diagnostic::make_fields(&[("address", addr)]),
    );
    anyhow::Error::new(diag)
}

/// `--listen` without a value: an address is required.
pub fn listen_requires_address() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.listen_requires_address",
        "--listen requires an address ([host:]port)".to_string(),
        Diagnostic::make_fields(&[("flag", "--listen")]),
    );
    anyhow::Error::new(diag)
}

/// `--listen` with a non-numeric bare port.
pub fn listen_port_numeric(raw: &str) -> anyhow::Error {
    let message = format!("--listen port must be numeric 1-65535, got {raw:?}");
    let diag = Diagnostic::new(
        "cli.listen_port_numeric",
        message,
        Diagnostic::make_fields(&[("flag", "--listen"), ("got", raw)]),
    );
    anyhow::Error::new(diag)
}

/// `--listen` port `0`: an exposed ephemeral port has no stable
/// service key (that shape belongs to `-p 0:<endpoint>`).
pub fn listen_port_zero() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.listen_port_zero",
        "--listen port 0 has no stable service key (use -p 0:<endpoint> for an ephemeral outer port)"
            .to_string(),
        Diagnostic::make_fields(&[("flag", "--listen")]),
    );
    anyhow::Error::new(diag)
}

/// `--listen` with an unparsable socket address.
pub fn listen_address_invalid(raw: &str) -> anyhow::Error {
    let message = format!("--listen address must be [host:]port (try 0.0.0.0:2251), got {raw:?}");
    let diag = Diagnostic::new(
        "cli.listen_address_invalid",
        message,
        Diagnostic::make_fields(&[("flag", "--listen"), ("got", raw)]),
    );
    anyhow::Error::new(diag)
}

/// `-p` without the `outer:inner` shape, or with an empty inner.
pub fn publish_requires_shape(raw: &str) -> anyhow::Error {
    let message = format!(
        "-p requires [host:]outer:inner (try -p 2222:2251 or -p 127.0.0.1:1234:ssh-server), got {raw:?}"
    );
    let diag = Diagnostic::new(
        "cli.publish_requires_shape",
        message,
        Diagnostic::make_fields(&[("flag", "-p"), ("got", raw)]),
    );
    anyhow::Error::new(diag)
}

/// `-p` with an unparsable outer address.
pub fn publish_outer_invalid(raw: &str) -> anyhow::Error {
    let message = format!(
        "-p outer address invalid in {raw:?} (expected a port 0-65535 or a [host:]port address)"
    );
    let diag = Diagnostic::new(
        "cli.publish_outer_invalid",
        message,
        Diagnostic::make_fields(&[("flag", "-p"), ("got", raw)]),
    );
    anyhow::Error::new(diag)
}

/// `-p` inner endpoint rejected by the endpoint grammar. Takes the
/// underlying failure; `{reason:#}` (full chain) formatting lives
/// here so call sites stay `format!`-free. Net-only: lean builds store
/// the raw inner text and reject later with the feature error.
#[cfg(feature = "net")]
pub fn publish_inner_invalid(raw: &str, reason: &anyhow::Error) -> anyhow::Error {
    let reason_text = format!("{reason:#}");
    let message = format!("-p inner endpoint invalid in {raw:?}: {reason_text}");
    let diag = Diagnostic::new(
        "cli.publish_inner_invalid",
        message,
        Diagnostic::make_fields(&[("flag", "-p"), ("got", raw), ("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// Script imports a module the CLI knows but this build did not link.
/// Takes the module name and the Cargo feature that would link it.
pub fn module_unlinked(module: &str, feature: &str) -> anyhow::Error {
    let message =
        format!("module `{module}` is known but not linked (rebuild with --features {feature})");
    let diag = Diagnostic::new(
        "cli.module_unlinked",
        message,
        Diagnostic::make_fields(&[("module", module), ("feature", feature)]),
    );
    anyhow::Error::new(diag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_message_shared_across_three_sites() {
        for flag in ["--listen", "-p", "script module"] {
            let err = feature_required(flag);
            let diag = err
                .downcast_ref::<Diagnostic>()
                .expect("CLI errors stay typed");
            assert_eq!(diag.key(), "cli.feature_required");
            assert!(diag.message().contains(flag), "{diag}");
        }
    }

    #[test]
    fn endpoint_duplicates_name_the_key() {
        let inner = endpoint_duplicate_inner("2251");
        let diag = inner
            .downcast_ref::<Diagnostic>()
            .expect("typed CLI error");
        assert_eq!(diag.key(), "cli.endpoint_duplicate_inner");

        let outer = endpoint_duplicate_outer("127.0.0.1:2222");
        let diag = outer
            .downcast_ref::<Diagnostic>()
            .expect("typed CLI error");
        assert_eq!(diag.key(), "cli.endpoint_duplicate_outer");
    }

    #[test]
    fn untrusted_input_cannot_crash_cli_errors() {
        let evil = "bad\u{1f}flag".to_string();
        let err = feature_required(&evil);
        let diag = err.downcast_ref::<Diagnostic>().expect("typed");
        assert!(!diag.message().contains('\u{1f}'));
    }

    #[test]
    fn net_shape_errors_carry_stable_keys() {
        let cases: &[(&str, anyhow::Error)] = &[
            ("cli.listen_requires_address", listen_requires_address()),
            ("cli.listen_port_numeric", listen_port_numeric("abc")),
            ("cli.listen_port_zero", listen_port_zero()),
            (
                "cli.listen_address_invalid",
                listen_address_invalid("0.0.0.0:99999"),
            ),
            ("cli.publish_requires_shape", publish_requires_shape("2251")),
            (
                "cli.publish_outer_invalid",
                publish_outer_invalid("abc:2251"),
            ),
            // Net-only: lean builds store the raw inner and reject with
            // the feature error instead.
            #[cfg(feature = "net")]
            (
                "cli.publish_inner_invalid",
                publish_inner_invalid("2222:0", &anyhow::anyhow!("bad endpoint")),
            ),
            ("cli.module_unlinked", module_unlinked("NET", "net")),
        ];
        let mut seen = std::collections::HashSet::new();
        for (key, err) in cases {
            let diag = err
                .downcast_ref::<Diagnostic>()
                .expect("CLI errors stay typed");
            assert_eq!(&diag.key(), key, "{diag}");
            assert!(seen.insert(*key), "duplicate key {key}");
        }
    }

    #[test]
    fn net_shape_messages_name_the_flag_and_value() {
        assert_eq!(
            listen_requires_address()
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "--listen requires an address ([host:]port)"
        );
        assert!(
            publish_requires_shape("2251")
                .to_string()
                .contains("\"2251\"")
        );
        #[cfg(feature = "net")]
        {
            let err = publish_inner_invalid("2222:0", &anyhow::anyhow!("bad endpoint"));
            let text = err.to_string();
            assert!(text.contains("\"2222:0\""), "{text}");
            assert!(text.contains("bad endpoint"), "{text}");
        }
        assert_eq!(
            module_unlinked("NET", "net")
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "module `NET` is known but not linked (rebuild with --features net)"
        );
    }
}
