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

/// A flag needing a Cargo feature this build lacks. Takes the flag and
/// the feature that would provide it; both ride as structured fields.
pub fn feature_required(flag: &str, feature: &str) -> anyhow::Error {
    let message =
        format!("{flag} requires the `{feature}` feature (rebuild with --features {feature})");
    let diag = Diagnostic::new(
        "cli.feature_required",
        message,
        Diagnostic::make_fields(&[("flag", flag), ("feature", feature)]),
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
/// underlying `Diagnostic` by value; `{reason:#}` (full chain)
/// formatting lives here so call sites stay `format!`-free. The inner
/// is a leaf parse-surface failure (sourceless inline), so rendering
/// the bare `Diagnostic` matches the retired anyhow-chain bytes
/// exactly. Net-only: lean builds store the raw inner text and reject
/// later with the feature error.
#[cfg(feature = "net")]
pub fn publish_inner_invalid(raw: &str, reason: Diagnostic) -> anyhow::Error {
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

/// Unknown `--long` / `-s` flag. Takes the bare name plus whether it
/// was long (`--name`) or short (`-n`); the template owns the dashes.
pub fn unexpected_flag(name: &str, long: bool) -> anyhow::Error {
    let prefix = if long { "--" } else { "-" };
    let message = format!("unexpected flag: {prefix}{name}");
    let diag = Diagnostic::new(
        "cli.unexpected_flag",
        message,
        Diagnostic::make_fields(&[("flag", name)]),
    );
    anyhow::Error::new(diag)
}

/// `--script` without a value.
pub fn script_flag_needs_path() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.script_flag_needs_path",
        "--script requires a path".to_string(),
        Diagnostic::make_fields(&[("flag", "--script")]),
    );
    anyhow::Error::new(diag)
}

/// SCRIPT positional that is a URL, not a workspace path.
pub fn script_not_local(text: &str) -> anyhow::Error {
    let message = format!("not a local path: {text:?} (pass a workspace path or `-` for stdin)");
    let diag = Diagnostic::new(
        "cli.script_not_local",
        message,
        Diagnostic::make_fields(&[("got", text)]),
    );
    anyhow::Error::new(diag)
}

/// OS argument that is not valid UTF-8.
pub fn arg_not_utf8() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.arg_not_utf8",
        "argument must be UTF-8".to_string(),
        Diagnostic::make_fields(&[]),
    );
    anyhow::Error::new(diag)
}

/// lexopt plumbing failure. Takes the underlying failure;
/// `{reason:#}` formatting lives here so call sites stay
/// `format!`-free.
pub fn arg_lex_error(reason: &lexopt::Error) -> anyhow::Error {
    let reason_text = format!("{reason:#}");
    let diag = Diagnostic::new(
        "cli.arg_lex_error",
        reason_text.clone(),
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// `--remote` without a value.
pub fn remote_requires_value() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.remote_requires_value",
        "--remote requires <target>=\"<command>\"".to_string(),
        Diagnostic::make_fields(&[("flag", "--remote")]),
    );
    anyhow::Error::new(diag)
}

/// `--remote` without the `<target>="<command>"` shape (lean pure
/// validator, kept live so the flag stays diagnosed without `net`).
#[cfg(not(feature = "net"))]
pub fn remote_requires_shape(raw: &str) -> anyhow::Error {
    let message = format!("invalid --remote {raw:?}: expected <target>=\"<command>\"");
    let diag = Diagnostic::new(
        "cli.remote_requires_shape",
        message,
        Diagnostic::make_fields(&[("flag", "--remote"), ("got", raw)]),
    );
    anyhow::Error::new(diag)
}

/// No piped script on stdin and no `--shell` to justify emptiness.
pub fn script_stdin_missing() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.script_stdin_missing",
        "no stdin detected; pass --script <file> or pipe a script into stdin (use --script - if explicit)"
            .to_string(),
        Diagnostic::make_fields(&[]),
    );
    anyhow::Error::new(diag)
}

/// `--shell` without a controlling terminal.
pub fn shell_needs_tty() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.shell_needs_tty",
        "--shell requires a tty (no controlling tty available)".to_string(),
        Diagnostic::make_fields(&[("flag", "--shell")]),
    );
    anyhow::Error::new(diag)
}

/// `--shell` through `execute_with_result`, which never supports it
/// (embedder API misuse, not user input).
pub fn shell_unsupported_in_result() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.shell_unsupported_in_result",
        "execute_with_result does not support --shell".to_string(),
        Diagnostic::make_fields(&[]),
    );
    anyhow::Error::new(diag)
}

/// Windows shell re-exec staging hit a temp path without a parent
/// (internal invariant, not user input).
#[cfg(windows)]
pub fn shell_temp_missing_parent() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.shell_temp_missing_parent",
        "temp path unexpectedly missing parent".to_string(),
        Diagnostic::make_fields(&[]),
    );
    anyhow::Error::new(diag)
}

/// `--remote-serve` combined with script/shell/remote inputs.
#[cfg(feature = "net")]
pub fn remote_serve_runs_alone() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.remote_serve_runs_alone",
        "--remote-serve runs alone (no --script, --shell, --remote, or script args)".to_string(),
        Diagnostic::make_fields(&[("flag", "--remote-serve")]),
    );
    anyhow::Error::new(diag)
}

/// `--remote-serve` without piped stdio.
#[cfg(feature = "net")]
pub fn remote_serve_needs_stdio() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.remote_serve_needs_stdio",
        "--remote-serve requires piped stdio (no TTY)".to_string(),
        Diagnostic::make_fields(&[("flag", "--remote-serve")]),
    );
    anyhow::Error::new(diag)
}

/// Fetch tar arrived with no staging section (guest protocol
/// invariant, not user input).
#[cfg(feature = "net")]
pub fn fetch_tar_no_stage() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.serve_fetch_tar_no_stage",
        "serve fetch tar arrived with no stage".to_string(),
        Diagnostic::make_fields(&[]),
    );
    anyhow::Error::new(diag)
}

/// Fetch tar dir member escapes staging. Takes the member path and the
/// underlying failure; `{reason:#}` formatting lives here so call
/// sites stay `format!`-free.
#[cfg(feature = "net")]
pub fn fetch_dir_escapes(path: &str, reason: &anyhow::Error) -> anyhow::Error {
    let reason_text = format!("{reason:#}");
    let message = format!("fetch dir {path:?} escapes staging: {reason_text}");
    let diag = Diagnostic::new(
        "cli.serve_fetch_dir_escapes",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// Fetch tar file member escapes staging.
#[cfg(feature = "net")]
pub fn fetch_file_escapes(path: &str, reason: &anyhow::Error) -> anyhow::Error {
    let reason_text = format!("{reason:#}");
    let message = format!("fetch file {path:?} escapes staging: {reason_text}");
    let diag = Diagnostic::new(
        "cli.serve_fetch_file_escapes",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// Fetch tar parent directory escapes staging.
#[cfg(feature = "net")]
pub fn fetch_parent_escapes(path: &str, reason: &anyhow::Error) -> anyhow::Error {
    let reason_text = format!("{reason:#}");
    let message = format!("fetch parent {path:?} escapes staging: {reason_text}");
    let diag = Diagnostic::new(
        "cli.serve_fetch_parent_escapes",
        message,
        Diagnostic::make_fields(&[("path", path), ("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// Fetch tar symlink member refused (content arrives resolved
/// host-side; symlinks are never served).
#[cfg(feature = "net")]
pub fn fetch_symlink_refused(path: &str, target: &str) -> anyhow::Error {
    let message = format!("fetch symlink {path:?} -> {target:?} refused: fetch entries are files");
    let diag = Diagnostic::new(
        "cli.serve_fetch_symlink_refused",
        message,
        Diagnostic::make_fields(&[("path", path), ("target", target)]),
    );
    anyhow::Error::new(diag)
}

/// Serve engine worker thread panicked (internal invariant).
#[cfg(feature = "net")]
pub fn serve_engine_panicked() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.serve_engine_panicked",
        "serve engine thread panicked".to_string(),
        Diagnostic::make_fields(&[]),
    );
    anyhow::Error::new(diag)
}

/// Transfer staging tempdir failed. Takes the wrapped failure;
/// `{reason:#}` formatting lives here.
#[cfg(feature = "net")]
pub fn serve_transfer_tempdir_failed(reason: &anyhow::Error) -> anyhow::Error {
    let reason_text = format!("{reason:#}");
    let message = format!("serve transfer tempdir failed: {reason_text}");
    let diag = Diagnostic::new(
        "cli.serve_transfer_tempdir_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// Fetch descriptor decode failed. Takes the concrete serialization
/// failure; rendering lives here.
#[cfg(feature = "net")]
pub fn serve_fetch_descriptor_decode_failed(reason: &serde_json::Error) -> anyhow::Error {
    let reason_text = reason.to_string();
    let message = format!("serve fetch descriptor decode failed: {reason_text}");
    let diag = Diagnostic::new(
        "cli.serve_fetch_descriptor_decode_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// Unpack staging tempdir failed. Takes the wrapped failure;
/// `{reason:#}` formatting lives here.
#[cfg(feature = "net")]
pub fn serve_staging_tempdir_failed(reason: &anyhow::Error) -> anyhow::Error {
    let reason_text = format!("{reason:#}");
    let message = format!("serve staging tempdir failed: {reason_text}");
    let diag = Diagnostic::new(
        "cli.serve_staging_tempdir_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// Push staging tempdir failed. Takes the wrapped failure;
/// `{reason:#}` formatting lives here.
#[cfg(feature = "net")]
pub fn serve_push_tempdir_failed(reason: &anyhow::Error) -> anyhow::Error {
    let reason_text = format!("{reason:#}");
    let message = format!("serve push tempdir failed: {reason_text}");
    let diag = Diagnostic::new(
        "cli.serve_push_tempdir_failed",
        message,
        Diagnostic::make_fields(&[("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// Guest push source escapes staging. Takes the underlying failure;
/// `{reason:#}` formatting lives here.
#[cfg(feature = "net")]
pub fn push_source_escapes(guest_src: &str, reason: &anyhow::Error) -> anyhow::Error {
    let reason_text = format!("{reason:#}");
    let message = format!("guest push source {guest_src:?} escapes staging: {reason_text}");
    let diag = Diagnostic::new(
        "cli.serve_push_source_escapes",
        message,
        Diagnostic::make_fields(&[("source", guest_src), ("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// Guest push source unreadable. Takes the underlying failure;
/// `{reason:#}` formatting lives here.
#[cfg(feature = "net")]
pub fn push_source_unreadable(path: &str, reason: &anyhow::Error) -> anyhow::Error {
    let reason_text = format!("{reason:#}");
    let message = format!("guest push source {path:?} unreadable: {reason_text}");
    let diag = Diagnostic::new(
        "cli.serve_push_source_unreadable",
        message,
        Diagnostic::make_fields(&[("source", path), ("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// Guest push source missing from staging.
#[cfg(feature = "net")]
pub fn push_source_missing(guest_src: &str, reason: &anyhow::Error) -> anyhow::Error {
    let reason_text = format!("{reason:#}");
    let message = format!("guest push source {guest_src:?} missing: {reason_text}");
    let diag = Diagnostic::new(
        "cli.serve_push_source_missing",
        message,
        Diagnostic::make_fields(&[("source", guest_src), ("reason", &reason_text)]),
    );
    anyhow::Error::new(diag)
}

/// `--mcp` without piped stdio.
#[cfg(feature = "mcp")]
pub fn mcp_needs_stdio() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.mcp_needs_stdio",
        "--mcp requires piped stdio transport".to_string(),
        Diagnostic::make_fields(&[("flag", "--mcp")]),
    );
    anyhow::Error::new(diag)
}

/// `--mcp` reading the script from stdin (stdin carries JSON-RPC).
#[cfg(feature = "mcp")]
pub fn mcp_needs_script_file() -> anyhow::Error {
    let diag = Diagnostic::new(
        "cli.mcp_needs_script_file",
        "--mcp requires a script file (stdin carries the JSON-RPC stream)".to_string(),
        Diagnostic::make_fields(&[("flag", "--mcp")]),
    );
    anyhow::Error::new(diag)
}

/// `--mcp` combined with other run inputs. Takes the conflicting
/// inputs as structured items; joining lives here.
#[cfg(feature = "mcp")]
pub fn mcp_runs_alone(conflicts: &[&str]) -> anyhow::Error {
    let joined = conflicts.join(", ");
    let message = format!("--mcp runs alone (no {joined})");
    let diag = Diagnostic::new(
        "cli.mcp_runs_alone",
        message,
        Diagnostic::make_fields(&[("flag", "--mcp"), ("conflicts", &joined)]),
    );
    anyhow::Error::new(diag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "net")]
    fn serde_err() -> serde_json::Error {
        serde_json::from_slice::<serde_json::Value>(b"{bad}").unwrap_err()
    }

    #[test]
    fn feature_message_shared_across_sites() {
        for (flag, feature) in [
            ("--listen", "net"),
            ("-p", "net"),
            ("script module", "net"),
            ("--remote-serve", "net"),
            ("--mcp", "mcp"),
        ] {
            let err = feature_required(flag, feature);
            let diag = err
                .downcast_ref::<Diagnostic>()
                .expect("CLI errors stay typed");
            assert_eq!(diag.key(), "cli.feature_required");
            assert!(diag.message().contains(flag), "{diag}");
            assert!(diag.message().contains(feature), "{diag}");
        }
    }

    #[test]
    fn mcp_feature_gate_names_mcp_not_net() {
        let err = feature_required("--mcp", "mcp");
        let diag = err
            .downcast_ref::<Diagnostic>()
            .expect("CLI errors stay typed");
        let text = diag.message();
        assert!(text.contains("`mcp`"), "{diag}");
        assert!(text.contains("--features mcp"), "{diag}");
        assert!(!text.contains("net"), "{diag}");
    }

    #[test]
    fn endpoint_duplicates_name_the_key() {
        let inner = endpoint_duplicate_inner("2251");
        let diag = inner.downcast_ref::<Diagnostic>().expect("typed CLI error");
        assert_eq!(diag.key(), "cli.endpoint_duplicate_inner");

        let outer = endpoint_duplicate_outer("127.0.0.1:2222");
        let diag = outer.downcast_ref::<Diagnostic>().expect("typed CLI error");
        assert_eq!(diag.key(), "cli.endpoint_duplicate_outer");
    }

    #[test]
    fn untrusted_input_cannot_crash_cli_errors() {
        let evil = "bad\u{1f}flag".to_string();
        let err = feature_required(&evil, "net");
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
                publish_inner_invalid(
                    "2222:0",
                    Diagnostic::new(
                        "t.reason",
                        "bad endpoint".to_string(),
                        Diagnostic::make_fields(&[]),
                    ),
                ),
            ),
            ("cli.module_unlinked", module_unlinked("NET", "net")),
            ("cli.unexpected_flag", unexpected_flag("frobnicate", true)),
            ("cli.script_flag_needs_path", script_flag_needs_path()),
            ("cli.script_not_local", script_not_local("https://x")),
            ("cli.arg_not_utf8", arg_not_utf8()),
            (
                "cli.arg_lex_error",
                arg_lex_error(&lexopt::Error::UnexpectedOption("--nope".to_string())),
            ),
            ("cli.remote_requires_value", remote_requires_value()),
            #[cfg(not(feature = "net"))]
            ("cli.remote_requires_shape", remote_requires_shape("prod")),
            ("cli.script_stdin_missing", script_stdin_missing()),
            ("cli.shell_needs_tty", shell_needs_tty()),
            (
                "cli.shell_unsupported_in_result",
                shell_unsupported_in_result(),
            ),
            #[cfg(windows)]
            ("cli.shell_temp_missing_parent", shell_temp_missing_parent()),
            #[cfg(feature = "net")]
            ("cli.remote_serve_runs_alone", remote_serve_runs_alone()),
            #[cfg(feature = "net")]
            ("cli.remote_serve_needs_stdio", remote_serve_needs_stdio()),
            #[cfg(feature = "net")]
            ("cli.serve_fetch_tar_no_stage", fetch_tar_no_stage()),
            #[cfg(feature = "net")]
            (
                "cli.serve_fetch_dir_escapes",
                fetch_dir_escapes("a", &anyhow::anyhow!("root")),
            ),
            #[cfg(feature = "net")]
            (
                "cli.serve_fetch_file_escapes",
                fetch_file_escapes("a", &anyhow::anyhow!("root")),
            ),
            #[cfg(feature = "net")]
            (
                "cli.serve_fetch_parent_escapes",
                fetch_parent_escapes("a", &anyhow::anyhow!("root")),
            ),
            #[cfg(feature = "net")]
            (
                "cli.serve_fetch_symlink_refused",
                fetch_symlink_refused("a", "b"),
            ),
            #[cfg(feature = "net")]
            ("cli.serve_engine_panicked", serve_engine_panicked()),
            #[cfg(feature = "net")]
            (
                "cli.serve_transfer_tempdir_failed",
                serve_transfer_tempdir_failed(&anyhow::anyhow!("boom")),
            ),
            #[cfg(feature = "net")]
            (
                "cli.serve_fetch_descriptor_decode_failed",
                serve_fetch_descriptor_decode_failed(&serde_err()),
            ),
            #[cfg(feature = "net")]
            (
                "cli.serve_staging_tempdir_failed",
                serve_staging_tempdir_failed(&anyhow::anyhow!("boom")),
            ),
            #[cfg(feature = "net")]
            (
                "cli.serve_push_tempdir_failed",
                serve_push_tempdir_failed(&anyhow::anyhow!("boom")),
            ),
            #[cfg(feature = "net")]
            (
                "cli.serve_push_source_escapes",
                push_source_escapes("a", &anyhow::anyhow!("root")),
            ),
            #[cfg(feature = "net")]
            (
                "cli.serve_push_source_unreadable",
                push_source_unreadable("a", &anyhow::anyhow!("root")),
            ),
            #[cfg(feature = "net")]
            (
                "cli.serve_push_source_missing",
                push_source_missing("a", &anyhow::anyhow!("root")),
            ),
            #[cfg(feature = "mcp")]
            ("cli.mcp_needs_stdio", mcp_needs_stdio()),
            #[cfg(feature = "mcp")]
            ("cli.mcp_needs_script_file", mcp_needs_script_file()),
            #[cfg(feature = "mcp")]
            ("cli.mcp_runs_alone", mcp_runs_alone(&["--shell"])),
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
    fn flag_shape_messages_name_the_flag() {
        assert_eq!(
            unexpected_flag("frobnicate", true)
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "unexpected flag: --frobnicate"
        );
        assert_eq!(
            unexpected_flag("z", false)
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "unexpected flag: -z"
        );
        assert_eq!(
            script_flag_needs_path()
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "--script requires a path"
        );
        assert!(
            script_not_local("https://x/y")
                .to_string()
                .contains("\"https://x/y\"")
        );
        assert!(
            arg_lex_error(&lexopt::Error::UnexpectedOption("--nope".to_string()))
                .to_string()
                .contains("--nope")
        );
        assert_eq!(
            remote_requires_value()
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "--remote requires <target>=\"<command>\""
        );
        #[cfg(feature = "mcp")]
        assert_eq!(
            mcp_runs_alone(&["--shell", "--remote"])
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "--mcp runs alone (no --shell, --remote)"
        );
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_gate_messages() {
        assert_eq!(
            mcp_needs_stdio()
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "--mcp requires piped stdio transport"
        );
        assert_eq!(
            mcp_needs_script_file()
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "--mcp requires a script file (stdin carries the JSON-RPC stream)"
        );
    }

    #[cfg(feature = "net")]
    #[test]
    fn serve_staging_messages_name_the_member() {
        let err = fetch_dir_escapes("a", &anyhow::anyhow!("root"));
        assert!(err.to_string().contains("\"a\""), "{err}");
        assert!(err.to_string().contains("root"), "{err}");
        let err = fetch_symlink_refused("a", "b");
        assert_eq!(
            err.downcast_ref::<Diagnostic>().expect("typed").message(),
            "fetch symlink \"a\" -> \"b\" refused: fetch entries are files"
        );
        let err = push_source_unreadable("a", &anyhow::anyhow!("denied"));
        assert!(err.to_string().contains("denied"), "{err}");
        assert_eq!(
            serve_transfer_tempdir_failed(&anyhow::anyhow!("boom"))
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "serve transfer tempdir failed: boom"
        );
        assert_eq!(
            serve_fetch_descriptor_decode_failed(&serde_err())
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "serve fetch descriptor decode failed: key must be a string at line 1 column 2"
        );
        assert_eq!(
            serve_staging_tempdir_failed(&anyhow::anyhow!("boom"))
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "serve staging tempdir failed: boom"
        );
        assert_eq!(
            serve_push_tempdir_failed(&anyhow::anyhow!("boom"))
                .downcast_ref::<Diagnostic>()
                .expect("typed")
                .message(),
            "serve push tempdir failed: boom"
        );
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
            let err = publish_inner_invalid(
                "2222:0",
                Diagnostic::new(
                    "t.reason",
                    "bad endpoint".to_string(),
                    Diagnostic::make_fields(&[]),
                ),
            );
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
