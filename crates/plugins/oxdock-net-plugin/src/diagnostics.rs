//! Net user-facing errors behind single constructors with stable keys.
//!
//! This module owns every net template and key (`net.*`). It builds on
//! the generic [`oxdock_diagnostics::Diagnostic`] transport but owns its
//! wording: changing net text happens here, never at the call sites.
//! Call sites pass structured data only, never `format!` strings.
//! Fixed-shape failures are parameterless; I/O failures take the
//! concrete `std::io::Error`; chained failures take `&anyhow::Error`
//! and render `{reason:#}` inline (never `with_source`, avoiding
//! double-print in `Display`); shell tokenizing takes the concrete
//! `shell_words::ParseError`. Same-failure-mode shares one key with
//! the invoker or option as a structured field.

use oxdock_diagnostics::Diagnostic;
use oxdock_parser::{HANDLE_TYPE_NAME, KEYWORD_ASYNC, KEYWORD_LET, KEYWORD_REMOTE};

use crate::validate::{EndpointKey, VirtualEndpoint};

/// Structured field keys. Single spelling for the whole module:
/// constructors reference these, never literals, so a key rename is
/// one edit.
const FIELD_FUNC: &str = "func";
const FIELD_WORKER: &str = "worker";
const FIELD_ERR: &str = "err";
const FIELD_KEY: &str = "key";
const FIELD_ENDPOINT: &str = "endpoint";
const FIELD_NOUN: &str = "noun";
const FIELD_TEXT: &str = "text";
const FIELD_RAW: &str = "raw";
const FIELD_URL: &str = "url";
const FIELD_HOST: &str = "host";
const FIELD_SCHEME: &str = "scheme";
const FIELD_STATUS: &str = "status";
const FIELD_OPTION: &str = "option";
const FIELD_VALUE: &str = "value";
const FIELD_TAG: &str = "tag";
const FIELD_TARGET: &str = "target";
const FIELD_REASON: &str = "reason";
const FIELD_GUEST: &str = "guest";
const FIELD_PATH: &str = "path";
const FIELD_DECLARED: &str = "declared";

// ---------------------------------------------------------------------------
// Pump (`net.pump.*`, `bridge.rs`)
// ---------------------------------------------------------------------------

/// Pump worker panicked. Takes the caller label plus the worker name;
/// rendering lives here.
pub fn pump_worker_panicked(func: &str, worker: &str) -> Diagnostic {
    let message = format!("{func} {worker} panicked");
    Diagnostic::new(
        "net.pump.worker_panicked",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_WORKER, worker)]),
    )
}

/// Socket shutdown failed. Takes the caller label plus the I/O
/// failure; rendering lives here.
pub fn pump_socket_shutdown_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} socket shutdown failed: {err_text}");
    Diagnostic::new(
        "net.pump.socket_shutdown_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
}

/// Stdout lock poisoned. Takes the caller label; rendering lives here.
/// Shared by the socket and memory output paths: one key, one mode.
pub fn pump_stdout_lock_poisoned(func: &str) -> Diagnostic {
    let message = format!("{func} stdout lock poisoned");
    Diagnostic::new(
        "net.pump.stdout_lock_poisoned",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func)]),
    )
}

/// Socket read failed. Takes the caller label plus the I/O failure;
/// rendering lives here.
pub fn pump_socket_read_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} socket read failed: {err_text}");
    Diagnostic::new(
        "net.pump.socket_read_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
}

/// Pump interrupted by cancellation. Takes the caller label; rendering
/// lives here. Shared by the socket and memory supervisors: one key,
/// one mode.
pub fn pump_interrupted_by_cancellation(func: &str) -> Diagnostic {
    let message = format!("{func} interrupted by cancellation");
    Diagnostic::new(
        "net.pump.interrupted_by_cancellation",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func)]),
    )
}

/// Memory pipe lock poisoned. Takes the caller label; rendering lives
/// here.
pub fn pump_memory_pipe_lock_poisoned(func: &str) -> Diagnostic {
    let message = format!("{func} memory pipe lock poisoned");
    Diagnostic::new(
        "net.pump.memory_pipe_lock_poisoned",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func)]),
    )
}

/// Memory pipe read failed. Takes the caller label plus the I/O
/// failure; rendering lives here.
pub fn pump_memory_pipe_read_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} memory pipe read failed: {err_text}");
    Diagnostic::new(
        "net.pump.memory_pipe_read_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
}

/// Stdin pipe read failed. Takes the caller label plus the I/O
/// failure; rendering lives here. Shared by the socket and memory
/// input paths: one key, one mode.
pub fn pump_stdin_pipe_read_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} stdin pipe read failed: {err_text}");
    Diagnostic::new(
        "net.pump.stdin_pipe_read_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
}

/// Socket write failed. Takes the caller label plus the I/O
/// failure; rendering lives here.
pub fn pump_socket_write_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} socket write failed: {err_text}");
    Diagnostic::new(
        "net.pump.socket_write_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
}

/// Stdout pipe write failed. Takes the caller label plus the I/O
/// failure; rendering lives here. Shared by the socket and memory
/// output paths: one key, one mode.
pub fn pump_stdout_pipe_write_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} stdout pipe write failed: {err_text}");
    Diagnostic::new(
        "net.pump.stdout_pipe_write_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
}

/// Stdout pipe flush failed. Takes the caller label plus the I/O
/// failure; rendering lives here. Shared by the socket and memory
/// output paths: one key, one mode.
pub fn pump_stdout_pipe_flush_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} stdout pipe flush failed: {err_text}");
    Diagnostic::new(
        "net.pump.stdout_pipe_flush_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
}

/// NET pump cannot borrow the input pipe. Takes the underlying typed
/// failure by value; its text inlines so the chain renders
/// byte-identical to the retired context layer.
pub fn pump_borrow_input_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("NET pump cannot borrow the input pipe: {reason_text}");
    Diagnostic::new(
        "net.pump.borrow_input_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_ERR, reason_text.as_str())]),
    )
}

/// NET pump cannot borrow the output pipe. Takes the underlying typed
/// failure by value; its text inlines so the chain renders
/// byte-identical to the retired context layer.
pub fn pump_borrow_output_failed(reason: Diagnostic) -> Diagnostic {
    let reason_text = format!("{reason:#}");
    let message = format!("NET pump cannot borrow the output pipe: {reason_text}");
    Diagnostic::new(
        "net.pump.borrow_output_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_ERR, reason_text.as_str())]),
    )
}

/// Socket clone failed. Takes the caller label plus the I/O failure;
/// rendering lives here. Shared by both clone sites in the socket
/// supervisor: one key, one mode.
pub fn pump_socket_clone_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} failed to clone socket: {err_text}");
    Diagnostic::new(
        "net.pump.socket_clone_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
}

/// Memory pipe write failed. Takes the caller label plus the I/O
/// failure; rendering lives here.
pub fn pump_memory_pipe_write_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} memory pipe write failed: {err_text}");
    Diagnostic::new(
        "net.pump.memory_pipe_write_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
}

/// Memory pipe flush failed. Takes the caller label plus the I/O
/// failure; rendering lives here.
pub fn pump_memory_pipe_flush_failed(func: &str, err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("{func} memory pipe flush failed: {err_text}");
    Diagnostic::new(
        "net.pump.memory_pipe_flush_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ERR, err_text.as_str())]),
    )
}

// ---------------------------------------------------------------------------
// Fetch (`net.fetch.*`, `fetch.rs`)
// ---------------------------------------------------------------------------

/// URL without a scheme. Takes the URL; `Debug` quoting lives here.
pub fn fetch_url_missing_scheme(url: &str) -> Diagnostic {
    let message = format!("NET_FETCH() needs an https URL, got {url:?} (missing '://')");
    Diagnostic::new(
        "net.fetch.url_missing_scheme",
        message,
        Diagnostic::make_fields(&[(FIELD_URL, url)]),
    )
}

/// URL that is not a usable https URL. Takes the URL; `Debug` quoting
/// lives here.
pub fn fetch_url_not_https(url: &str) -> Diagnostic {
    let message = format!("NET_FETCH() needs an https URL, got {url:?}");
    Diagnostic::new(
        "net.fetch.url_not_https",
        message,
        Diagnostic::make_fields(&[(FIELD_URL, url)]),
    )
}

/// Cleartext http to a non-loopback host. Takes the host; `Debug`
/// quoting lives here.
pub fn fetch_refuses_cleartext_http(host: &str) -> Diagnostic {
    let message =
        format!("NET_FETCH() refuses cleartext http to non-loopback host {host:?}: use https");
    Diagnostic::new(
        "net.fetch.refuses_cleartext_http",
        message,
        Diagnostic::make_fields(&[(FIELD_HOST, host)]),
    )
}

/// Unsupported URL scheme. Takes the scheme; `Debug` quoting lives
/// here.
pub fn fetch_unsupported_scheme(scheme: &str) -> Diagnostic {
    let message = format!(
        "NET_FETCH() supports https URLs (plus loopback http for tests), got scheme {scheme:?}"
    );
    Diagnostic::new(
        "net.fetch.unsupported_scheme",
        message,
        Diagnostic::make_fields(&[(FIELD_SCHEME, scheme)]),
    )
}

/// Too many redirects. Takes the initial URL; `Debug` quoting lives
/// here.
pub fn fetch_too_many_redirects(initial_url: &str) -> Diagnostic {
    let message = format!("NET_FETCH({initial_url:?}) failed: too many redirects");
    Diagnostic::new(
        "net.fetch.too_many_redirects",
        message,
        Diagnostic::make_fields(&[(FIELD_URL, initial_url)]),
    )
}

/// Redirect without a `Location` header. Takes the current URL;
/// `Debug` quoting lives here.
pub fn fetch_redirect_missing_location(current_url: &str) -> Diagnostic {
    let message = format!("NET_FETCH({current_url:?}) redirect missing Location header");
    Diagnostic::new(
        "net.fetch.redirect_missing_location",
        message,
        Diagnostic::make_fields(&[(FIELD_URL, current_url)]),
    )
}

/// Non-2xx status. Takes the current URL plus the status; rendering
/// lives here.
pub fn fetch_http_status(current_url: &str, status: u16) -> Diagnostic {
    let message = format!("NET_FETCH({current_url:?}) failed: http status: {status}");
    Diagnostic::new(
        "net.fetch.http_status",
        message,
        Diagnostic::make_fields(&[
            (FIELD_URL, current_url),
            (FIELD_STATUS, &status.to_string()),
        ]),
    )
}

/// Transport or body-read failure. Takes the current URL plus the
/// wrapped ureq failure; the chain renders inline. Shared by the
/// initial call and the body read, which retire the same context
/// text: one key, one mode. The `&anyhow::Error` carries ureq's own
/// source chain into `{err:#}` so the bytes match the retired
/// `.with_context` layer exactly.
pub fn fetch_request_failed(current_url: &str, err: &anyhow::Error) -> Diagnostic {
    let err_text = format!("{err:#}");
    let message = format!("NET_FETCH({current_url:?}) failed: {err_text}");
    Diagnostic::new(
        "net.fetch.request_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_URL, current_url), (FIELD_ERR, err_text.as_str())]),
    )
}

/// Non-ASCII redirect `Location`. Takes the current URL plus the
/// wrapped header-decode failure; the chain renders inline.
pub fn fetch_redirect_location_not_ascii(current_url: &str, err: &anyhow::Error) -> Diagnostic {
    let err_text = format!("{err:#}");
    let message =
        format!("NET_FETCH({current_url:?}) redirect Location is not visible ASCII: {err_text}");
    Diagnostic::new(
        "net.fetch.redirect_location_not_ascii",
        message,
        Diagnostic::make_fields(&[(FIELD_URL, current_url), (FIELD_ERR, err_text.as_str())]),
    )
}

// ---------------------------------------------------------------------------
// Endpoints (`net.endpoint.*`, `endpoints.rs`)
// ---------------------------------------------------------------------------

/// Missing CLI port mapping. Takes the caller label plus the endpoint;
/// rendering lives here. Shared by serve, dial, and pre-run paths
/// across both plugins: one key, one mode.
pub fn no_cli_mapping(func: &str, endpoint: &VirtualEndpoint) -> Diagnostic {
    let endpoint_text = endpoint.to_string();
    let message = format!(
        "{func} service '{endpoint_text}' has no CLI port mapping; pass -p <host_port>:{endpoint_text} at invocation"
    );
    Diagnostic::new(
        "net.endpoint.no_cli_mapping",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ENDPOINT, endpoint_text.as_str())]),
    )
}

/// Duplicate endpoint mapping. Takes the key; rendering lives here.
pub fn duplicate_mapping(key: &EndpointKey) -> Diagnostic {
    let key_text = key.to_string();
    let message = format!("duplicate endpoint mapping for '{key_text}'");
    Diagnostic::new(
        "net.endpoint.duplicate_mapping",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key_text.as_str())]),
    )
}

/// Endpoint with no bound socket. Takes the key; rendering lives here.
pub fn endpoint_not_bound(key: &EndpointKey) -> Diagnostic {
    let key_text = key.to_string();
    let message = format!("virtual endpoint '{key_text}' is not bound");
    Diagnostic::new(
        "net.endpoint.not_bound",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key_text.as_str())]),
    )
}

/// Socketless service where a socket was required. Takes the caller
/// label, the endpoint, and the caller noun (`port` / `address`);
/// rendering lives here.
pub fn virtual_channel_no_physical(
    func: &str,
    endpoint: &VirtualEndpoint,
    noun: &str,
) -> Diagnostic {
    let endpoint_text = endpoint.to_string();
    let message = format!(
        "{func} service '{endpoint_text}' is running as an in-memory virtual channel with no physical {noun}"
    );
    Diagnostic::new(
        "net.endpoint.virtual_channel_no_physical",
        message,
        Diagnostic::make_fields(&[
            (FIELD_FUNC, func),
            (FIELD_ENDPOINT, endpoint_text.as_str()),
            (FIELD_NOUN, noun),
        ]),
    )
}

/// Service with no runner mapping. Takes the caller label plus the
/// endpoint; rendering lives here.
pub fn service_not_mapped(func: &str, endpoint: &VirtualEndpoint) -> Diagnostic {
    let endpoint_text = endpoint.to_string();
    let message = format!(
        "{func} service '{endpoint_text}' is not mapped by the runner (map it with -p/--listen)"
    );
    Diagnostic::new(
        "net.endpoint.service_not_mapped",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_ENDPOINT, endpoint_text.as_str())]),
    )
}

/// Slot mapped but never bound. Takes the caller label plus the key;
/// rendering lives here. Shared by serve, dial, introspection, and
/// pre-run paths: one key, one mode.
pub fn never_bound(func: &str, key: &EndpointKey) -> Diagnostic {
    let key_text = key.to_string();
    let message = format!("{func}: '{key_text}' was never bound (the runner must call bind_all)");
    Diagnostic::new(
        "net.endpoint.never_bound",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_KEY, key_text.as_str())]),
    )
}

/// Bare endpoint bound under both protocols. Takes the trimmed text;
/// `Debug` quoting lives here.
pub fn ambiguous_endpoint(text: &str) -> Diagnostic {
    let message = format!(
        "ambiguous virtual endpoint '{text}': both TCP and UDP bindings exist; qualify as 'tcp/{text}' or 'udp/{text}'"
    );
    Diagnostic::new(
        "net.endpoint.ambiguous",
        message,
        Diagnostic::make_fields(&[(FIELD_TEXT, text)]),
    )
}

/// Claim of an unmapped service. Takes the caller label plus the key;
/// rendering lives here.
pub fn unknown_service(func: &str, key: &EndpointKey) -> Diagnostic {
    let key_text = key.to_string();
    let message = format!("{func}: unknown service '{key_text}' (map it with -p/--listen)");
    Diagnostic::new(
        "net.endpoint.unknown_service",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_KEY, key_text.as_str())]),
    )
}

/// Second claim of a busy slot. Takes the caller label plus the key;
/// rendering lives here.
pub fn already_claimed(func: &str, key: &EndpointKey) -> Diagnostic {
    let key_text = key.to_string();
    let message = format!("{func}: '{key_text}' is already claimed");
    Diagnostic::new(
        "net.endpoint.already_claimed",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_KEY, key_text.as_str())]),
    )
}

/// Claim of a dial-only mapping. Takes the caller label plus the key;
/// rendering lives here.
pub fn dial_only_mapping(func: &str, key: &EndpointKey) -> Diagnostic {
    let key_text = key.to_string();
    let message = format!(
        "{func}: '{key_text}' is a dial-only mapping (the runner owns the socket; LISTEN elsewhere)"
    );
    Diagnostic::new(
        "net.endpoint.dial_only_mapping",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_KEY, key_text.as_str())]),
    )
}

/// Enqueue against an unmapped service. `NET_CONNECT` only reaches
/// this path, so the caller is fixed; takes the key.
pub fn connect_unknown_service(key: &EndpointKey) -> Diagnostic {
    let key_text = key.to_string();
    let message = format!("NET_CONNECT: unknown service '{key_text}' (map it with -p/--listen)");
    Diagnostic::new(
        "net.endpoint.connect_unknown_service",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key_text.as_str())]),
    )
}

/// Memory queue saturation. Takes the key; rendering lives here.
pub fn connect_queue_full(key: &EndpointKey) -> Diagnostic {
    let key_text = key.to_string();
    let message = format!("NET_CONNECT: memory connection queue for '{key_text}' is full");
    Diagnostic::new(
        "net.endpoint.connect_queue_full",
        message,
        Diagnostic::make_fields(&[(FIELD_KEY, key_text.as_str())]),
    )
}

/// Non-literal service name at pre-run. Takes the caller label;
/// rendering lives here.
pub fn service_names_must_be_literal(func: &str) -> Diagnostic {
    let message = format!(
        "{func} service names must be string literals so the runner can verify bindings before execution"
    );
    Diagnostic::new(
        "net.endpoint.service_names_must_be_literal",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func)]),
    )
}

/// Offline service where a socket was required. Takes the caller
/// label plus the key; rendering lives here. Shared by the offline
/// registry and offline-slot paths: one key, one mode.
pub fn service_offline(func: &str, key: &EndpointKey) -> Diagnostic {
    let key_text = key.to_string();
    let message = format!("{func}: '{key_text}' is offline");
    Diagnostic::new(
        "net.endpoint.service_offline",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_KEY, key_text.as_str())]),
    )
}

// ---------------------------------------------------------------------------
// Validation (`net.validate.*`, `validate.rs`)
// ---------------------------------------------------------------------------

/// Non-numeric port text. Parameterless: the wording is fixed.
pub fn port_not_numeric() -> Diagnostic {
    Diagnostic::new(
        "net.validate.port_not_numeric",
        "port must be numeric 0-65535".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Unknown protocol qualifier. Takes the endpoint text; `Debug`
/// quoting lives here.
pub fn unknown_protocol_qualifier(text: &str) -> Diagnostic {
    let message = format!(
        "unknown protocol qualifier in {text:?} (expected 'tcp/...', 'udp/...', '.../tcp', or '.../udp')"
    );
    Diagnostic::new(
        "net.validate.unknown_protocol_qualifier",
        message,
        Diagnostic::make_fields(&[(FIELD_TEXT, text)]),
    )
}

/// Qualified-endpoint parse failure. Takes the caller label, the raw
/// text, and the underlying typed failure by value; the reason text
/// renders inline.
pub fn endpoint_ref_invalid(func: &str, raw: &str, reason: Diagnostic) -> Diagnostic {
    let err_text = format!("{reason:#}");
    let message = format!("{func} invalid endpoint {raw:?}: {err_text}");
    Diagnostic::new(
        "net.validate.endpoint_ref_invalid",
        message,
        Diagnostic::make_fields(&[
            (FIELD_FUNC, func),
            (FIELD_RAW, raw),
            (FIELD_ERR, err_text.as_str()),
        ]),
    )
}

/// Empty endpoint text. Takes the caller label plus the raw text; the
/// `{func} invalid endpoint {raw:?}` prefix renders here.
pub fn endpoint_empty(func: &str, raw: &str) -> Diagnostic {
    let message =
        format!("{func} invalid endpoint {raw:?}: expected a port 1-65535 or a service name");
    Diagnostic::new(
        "net.validate.endpoint_empty",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_RAW, raw)]),
    )
}

/// `host:port` bind shape in-script. Takes the caller label plus the
/// raw text; the prefix renders here.
pub fn endpoint_host_port(func: &str, raw: &str) -> Diagnostic {
    let message = format!(
        "{func} invalid endpoint {raw:?}: scripts declare logical endpoints, not host:port binds (the host runner maps them with --listen/-p)"
    );
    Diagnostic::new(
        "net.validate.endpoint_host_port",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_RAW, raw)]),
    )
}

/// Illegal endpoint characters. Takes the caller label plus the raw
/// text; the prefix renders here.
pub fn endpoint_bad_chars(func: &str, raw: &str) -> Diagnostic {
    let message = format!("{func} invalid endpoint {raw:?}: service names use [A-Za-z0-9_-] only");
    Diagnostic::new(
        "net.validate.endpoint_bad_chars",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_RAW, raw)]),
    )
}

/// Numeric parse failure inside endpoint validation. Takes the caller
/// label, the raw text, and the underlying typed failure by value;
/// the reason text renders inline.
pub fn endpoint_port_parse(func: &str, raw: &str, reason: Diagnostic) -> Diagnostic {
    let err_text = format!("{reason:#}");
    let message = format!("{func} invalid endpoint {raw:?}: {err_text}");
    Diagnostic::new(
        "net.validate.endpoint_port_parse",
        message,
        Diagnostic::make_fields(&[
            (FIELD_FUNC, func),
            (FIELD_RAW, raw),
            (FIELD_ERR, err_text.as_str()),
        ]),
    )
}

/// Reserved port zero in-script. Takes the caller label plus the raw
/// text; the prefix renders here.
pub fn endpoint_port_zero(func: &str, raw: &str) -> Diagnostic {
    let message = format!(
        "{func} invalid endpoint {raw:?}: port 0 is reserved for the CLI outer mapping (use -p 0:<endpoint>); scripts declare fixed ports 1-65535 or service names"
    );
    Diagnostic::new(
        "net.validate.endpoint_port_zero",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_RAW, raw)]),
    )
}

/// Malformed service name. Takes the caller label plus the raw text;
/// the prefix renders here.
pub fn endpoint_bad_shape(func: &str, raw: &str) -> Diagnostic {
    let message = format!(
        "{func} invalid endpoint {raw:?}: service names are 1-64 chars of [A-Za-z0-9_-] with at least one letter, '-' or '_'"
    );
    Diagnostic::new(
        "net.validate.endpoint_bad_shape",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_RAW, raw)]),
    )
}

/// Reserved host name as a service name. Takes the caller label plus
/// the raw text; trimming and `Debug` quoting live here.
pub fn endpoint_reserved(func: &str, raw: &str) -> Diagnostic {
    let text = raw.trim();
    let message = format!(
        "{func} invalid endpoint {raw:?}: {text:?} is a reserved host name, not a service name"
    );
    Diagnostic::new(
        "net.validate.endpoint_reserved",
        message,
        Diagnostic::make_fields(&[(FIELD_FUNC, func), (FIELD_RAW, raw), (FIELD_TEXT, text)]),
    )
}

// ---------------------------------------------------------------------------
// Host functions (`net.func.*`, `funcs.rs`)
// ---------------------------------------------------------------------------

/// Protocol-qualified `NET_LISTEN` endpoint. Takes the bind text;
/// `Debug` quoting lives here.
pub fn listen_protocol_qualified(bind: &str) -> Diagnostic {
    let message = format!(
        "NET_LISTEN() invalid endpoint {bind:?}: protocol-qualified endpoints cannot be claimed (UDP listen is not supported); observe -p mappings with NET_PORT/NET_ADDR instead"
    );
    Diagnostic::new(
        "net.func.listen_protocol_qualified",
        message,
        Diagnostic::make_fields(&[(FIELD_TEXT, bind)]),
    )
}

/// `NET_ACCEPT` outside `ASYNC`. Parameterless: the wrap hint lives
/// here.
pub fn accept_requires_async() -> Diagnostic {
    Diagnostic::new(
        "net.func.accept_requires_async",
        format!(
            "NET_ACCEPT requires {KEYWORD_ASYNC}: wrap it as {KEYWORD_LET} $t: {HANDLE_TYPE_NAME}<T> = {KEYWORD_ASYNC} {{ NET_ACCEPT($l, $in, $out, {{}}) }}"
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `NET_ACCEPT` wait cancelled. Parameterless: the wording is fixed.
/// Shared by the offline, memory, and TCP accept loops: one key, one
/// mode.
pub fn accept_interrupted() -> Diagnostic {
    Diagnostic::new(
        "net.func.accept_interrupted",
        "NET_ACCEPT interrupted by cancellation".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Accept against a closed listener. Parameterless: the wording is
/// fixed. Shared by every accept path: one key, one mode.
pub fn accept_listener_closed() -> Diagnostic {
    Diagnostic::new(
        "net.func.accept_listener_closed",
        "NET_ACCEPT: listener is closed".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Accept socket failure. Takes the I/O failure; rendering lives here.
pub fn accept_failed(err: &std::io::Error) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("NET_ACCEPT accept failed: {err_text}");
    Diagnostic::new(
        "net.func.accept_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_ERR, err_text.as_str())]),
    )
}

/// `NET_CONNECT` outside `ASYNC`. Parameterless: the wrap hint lives
/// here.
pub fn connect_requires_async() -> Diagnostic {
    Diagnostic::new(
        "net.func.connect_requires_async",
        format!(
            "NET_CONNECT requires {KEYWORD_ASYNC}: wrap it as {KEYWORD_LET} $t: {HANDLE_TYPE_NAME}<T> = {KEYWORD_ASYNC} {{ NET_CONNECT($target, $in, $out, {{}}) }}"
        ),
        Diagnostic::make_fields(&[]),
    )
}

/// `NET_CONNECT` while offline. Parameterless: the wording is fixed.
pub fn connect_offline() -> Diagnostic {
    Diagnostic::new(
        "net.func.connect_offline",
        "NET_CONNECT failed: engine running in --offline mode".to_string(),
        Diagnostic::make_fields(&[]),
    )
}

/// Negative `NET_FETCH` numeric option. Takes the option name (the
/// `OPT_*` constants) plus the value; rendering lives here. Shared by
/// `max_body_bytes` / `max_redirects`: one key, one mode.
pub fn fetch_option_negative(option: &str, value: i64) -> Diagnostic {
    let message = format!("NET_FETCH option '{option}' must be non-negative, got {value}");
    Diagnostic::new(
        "net.func.fetch_option_negative",
        message,
        Diagnostic::make_fields(&[(FIELD_OPTION, option), (FIELD_VALUE, &value.to_string())]),
    )
}

/// `NET_FETCH` redirect budget outside `u32`. Takes the option name
/// plus the value; rendering lives here.
pub fn fetch_option_exceeds_u32(option: &str, value: u64) -> Diagnostic {
    let message = format!("NET_FETCH() option '{option}' exceeds u32: {value}");
    Diagnostic::new(
        "net.func.fetch_option_exceeds_u32",
        message,
        Diagnostic::make_fields(&[(FIELD_OPTION, option), (FIELD_VALUE, &value.to_string())]),
    )
}

// ---------------------------------------------------------------------------
// Remote (`net.remote.*`, `remote/`)
// ---------------------------------------------------------------------------

/// Empty remote transport command. Parameterless: the wording is
/// fixed.
pub fn remote_transport_command_empty() -> Diagnostic {
    Diagnostic::new(
        "net.remote.transport_command_empty",
        format!("{KEYWORD_REMOTE} transport command is empty"),
        Diagnostic::make_fields(&[]),
    )
}

/// Cancelled remote block. Parameterless: the wording is fixed.
pub fn remote_block_cancelled() -> Diagnostic {
    Diagnostic::new(
        "net.remote.block_cancelled",
        format!("{KEYWORD_REMOTE} block cancelled"),
        Diagnostic::make_fields(&[]),
    )
}

/// Unknown response tag. Takes the tag byte; rendering lives here.
pub fn remote_unknown_response_tag(tag: u8) -> Diagnostic {
    let message = format!("{KEYWORD_REMOTE} target sent unknown response tag {tag}");
    Diagnostic::new(
        "net.remote.unknown_response_tag",
        message,
        Diagnostic::make_fields(&[(FIELD_TAG, &tag.to_string())]),
    )
}

/// Result bytes with no staged tarball. Parameterless: the wording is
/// fixed.
pub fn remote_result_no_staged_tarball() -> Diagnostic {
    Diagnostic::new(
        "net.remote.result_no_staged_tarball",
        format!("{KEYWORD_REMOTE} result arrived with no staged tarball"),
        Diagnostic::make_fields(&[]),
    )
}

/// Handshake rejection. Takes the target plus the optional guest
/// reason (defaulting inside); rendering lives here.
pub fn remote_handshake_rejected(target: &str, reason: Option<&str>) -> Diagnostic {
    let reason_text = reason.unwrap_or("no reason given");
    let message =
        format!("{KEYWORD_REMOTE} target '{target}' rejected the handshake: {reason_text}");
    Diagnostic::new(
        "net.remote.handshake_rejected",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target), (FIELD_REASON, reason_text)]),
    )
}

/// Protocol digest mismatch. Takes the target plus both digests;
/// rendering lives here.
pub fn remote_proto_digest_mismatch(target: &str, host: &str, guest: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_REMOTE} target '{target}' protocol digest mismatch (host {host} guest {guest}): rebuild and reinstall matching oxdock on the guest"
    );
    Diagnostic::new(
        "net.remote.proto_digest_mismatch",
        message,
        Diagnostic::make_fields(&[
            (FIELD_TARGET, target),
            (FIELD_VALUE, host),
            (FIELD_GUEST, guest),
        ]),
    )
}

/// Language surface mismatch. Takes the target plus both hashes;
/// rendering lives here.
pub fn remote_language_mismatch(target: &str, host: &str, guest: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_REMOTE} target '{target}' language surface mismatch (host {host} guest {guest}): rebuild and reinstall matching oxdock on the guest"
    );
    Diagnostic::new(
        "net.remote.language_mismatch",
        message,
        Diagnostic::make_fields(&[
            (FIELD_TARGET, target),
            (FIELD_VALUE, host),
            (FIELD_GUEST, guest),
        ]),
    )
}

/// Response stream failure. Takes the wrapped failure; the chain
/// renders inline.
pub fn remote_response_failed(err: &anyhow::Error) -> Diagnostic {
    let err_text = format!("{err:#}");
    let message = format!("{KEYWORD_REMOTE} response failed: {err_text}");
    Diagnostic::new(
        "net.remote.response_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_ERR, err_text.as_str())]),
    )
}

/// Exec with no result frame. Takes the target; rendering lives here.
pub fn remote_no_result_frame(target: &str) -> Diagnostic {
    let message = format!("{KEYWORD_REMOTE} target '{target}' sent no result frame");
    Diagnostic::new(
        "net.remote.no_result_frame",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target)]),
    )
}

/// Guest-side failure. Takes the target plus the optional guest
/// reason (defaulting inside); rendering lives here.
pub fn remote_target_failed(target: &str, reason: Option<&str>) -> Diagnostic {
    let reason_text = reason.unwrap_or("no reason given");
    let message = format!("{KEYWORD_REMOTE} target '{target}' failed: {reason_text}");
    Diagnostic::new(
        "net.remote.target_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target), (FIELD_REASON, reason_text)]),
    )
}

/// Script integrity mismatch. Takes the target; rendering lives here.
pub fn remote_script_sha_mismatch(target: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_REMOTE} target '{target}' executed different script bytes than shipped (transport corruption)"
    );
    Diagnostic::new(
        "net.remote.script_sha_mismatch",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target)]),
    )
}

/// Stdout integrity mismatch. Takes the target; rendering lives here.
pub fn remote_stdout_sha_mismatch(target: &str) -> Diagnostic {
    let message =
        format!("{KEYWORD_REMOTE} target '{target}' stdout hash mismatch (transport corruption)");
    Diagnostic::new(
        "net.remote.stdout_sha_mismatch",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target)]),
    )
}

/// Exec with no result tarball. Takes the target; rendering lives
/// here.
pub fn remote_no_result_tarball(target: &str) -> Diagnostic {
    let message = format!("{KEYWORD_REMOTE} target '{target}' sent no result tarball");
    Diagnostic::new(
        "net.remote.no_result_tarball",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target)]),
    )
}

/// Push tarball integrity mismatch. Takes the target; rendering lives
/// here.
pub fn remote_push_tarball_sha_mismatch(target: &str) -> Diagnostic {
    let message = format!(
        "{KEYWORD_REMOTE} target '{target}' push tarball sha256 mismatch (transport corruption)"
    );
    Diagnostic::new(
        "net.remote.push_tarball_sha_mismatch",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target)]),
    )
}

/// Guest path outside the declared push set. Takes the target plus
/// the path; `Debug` quoting lives here. Shared by dir, file, and
/// symlink entries: one key, one mode.
pub fn remote_undeclared_path(target: &str, path: &str) -> Diagnostic {
    let message = format!("{KEYWORD_REMOTE} target '{target}' returned undeclared path {path:?}");
    Diagnostic::new(
        "net.remote.undeclared_path",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target), (FIELD_PATH, path)]),
    )
}

/// Malformed `--remote` flag. Takes the raw flag; `Debug` quoting
/// lives here.
pub fn remote_arg_malformed(raw: &str) -> Diagnostic {
    let message = format!(
        "invalid --remote {raw:?}: expected <target>=\"<command>\", e.g. --remote prod=\"ssh user@host oxdock\""
    );
    Diagnostic::new(
        "net.remote.arg_malformed",
        message,
        Diagnostic::make_fields(&[(FIELD_RAW, raw)]),
    )
}

/// Invalid remote target name. Takes the target; `Debug` quoting
/// lives here.
pub fn remote_target_invalid(target: &str) -> Diagnostic {
    let message = format!(
        "invalid --remote target {target:?}: start alphanumeric, then alphanumeric, `_`, or `-`, max 64 chars, never SNAPSHOT, LOCAL, CACHE, SYSTEM, or {KEYWORD_REMOTE}"
    );
    Diagnostic::new(
        "net.remote.target_invalid",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target)]),
    )
}

/// Empty remote command. Takes the target; rendering lives here.
pub fn remote_command_empty(target: &str) -> Diagnostic {
    let message = format!("invalid --remote {target:?}: command string is empty");
    Diagnostic::new(
        "net.remote.command_empty",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target)]),
    )
}

/// Untokenizable remote command at flag parse. Takes the target plus
/// the concrete tokenizer failure; rendering lives here.
pub fn remote_tokenize_failed(target: &str, err: &shell_words::ParseError) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("invalid --remote {target:?}: cannot tokenize command: {err_text}");
    Diagnostic::new(
        "net.remote.tokenize_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target), (FIELD_ERR, err_text.as_str())]),
    )
}

/// Duplicate `--remote` target. Takes the target; `Debug` quoting
/// lives here.
pub fn inventory_duplicate_target(target: &str) -> Diagnostic {
    let message = format!("duplicate --remote target {target:?}");
    Diagnostic::new(
        "net.remote.inventory_duplicate_target",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target)]),
    )
}

/// Untokenizable remote command at inventory build. Takes the target
/// plus the concrete tokenizer failure; rendering lives here.
pub fn inventory_tokenize_failed(target: &str, err: &shell_words::ParseError) -> Diagnostic {
    let err_text = err.to_string();
    let message = format!("invalid --remote {target:?}: cannot tokenize command: {err_text}");
    Diagnostic::new(
        "net.remote.inventory_tokenize_failed",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target), (FIELD_ERR, err_text.as_str())]),
    )
}

/// Tokenized but empty remote command. Takes the target; rendering
/// lives here.
pub fn inventory_no_executable(target: &str) -> Diagnostic {
    let message = format!("invalid --remote {target:?}: command tokenizes to no executable");
    Diagnostic::new(
        "net.remote.inventory_no_executable",
        message,
        Diagnostic::make_fields(&[(FIELD_TARGET, target)]),
    )
}

/// Script target with no inventory binding. Takes the target plus the
/// declared names; the list renders here.
pub fn inventory_unknown_target(target: &str, declared: &[String]) -> Diagnostic {
    let declared_text = declared
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!(
        "unknown remote target {target:?} (declared: [{declared_text}]; add --remote {target}=\"<command>\")"
    );
    Diagnostic::new(
        "net.remote.inventory_unknown_target",
        message,
        Diagnostic::make_fields(&[
            (FIELD_TARGET, target),
            (FIELD_DECLARED, declared_text.as_str()),
        ]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_err(msg: &str) -> std::io::Error {
        std::io::Error::other(msg.to_string())
    }

    fn anyhow_err(msg: &str) -> anyhow::Error {
        anyhow::Error::msg(msg.to_string())
    }

    fn tokenize_err() -> shell_words::ParseError {
        shell_words::split("\"unterminated").unwrap_err()
    }

    fn named_key(name: &str) -> EndpointKey {
        EndpointKey::tcp(VirtualEndpoint::Name(name.to_string()))
    }

    #[test]
    fn each_failure_has_a_distinct_key() {
        let err = io_err("boom");
        let chain = anyhow_err("boom");
        let shell_err = tokenize_err();
        let tcp_key = named_key("web");
        let keys = [
            pump_worker_panicked("NET_PUMP", "input worker").key(),
            pump_socket_shutdown_failed("NET_PUMP", &err).key(),
            pump_stdout_lock_poisoned("NET_PUMP").key(),
            pump_socket_read_failed("NET_PUMP", &err).key(),
            pump_interrupted_by_cancellation("NET_PUMP").key(),
            pump_memory_pipe_lock_poisoned("NET_PUMP").key(),
            pump_memory_pipe_read_failed("NET_PUMP", &err).key(),
            pump_stdin_pipe_read_failed("NET_PUMP", &err).key(),
            pump_socket_write_failed("NET_PUMP", &err).key(),
            pump_stdout_pipe_write_failed("NET_PUMP", &err).key(),
            pump_stdout_pipe_flush_failed("NET_PUMP", &err).key(),
            pump_borrow_input_failed(pump_stdout_lock_poisoned("NET_PUMP")).key(),
            pump_borrow_output_failed(pump_stdout_lock_poisoned("NET_PUMP")).key(),
            pump_socket_clone_failed("NET_PUMP", &err).key(),
            pump_memory_pipe_write_failed("NET_PUMP", &err).key(),
            pump_memory_pipe_flush_failed("NET_PUMP", &err).key(),
            fetch_url_missing_scheme("example.com/x").key(),
            fetch_url_not_https("example.com/x").key(),
            fetch_refuses_cleartext_http("example.com").key(),
            fetch_unsupported_scheme("ftp").key(),
            fetch_too_many_redirects("https://a/x").key(),
            fetch_redirect_missing_location("https://a/x").key(),
            fetch_http_status("https://a/x", 500).key(),
            fetch_request_failed("https://a/x", &chain).key(),
            fetch_redirect_location_not_ascii("https://a/x", &chain).key(),
            no_cli_mapping("NET_LISTEN", &VirtualEndpoint::Name("web".to_string())).key(),
            duplicate_mapping(&tcp_key).key(),
            endpoint_not_bound(&tcp_key).key(),
            virtual_channel_no_physical(
                "NET_PORT",
                &VirtualEndpoint::Name("mem".to_string()),
                "port",
            )
            .key(),
            service_not_mapped("NET_PORT", &VirtualEndpoint::Name("nope".to_string())).key(),
            never_bound("NET_LISTEN", &tcp_key).key(),
            ambiguous_endpoint("dns").key(),
            unknown_service("NET_LISTEN", &tcp_key).key(),
            already_claimed("NET_LISTEN", &tcp_key).key(),
            dial_only_mapping("NET_LISTEN", &tcp_key).key(),
            connect_unknown_service(&tcp_key).key(),
            connect_queue_full(&tcp_key).key(),
            service_names_must_be_literal("NET_LISTEN").key(),
            service_offline("NET_CONNECT", &tcp_key).key(),
            port_not_numeric().key(),
            unknown_protocol_qualifier("sctp/dns").key(),
            endpoint_ref_invalid("NET_PORT", "tcp/dns", port_not_numeric()).key(),
            endpoint_empty("NET_LISTEN", "").key(),
            endpoint_host_port("NET_LISTEN", "a:1").key(),
            endpoint_bad_chars("NET_LISTEN", "has space").key(),
            endpoint_port_parse("NET_LISTEN", "99999", port_not_numeric()).key(),
            endpoint_port_zero("NET_LISTEN", "0").key(),
            endpoint_bad_shape("NET_LISTEN", "!!!").key(),
            endpoint_reserved("NET_LISTEN", "localhost").key(),
            listen_protocol_qualified("tcp/web").key(),
            accept_requires_async().key(),
            accept_interrupted().key(),
            accept_listener_closed().key(),
            accept_failed(&err).key(),
            connect_requires_async().key(),
            connect_offline().key(),
            fetch_option_negative("max_body_bytes", -1).key(),
            fetch_option_exceeds_u32("max_redirects", u64::from(u32::MAX) + 1).key(),
            remote_transport_command_empty().key(),
            remote_block_cancelled().key(),
            remote_unknown_response_tag(99).key(),
            remote_result_no_staged_tarball().key(),
            remote_handshake_rejected("prod", None).key(),
            remote_proto_digest_mismatch("prod", "h", "g").key(),
            remote_language_mismatch("prod", "h", "g").key(),
            remote_response_failed(&chain).key(),
            remote_no_result_frame("prod").key(),
            remote_target_failed("prod", None).key(),
            remote_script_sha_mismatch("prod").key(),
            remote_stdout_sha_mismatch("prod").key(),
            remote_no_result_tarball("prod").key(),
            remote_push_tarball_sha_mismatch("prod").key(),
            remote_undeclared_path("prod", "evil.txt").key(),
            remote_arg_malformed("no-equals-sign").key(),
            remote_target_invalid("LOCAL").key(),
            remote_command_empty("prod").key(),
            remote_tokenize_failed("prod", &shell_err).key(),
            inventory_duplicate_target("a").key(),
            inventory_tokenize_failed("a", &shell_err).key(),
            inventory_no_executable("a").key(),
            inventory_unknown_target("missing", &["prod".to_string()]).key(),
        ];
        let mut seen = std::collections::HashSet::new();
        for k in keys {
            assert!(seen.insert(k), "duplicate key {k}");
        }
    }

    #[test]
    fn shared_modes_keep_one_key() {
        assert_eq!(
            virtual_channel_no_physical(
                "NET_PORT",
                &VirtualEndpoint::Name("m".to_string()),
                "port"
            )
            .key(),
            virtual_channel_no_physical(
                "NET_ADDR",
                &VirtualEndpoint::Name("m".to_string()),
                "address"
            )
            .key()
        );
        assert_eq!(
            never_bound("NET_LISTEN", &named_key("a")).key(),
            never_bound("NET_CONNECT", &named_key("a")).key()
        );
        assert_eq!(
            service_offline("NET_CONNECT", &named_key("a")).key(),
            service_offline("SSH_SERVE", &named_key("a")).key()
        );
        assert_eq!(
            fetch_option_negative("max_body_bytes", -1).key(),
            fetch_option_negative("max_redirects", -1).key()
        );
        assert_eq!(
            pump_stdout_lock_poisoned("NET_ACCEPT").key(),
            pump_stdout_lock_poisoned("NET_CONNECT").key()
        );
        assert_eq!(
            pump_interrupted_by_cancellation("NET_ACCEPT").key(),
            pump_interrupted_by_cancellation("NET_CONNECT").key()
        );
    }

    #[test]
    fn messages_match_canonical_wording() {
        let err = io_err("boom");
        let chain = anyhow_err("boom");
        let shell_err = tokenize_err();
        assert_eq!(
            pump_worker_panicked("NET_ACCEPT", "input worker").message(),
            "NET_ACCEPT input worker panicked"
        );
        assert_eq!(
            pump_socket_shutdown_failed("NET_ACCEPT", &err).message(),
            "NET_ACCEPT socket shutdown failed: boom"
        );
        assert_eq!(
            pump_stdout_lock_poisoned("NET_ACCEPT").message(),
            "NET_ACCEPT stdout lock poisoned"
        );
        assert_eq!(
            pump_socket_read_failed("NET_ACCEPT", &err).message(),
            "NET_ACCEPT socket read failed: boom"
        );
        assert_eq!(
            pump_interrupted_by_cancellation("NET_ACCEPT").message(),
            "NET_ACCEPT interrupted by cancellation"
        );
        assert_eq!(
            pump_memory_pipe_lock_poisoned("NET_ACCEPT").message(),
            "NET_ACCEPT memory pipe lock poisoned"
        );
        assert_eq!(
            pump_memory_pipe_read_failed("NET_ACCEPT", &err).message(),
            "NET_ACCEPT memory pipe read failed: boom"
        );
        assert_eq!(
            pump_stdin_pipe_read_failed("NET_ACCEPT", &err).message(),
            "NET_ACCEPT stdin pipe read failed: boom"
        );
        assert_eq!(
            pump_socket_write_failed("NET_ACCEPT", &err).message(),
            "NET_ACCEPT socket write failed: boom"
        );
        assert_eq!(
            pump_stdout_pipe_write_failed("NET_ACCEPT", &err).message(),
            "NET_ACCEPT stdout pipe write failed: boom"
        );
        assert_eq!(
            pump_stdout_pipe_flush_failed("NET_ACCEPT", &err).message(),
            "NET_ACCEPT stdout pipe flush failed: boom"
        );
        assert_eq!(
            pump_borrow_input_failed(pump_stdout_lock_poisoned("NET_ACCEPT")).message(),
            "NET pump cannot borrow the input pipe: NET_ACCEPT stdout lock poisoned"
        );
        assert_eq!(
            pump_borrow_output_failed(pump_stdout_lock_poisoned("NET_ACCEPT")).message(),
            "NET pump cannot borrow the output pipe: NET_ACCEPT stdout lock poisoned"
        );
        assert_eq!(
            pump_socket_clone_failed("NET_ACCEPT", &err).message(),
            "NET_ACCEPT failed to clone socket: boom"
        );
        assert_eq!(
            pump_memory_pipe_write_failed("NET_ACCEPT", &err).message(),
            "NET_ACCEPT memory pipe write failed: boom"
        );
        assert_eq!(
            pump_memory_pipe_flush_failed("NET_ACCEPT", &err).message(),
            "NET_ACCEPT memory pipe flush failed: boom"
        );
        assert_eq!(
            fetch_url_missing_scheme("example.com/x").message(),
            "NET_FETCH() needs an https URL, got \"example.com/x\" (missing '://')"
        );
        assert_eq!(
            fetch_url_not_https("https://").message(),
            "NET_FETCH() needs an https URL, got \"https://\""
        );
        assert_eq!(
            fetch_refuses_cleartext_http("example.com").message(),
            "NET_FETCH() refuses cleartext http to non-loopback host \"example.com\": use https"
        );
        assert_eq!(
            fetch_unsupported_scheme("ftp").message(),
            "NET_FETCH() supports https URLs (plus loopback http for tests), got scheme \"ftp\""
        );
        assert_eq!(
            fetch_too_many_redirects("https://a/x").message(),
            "NET_FETCH(\"https://a/x\") failed: too many redirects"
        );
        assert_eq!(
            fetch_redirect_missing_location("https://a/x").message(),
            "NET_FETCH(\"https://a/x\") redirect missing Location header"
        );
        assert_eq!(
            fetch_http_status("https://a/x", 500).message(),
            "NET_FETCH(\"https://a/x\") failed: http status: 500"
        );
        assert_eq!(
            fetch_request_failed("https://a/x", &chain).message(),
            "NET_FETCH(\"https://a/x\") failed: boom"
        );
        assert_eq!(
            fetch_redirect_location_not_ascii("https://a/x", &chain).message(),
            "NET_FETCH(\"https://a/x\") redirect Location is not visible ASCII: boom"
        );
        assert_eq!(
            no_cli_mapping("NET_LISTEN", &VirtualEndpoint::Name("web".to_string())).message(),
            "NET_LISTEN service 'web' has no CLI port mapping; pass -p <host_port>:web at invocation"
        );
        let key = named_key("web");
        assert_eq!(
            duplicate_mapping(&key).message(),
            "duplicate endpoint mapping for 'web'"
        );
        assert_eq!(
            endpoint_not_bound(&key).message(),
            "virtual endpoint 'web' is not bound"
        );
        assert_eq!(
            virtual_channel_no_physical(
                "NET_PORT",
                &VirtualEndpoint::Name("mem".to_string()),
                "port"
            )
            .message(),
            "NET_PORT service 'mem' is running as an in-memory virtual channel with no physical port"
        );
        assert_eq!(
            service_not_mapped("NET_PORT", &VirtualEndpoint::Name("nope".to_string())).message(),
            "NET_PORT service 'nope' is not mapped by the runner (map it with -p/--listen)"
        );
        assert_eq!(
            never_bound("NET_LISTEN", &key).message(),
            "NET_LISTEN: 'web' was never bound (the runner must call bind_all)"
        );
        assert_eq!(
            ambiguous_endpoint("dns").message(),
            "ambiguous virtual endpoint 'dns': both TCP and UDP bindings exist; qualify as 'tcp/dns' or 'udp/dns'"
        );
        assert_eq!(
            unknown_service("NET_LISTEN", &key).message(),
            "NET_LISTEN: unknown service 'web' (map it with -p/--listen)"
        );
        assert_eq!(
            already_claimed("NET_LISTEN", &key).message(),
            "NET_LISTEN: 'web' is already claimed"
        );
        assert_eq!(
            dial_only_mapping("NET_LISTEN", &key).message(),
            "NET_LISTEN: 'web' is a dial-only mapping (the runner owns the socket; LISTEN elsewhere)"
        );
        assert_eq!(
            connect_unknown_service(&key).message(),
            "NET_CONNECT: unknown service 'web' (map it with -p/--listen)"
        );
        assert_eq!(
            connect_queue_full(&key).message(),
            "NET_CONNECT: memory connection queue for 'web' is full"
        );
        assert_eq!(
            service_names_must_be_literal("NET_LISTEN").message(),
            "NET_LISTEN service names must be string literals so the runner can verify bindings before execution"
        );
        assert_eq!(
            service_offline("NET_CONNECT", &key).message(),
            "NET_CONNECT: 'web' is offline"
        );
        assert_eq!(port_not_numeric().message(), "port must be numeric 0-65535");
        assert_eq!(
            unknown_protocol_qualifier("sctp/dns").message(),
            "unknown protocol qualifier in \"sctp/dns\" (expected 'tcp/...', 'udp/...', '.../tcp', or '.../udp')"
        );
        assert_eq!(
            endpoint_ref_invalid("NET_PORT", "tcp/dns", port_not_numeric()).message(),
            "NET_PORT invalid endpoint \"tcp/dns\": port must be numeric 0-65535"
        );
        assert_eq!(
            endpoint_empty("NET_LISTEN", "").message(),
            "NET_LISTEN invalid endpoint \"\": expected a port 1-65535 or a service name"
        );
        assert_eq!(
            endpoint_host_port("NET_LISTEN", "a:1").message(),
            "NET_LISTEN invalid endpoint \"a:1\": scripts declare logical endpoints, not host:port binds (the host runner maps them with --listen/-p)"
        );
        assert_eq!(
            endpoint_bad_chars("NET_LISTEN", "has space").message(),
            "NET_LISTEN invalid endpoint \"has space\": service names use [A-Za-z0-9_-] only"
        );
        assert_eq!(
            endpoint_port_parse("NET_LISTEN", "99999", port_not_numeric()).message(),
            "NET_LISTEN invalid endpoint \"99999\": port must be numeric 0-65535"
        );
        assert_eq!(
            endpoint_port_zero("NET_LISTEN", "0").message(),
            "NET_LISTEN invalid endpoint \"0\": port 0 is reserved for the CLI outer mapping (use -p 0:<endpoint>); scripts declare fixed ports 1-65535 or service names"
        );
        assert_eq!(
            endpoint_bad_shape("NET_LISTEN", "!!!").message(),
            "NET_LISTEN invalid endpoint \"!!!\": service names are 1-64 chars of [A-Za-z0-9_-] with at least one letter, '-' or '_'"
        );
        assert_eq!(
            endpoint_reserved("NET_LISTEN", "localhost").message(),
            "NET_LISTEN invalid endpoint \"localhost\": \"localhost\" is a reserved host name, not a service name"
        );
        assert_eq!(
            listen_protocol_qualified("tcp/web").message(),
            "NET_LISTEN() invalid endpoint \"tcp/web\": protocol-qualified endpoints cannot be claimed (UDP listen is not supported); observe -p mappings with NET_PORT/NET_ADDR instead"
        );
        assert_eq!(
            accept_requires_async().message(),
            "NET_ACCEPT requires ASYNC: wrap it as LET $t: HANDLE<T> = ASYNC { NET_ACCEPT($l, $in, $out, {}) }"
        );
        assert_eq!(
            accept_interrupted().message(),
            "NET_ACCEPT interrupted by cancellation"
        );
        assert_eq!(
            accept_listener_closed().message(),
            "NET_ACCEPT: listener is closed"
        );
        assert_eq!(
            accept_failed(&err).message(),
            "NET_ACCEPT accept failed: boom"
        );
        assert_eq!(
            connect_requires_async().message(),
            "NET_CONNECT requires ASYNC: wrap it as LET $t: HANDLE<T> = ASYNC { NET_CONNECT($target, $in, $out, {}) }"
        );
        assert_eq!(
            connect_offline().message(),
            "NET_CONNECT failed: engine running in --offline mode"
        );
        assert_eq!(
            fetch_option_negative("max_body_bytes", -1).message(),
            "NET_FETCH option 'max_body_bytes' must be non-negative, got -1"
        );
        assert_eq!(
            fetch_option_exceeds_u32("max_redirects", 4294967296).message(),
            "NET_FETCH() option 'max_redirects' exceeds u32: 4294967296"
        );
        assert_eq!(
            remote_transport_command_empty().message(),
            "REMOTE transport command is empty"
        );
        assert_eq!(remote_block_cancelled().message(), "REMOTE block cancelled");
        assert_eq!(
            remote_unknown_response_tag(99).message(),
            "REMOTE target sent unknown response tag 99"
        );
        assert_eq!(
            remote_result_no_staged_tarball().message(),
            "REMOTE result arrived with no staged tarball"
        );
        assert_eq!(
            remote_handshake_rejected("prod", None).message(),
            "REMOTE target 'prod' rejected the handshake: no reason given"
        );
        assert_eq!(
            remote_handshake_rejected("prod", Some("stale")).message(),
            "REMOTE target 'prod' rejected the handshake: stale"
        );
        assert_eq!(
            remote_proto_digest_mismatch("prod", "h", "g").message(),
            "REMOTE target 'prod' protocol digest mismatch (host h guest g): rebuild and reinstall matching oxdock on the guest"
        );
        assert_eq!(
            remote_language_mismatch("prod", "h", "g").message(),
            "REMOTE target 'prod' language surface mismatch (host h guest g): rebuild and reinstall matching oxdock on the guest"
        );
        assert_eq!(
            remote_response_failed(&chain).message(),
            "REMOTE response failed: boom"
        );
        assert_eq!(
            remote_no_result_frame("prod").message(),
            "REMOTE target 'prod' sent no result frame"
        );
        assert_eq!(
            remote_target_failed("prod", None).message(),
            "REMOTE target 'prod' failed: no reason given"
        );
        assert_eq!(
            remote_script_sha_mismatch("prod").message(),
            "REMOTE target 'prod' executed different script bytes than shipped (transport corruption)"
        );
        assert_eq!(
            remote_stdout_sha_mismatch("prod").message(),
            "REMOTE target 'prod' stdout hash mismatch (transport corruption)"
        );
        assert_eq!(
            remote_no_result_tarball("prod").message(),
            "REMOTE target 'prod' sent no result tarball"
        );
        assert_eq!(
            remote_push_tarball_sha_mismatch("prod").message(),
            "REMOTE target 'prod' push tarball sha256 mismatch (transport corruption)"
        );
        assert_eq!(
            remote_undeclared_path("prod", "evil.txt").message(),
            "REMOTE target 'prod' returned undeclared path \"evil.txt\""
        );
        assert_eq!(
            remote_arg_malformed("no-equals-sign").message(),
            "invalid --remote \"no-equals-sign\": expected <target>=\"<command>\", e.g. --remote prod=\"ssh user@host oxdock\""
        );
        assert_eq!(
            remote_target_invalid("LOCAL").message(),
            "invalid --remote target \"LOCAL\": start alphanumeric, then alphanumeric, `_`, or `-`, max 64 chars, never SNAPSHOT, LOCAL, CACHE, SYSTEM, or REMOTE"
        );
        assert_eq!(
            remote_command_empty("prod").message(),
            "invalid --remote \"prod\": command string is empty"
        );
        assert!(
            remote_tokenize_failed("prod", &shell_err)
                .message()
                .starts_with("invalid --remote \"prod\": cannot tokenize command: ")
        );
        assert_eq!(
            inventory_duplicate_target("a").message(),
            "duplicate --remote target \"a\""
        );
        assert!(
            inventory_tokenize_failed("a", &shell_err)
                .message()
                .starts_with("invalid --remote \"a\": cannot tokenize command: ")
        );
        assert_eq!(
            inventory_no_executable("a").message(),
            "invalid --remote \"a\": command tokenizes to no executable"
        );
        assert_eq!(
            inventory_unknown_target("missing", &["prod".to_string()]).message(),
            "unknown remote target \"missing\" (declared: [\"prod\"]; add --remote missing=\"<command>\")"
        );
    }
}
