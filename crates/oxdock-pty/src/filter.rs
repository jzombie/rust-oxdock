//! ConPTY setup handshake filter.
//!
//! conhost emits a cursor-position query when a console is created
//! without an attached terminal (CI runners, services), then stalls
//! console setup until something answers it. A real terminal answers
//! in band as part of VT handling; this pump is the only reader, so
//! the output worker answers on the caller's behalf. The query itself
//! is consumed, never forwarded: a real terminal's answer would land
//! as stray input in the child. Only conhost emits this sequence
//! toward us (a child cursor query travels the opposite direction), so
//! answering never injects stray input into the child. Unix ptys never
//! emit the query, so this is inert there.

use std::io::Write;
use std::sync::{Arc, Mutex};

/// ConPTY console-setup query answered on the master's behalf.
const CONHOST_DSR_QUERY: &[u8] = b"\x1b[6n";
/// Fresh 1,1 cursor position reported in reply.
const CONHOST_CPR_REPLY: &[u8] = b"\x1b[1;1R";

/// Streaming `ESC[6n` filter over master output: answers each query
/// once through the master writer and releases the chunk minus query
/// bytes. A trailing query-prefix holds for the next chunk, so a
/// query split over two reads still matches and never reaches the
/// wire half. `flush` releases held bytes at EOF: they never
/// completed a query, so they are genuine output.
#[derive(Default)]
pub struct DsrFilter {
    carry: [u8; 3],
    len: usize,
}

impl DsrFilter {
    /// Empty filter with no held prefix.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer completed queries found in `chunk`, returning the chunk
    /// minus query bytes.
    pub fn feed(&mut self, chunk: &[u8], master_in: &Arc<Mutex<Box<dyn Write + Send>>>) -> Vec<u8> {
        let mut probe = Vec::with_capacity(self.len + chunk.len());
        probe.extend_from_slice(&self.carry[..self.len]);
        probe.extend_from_slice(chunk);
        // Consumed bytes never rescan: a completed query is new and
        // answered exactly once, so every match here is fresh.
        let mut answers = 0;
        let mut kept = Vec::with_capacity(probe.len());
        let mut idx = 0;
        while idx + CONHOST_DSR_QUERY.len() <= probe.len() {
            if probe[idx..idx + CONHOST_DSR_QUERY.len()] == CONHOST_DSR_QUERY[..] {
                answers += 1;
                idx += CONHOST_DSR_QUERY.len();
            } else {
                kept.push(probe[idx]);
                idx += 1;
            }
        }
        kept.extend_from_slice(&probe[idx..]);
        // Hold back the longest trailing query-prefix: it may yet
        // complete a split query, and must not reach the wire half.
        let widest = kept.len().min(CONHOST_DSR_QUERY.len() - 1);
        let mut hold = 0;
        for width in (1..=widest).rev() {
            if kept[kept.len() - width..] == CONHOST_DSR_QUERY[..width] {
                hold = width;
                break;
            }
        }
        let emit = kept[..kept.len() - hold].to_vec();
        self.carry[..hold].copy_from_slice(&kept[kept.len() - hold..]);
        self.len = hold;
        if answers > 0
            && let Ok(mut guard) = master_in.lock()
        {
            for _ in 0..answers {
                let _ = guard.write_all(CONHOST_CPR_REPLY);
            }
            let _ = guard.flush();
        }
        emit
    }

    /// Release held prefix bytes at EOF: they never completed a query,
    /// so they are genuine output.
    pub fn flush(&mut self) -> Vec<u8> {
        let tail = self.carry[..self.len].to_vec();
        self.len = 0;
        tail
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;
    type SeenBytes = Arc<Mutex<Vec<u8>>>;

    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("sink lock unpoisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn harness() -> (SharedWriter, SeenBytes) {
        let seen: SeenBytes = Arc::new(Mutex::new(Vec::new()));
        let writer: Box<dyn Write + Send> = Box::new(Sink(Arc::clone(&seen)));
        (Arc::new(Mutex::new(writer)), seen)
    }

    #[test]
    fn query_answered_once_and_stripped() {
        let (master_in, seen) = harness();
        let mut filter = DsrFilter::new();
        assert_eq!(filter.feed(b"ab\x1b[6ncd", &master_in).as_slice(), b"abcd");
        assert_eq!(
            seen.lock().expect("sink readable").as_slice(),
            b"\x1b[1;1R".as_slice()
        );
        assert!(filter.flush().is_empty());
    }

    #[test]
    fn split_query_answered_once_and_stripped() {
        let (master_in, seen) = harness();
        let mut filter = DsrFilter::new();
        assert_eq!(filter.feed(b"abc\x1b[", &master_in).as_slice(), b"abc");
        assert!(seen.lock().expect("sink readable").is_empty());
        assert_eq!(filter.feed(b"6nrest", &master_in).as_slice(), b"rest");
        assert_eq!(
            seen.lock().expect("sink readable").as_slice(),
            b"\x1b[1;1R".as_slice()
        );
        // Replay past the boundary: the consumed query never rescans.
        assert_eq!(filter.feed(b"more", &master_in).as_slice(), b"more");
        assert_eq!(
            seen.lock().expect("sink readable").as_slice(),
            b"\x1b[1;1R".as_slice()
        );
        assert!(filter.flush().is_empty());
    }

    #[test]
    fn trailing_prefix_flushed_at_eof() {
        let (master_in, seen) = harness();
        let mut filter = DsrFilter::new();
        assert_eq!(
            filter.feed(b"ends with \x1b[", &master_in).as_slice(),
            b"ends with "
        );
        assert!(seen.lock().expect("sink readable").is_empty());
        assert_eq!(filter.flush().as_slice(), b"\x1b[");
    }

    #[test]
    fn silent_without_query() {
        let (master_in, seen) = harness();
        let mut filter = DsrFilter::new();
        assert_eq!(
            filter
                .feed(b"plain output, no query", &master_in)
                .as_slice(),
            b"plain output, no query"
        );
        assert!(seen.lock().expect("sink readable").is_empty());
        assert!(filter.flush().is_empty());
    }
}
