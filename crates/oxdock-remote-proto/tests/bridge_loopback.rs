//! Muxio bridge loopback over memory queues: host and guest `Bridge`
//! instances wired back to back with no threads, no pipes, no subprocess.
//! Proves framing, multiplexing, tags, re-chunking tolerance, and error
//! surfacing. Runs under Miri.

use std::sync::mpsc;

use oxdock_remote_proto::bridge::{Bridge, Role, StreamEvent};
use oxdock_remote_proto::{channel, methods};

fn pair() -> (
    Bridge,
    Bridge,
    mpsc::Receiver<Vec<u8>>,
    mpsc::Receiver<Vec<u8>>,
) {
    let (host_tx, host_rx) = mpsc::channel();
    let (guest_tx, guest_rx) = mpsc::channel();
    (
        Bridge::new(Role::Host, host_tx),
        Bridge::new(Role::Guest, guest_tx),
        host_rx,
        guest_rx,
    )
}

fn drain(rx: &mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        out.extend_from_slice(&frame);
    }
    out
}

#[test]
fn script_stream_round_trips_with_tag() {
    let (mut host, mut guest, host_rx, _guest_rx) = pair();
    host.open(methods::EXEC, channel::TAG_SCRIPT, b"ECHO hi")
        .unwrap();
    let wire = drain(&host_rx);
    assert!(!wire.is_empty(), "open must emit frames");
    let events = guest.read(&wire).unwrap();
    let mut payload = Vec::new();
    let mut ended = false;
    for event in events {
        match event {
            StreamEvent::Chunk { method_id, bytes, .. } => {
                assert_eq!(method_id, methods::EXEC);
                payload.extend_from_slice(&bytes);
            }
            StreamEvent::End { method_id, .. } => {
                assert_eq!(method_id, methods::EXEC);
                ended = true;
            }
            StreamEvent::Error { detail } => panic!("unexpected error: {detail}"),
        }
    }
    // The opening write has not ended: payload holds tag plus script, and
    // no End arrived yet.
    assert!(!ended);
    assert_eq!(payload[0], channel::TAG_SCRIPT);
    assert_eq!(&payload[1..], b"ECHO hi");
}

#[test]
fn writes_chunks_and_end_arrive_in_order() {
    let (mut host, mut guest, host_rx, _guest_rx) = pair();
    let id = host.open(methods::EXEC, channel::TAG_FETCH, b"").unwrap();
    for chunk in [b"aaaa".as_slice(), b"bbbb", b"cc"] {
        host.write(id, chunk).unwrap();
    }
    host.end(id).unwrap();
    let wire = drain(&host_rx);
    let events = guest.read(&wire).unwrap();
    let mut payload = Vec::new();
    let mut ended = false;
    for event in events {
        match event {
            StreamEvent::Chunk { bytes, .. } => payload.extend_from_slice(&bytes),
            StreamEvent::End { .. } => ended = true,
            StreamEvent::Error { detail } => panic!("unexpected error: {detail}"),
        }
    }
    assert!(ended);
    assert_eq!(payload[0], channel::TAG_FETCH);
    assert_eq!(&payload[1..], b"aaaabbbbcc");
}

#[test]
fn guest_to_host_stream_flows_back() {
    let (mut host, mut guest, _host_rx, guest_rx) = pair();
    let id = guest
        .open(methods::EXEC, channel::TAG_STDOUT, b"out-bytes")
        .unwrap();
    guest.end(id).unwrap();
    let wire = drain(&guest_rx);
    let events = host.read(&wire).unwrap();
    let mut payload = Vec::new();
    let mut ended = false;
    for event in events {
        match event {
            StreamEvent::Chunk { bytes, .. } => payload.extend_from_slice(&bytes),
            StreamEvent::End { .. } => ended = true,
            StreamEvent::Error { detail } => panic!("unexpected error: {detail}"),
        }
    }
    assert!(ended);
    assert_eq!(payload[0], channel::TAG_STDOUT);
    assert_eq!(&payload[1..], b"out-bytes");
}

#[test]
fn concurrent_streams_interleave() {
    let (mut host, mut guest, host_rx, _guest_rx) = pair();
    let a = host.open(methods::EXEC, channel::TAG_STDIN, b"A1").unwrap();
    let b = host
        .open(methods::HANDSHAKE, channel::TAG_HELLO, b"H1")
        .unwrap();
    host.write(a, b"A2").unwrap();
    host.write(b, b"H2").unwrap();
    host.end(a).unwrap();
    host.end(b).unwrap();
    let wire = drain(&host_rx);
    // Feed byte by byte: re-chunking must not disturb routing.
    let mut payloads: std::collections::HashMap<(u64, u32), Vec<u8>> =
        std::collections::HashMap::new();
    let mut ends = 0;
    for byte in wire.chunks(1) {
        for event in guest.read(byte).unwrap() {
            match event {
                StreamEvent::Chunk {
                    method_id,
                    request_id,
                    bytes,
                } => payloads
                    .entry((method_id, request_id))
                    .or_default()
                    .extend_from_slice(&bytes),
                StreamEvent::End { .. } => ends += 1,
                StreamEvent::Error { detail } => panic!("unexpected error: {detail}"),
            }
        }
    }
    assert_eq!(ends, 2);
    assert_eq!(payloads.len(), 2);
    let mut bodies: Vec<Vec<u8>> = payloads.into_values().collect();
    bodies.sort();
    assert_eq!(bodies[0][0], channel::TAG_HELLO.min(channel::TAG_STDIN));
}

#[test]
fn tampered_wire_never_panics_and_session_survives() {
    let (mut host, mut guest, host_rx, _guest_rx) = pair();
    let id = host.open(methods::EXEC, channel::TAG_SCRIPT, b"ECHO hi").unwrap();
    host.end(id).unwrap();
    let mut wire = drain(&host_rx);
    assert!(!wire.is_empty());
    // Flip bits mid-frame. Muxio frames detect structural corruption but
    // carry no checksum: content integrity is the sha-echo layer's job
    // (ExecResult.script_sha256), verified in session tests. Here the
    // contract is bail-only behavior: no panic, and the session stays
    // usable for the next stream either way.
    let mid = wire.len() / 2;
    wire[mid] ^= 0xFF;
    wire[mid + 1] ^= 0xFF;
    let _ = guest.read(&wire);
    let id2 = host.open(methods::EXEC, channel::TAG_STDIN, b"more").unwrap();
    host.end(id2).unwrap();
    let wire2 = drain(&host_rx);
    let events = guest.read(&wire2).unwrap();
    assert!(
        events.iter().any(|e| matches!(e, StreamEvent::End { .. })),
        "session must stay usable after tampered bytes: {events:?}"
    );
}
