//! Contract stability for the remote execution protocol: golden JSON
//! vectors, method-ID determinism, chunk caps, and digest shape. These run
//! under Miri (no threads, no sockets, no tempdirs).

use oxdock_remote_proto::{CHUNK_BYTES, PROTO_DIGEST, methods, types::*};

#[test]
fn method_ids_are_stable_distinct_and_nonzero() {
    assert_ne!(methods::HANDSHAKE, 0);
    assert_ne!(methods::EXEC, 0);
    assert_ne!(methods::CLOSE, 0);
    assert_ne!(methods::HANDSHAKE, methods::EXEC);
    assert_ne!(methods::HANDSHAKE, methods::CLOSE);
    assert_ne!(methods::EXEC, methods::CLOSE);
    // Same input, same output: the macro is a pure compile-time hash.
    assert_eq!(
        methods::HANDSHAKE,
        muxio_rpc_service::rpc_method_id!("oxdock.remote.handshake")
    );
    assert_eq!(
        methods::EXEC,
        muxio_rpc_service::rpc_method_id!("oxdock.remote.exec")
    );
}

#[test]
fn proto_digest_is_hex_sha256() {
    assert_eq!(PROTO_DIGEST.len(), 64);
    assert!(PROTO_DIGEST.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn chunk_cap_is_64kib() {
    assert_eq!(CHUNK_BYTES, 64 * 1024);
}

#[test]
fn handshake_hello_golden_json() {
    let hello = HandshakeHello {
        proto_digest: "abc".to_string(),
        oxdock_version: "0.19.0-alpha".to_string(),
        modules: vec!["NET".to_string()],
        module_hash: "def".to_string(),
    };
    let text = serde_json::to_string(&hello).unwrap();
    assert_eq!(
        text,
        r#"{"proto_digest":"abc","oxdock_version":"0.19.0-alpha","modules":["NET"],"module_hash":"def"}"#
    );
    let back: HandshakeHello = serde_json::from_str(&text).unwrap();
    assert_eq!(back, hello);
}

#[test]
fn handshake_verdict_golden_json() {
    let verdict = HandshakeVerdict {
        accepted: false,
        proto_digest: "abc".to_string(),
        oxdock_version: "0.19.0-alpha".to_string(),
        modules: vec![],
        module_hash: "def".to_string(),
        error: Some("digest mismatch".to_string()),
    };
    let text = serde_json::to_string(&verdict).unwrap();
    let back: HandshakeVerdict = serde_json::from_str(&text).unwrap();
    assert_eq!(back, verdict);
}

#[test]
fn exec_result_golden_json() {
    let result = ExecResult {
        ok: true,
        error: None,
        result_tar_sha256: "00".repeat(32),
        script_sha256: "11".repeat(32),
        stdout_sha256: "22".repeat(32),
    };
    let text = serde_json::to_string(&result).unwrap();
    let back: ExecResult = serde_json::from_str(&text).unwrap();
    assert_eq!(back, result);
}

#[test]
fn malformed_control_frames_bail() {
    let bad: Result<ExecHeader, _> = serde_json::from_str(r#"{"target": 7}"#);
    assert!(bad.is_err());
    let truncated: Result<HandshakeHello, _> = serde_json::from_str(r#"{"proto_digest":"x""#);
    assert!(truncated.is_err());
}
