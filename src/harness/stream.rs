//! One draining loop, independent bounded capture and physical-line framing.
use super::{SharedBuf, push_capped};
use std::sync::{Arc, Mutex};

/// Observations must publish to shared owners synchronously: a reader may be
/// cancelled at its next read. Unfinished lines are not observations on abort.
pub(super) trait Observer {
    fn first_bytes(&mut self) {}
    /// None disables scanning permanently. The cap excludes the newline.
    fn line_cap(&self) -> Option<usize>;
    fn line(&mut self, line: &[u8]);
    /// A line longer than the cap was discarded unobserved.
    fn skipped(&mut self) {}
    /// The stream reached end of file. Not called after a read error, nor on
    /// a reader cancelled before the end, so an observer that never sees it
    /// knows its view of the stream is partial.
    fn eof(&mut self) {}
}

pub(super) struct Ignore;
impl Observer for Ignore {
    fn line_cap(&self) -> Option<usize> {
        None
    }
    fn line(&mut self, _: &[u8]) {}
}

#[derive(Clone)]
pub(super) struct Capture {
    head: SharedBuf,
    cap: usize,
    tail: Option<Arc<Mutex<Tail>>>,
}

struct Tail {
    bytes: Vec<u8>,
    keep: usize,
    overflowed: bool,
    starts_at_boundary: bool,
}

impl Capture {
    pub(super) fn head(head: SharedBuf, cap: usize) -> Self {
        Self {
            head,
            cap,
            tail: None,
        }
    }

    pub(super) fn head_tail(head: SharedBuf, cap: usize, keep: usize) -> Self {
        Self {
            head,
            cap,
            tail: Some(Arc::new(Mutex::new(Tail {
                bytes: Vec::new(),
                keep,
                overflowed: false,
                starts_at_boundary: true,
            }))),
        }
    }

    fn push(&self, mut bytes: &[u8]) {
        let Some(tail) = &self.tail else {
            push_capped(&self.head, bytes, self.cap);
            return;
        };
        // Always lock in head/tail order, also used by snapshot. No await.
        if let (Ok(mut head), Ok(mut tail)) = (self.head.lock(), tail.lock()) {
            let take = self.cap.saturating_sub(head.len()).min(bytes.len());
            head.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if bytes.is_empty() {
                return;
            }
            if !tail.overflowed {
                tail.starts_at_boundary = head.last().is_none_or(|b| *b == b'\n');
                tail.overflowed = true;
            }
            if tail.keep == 0 {
                return;
            }
            tail.bytes.extend_from_slice(bytes);
            if tail.bytes.len() > tail.keep.saturating_mul(2) {
                let excess = tail.bytes.len() - tail.keep;
                tail.starts_at_boundary = tail.bytes[excess - 1] == b'\n';
                tail.bytes.drain(..excess);
            }
        }
    }

    /// Head-only capture stays raw. A discontinuous head/tail capture exposes
    /// only authentic line starts, never JSON fabricated at a retention cut.
    /// A final partial is retained, just as it is offered by the live framer.
    pub(super) fn snapshot(&self) -> Vec<u8> {
        let Ok(head) = self.head.lock() else {
            return Vec::new();
        };
        let Some(tail) = &self.tail else {
            return head.clone();
        };
        let Ok(tail) = tail.lock() else {
            return Vec::new();
        };
        if !tail.overflowed {
            return head.clone();
        }
        let head_end = head.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let mut out = head[..head_end].to_vec();
        let start = if tail.starts_at_boundary {
            0
        } else {
            tail.bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(tail.bytes.len(), |i| i + 1)
        };
        // The surviving head ends in a delimiter, so the two regions cannot
        // combine into a fabricated event (even when the middle was omitted).
        out.extend_from_slice(&tail.bytes[start..]);
        out
    }
}

/// Physical-line framing for observers such as session discovery. A line is
/// offered only up to the observer's cap (codex: 1 MiB). An oversized line is
/// discarded through its newline and never reparsed from a suffix, which could
/// otherwise read as a fabricated event. A final unterminated line is offered
/// at EOF.
#[derive(Default)]
struct Lines {
    pending: Vec<u8>,
    oversized: bool,
}

impl Lines {
    fn append(&mut self, bytes: &[u8], cap: usize) {
        if self.oversized {
            return;
        }
        if bytes.len() > cap.saturating_sub(self.pending.len()) {
            self.pending.clear();
            self.oversized = true;
        } else {
            self.pending.extend_from_slice(bytes);
        }
    }

    fn feed(&mut self, mut bytes: &[u8], observer: &mut impl Observer) {
        while let Some(cap) = observer.line_cap() {
            let end = bytes.iter().position(|b| *b == b'\n');
            let len = end.unwrap_or(bytes.len());
            self.append(&bytes[..len], cap);
            let Some(end) = end else {
                return;
            };
            if self.oversized {
                observer.skipped();
            } else {
                observer.line(&self.pending);
            }
            self.pending.clear();
            self.oversized = false;
            bytes = &bytes[end + 1..];
        }
        // Once scanning stops, release even the allocation held by the carry.
        self.pending = Vec::new();
    }

    fn finish(&mut self, observer: &mut impl Observer) {
        if observer.line_cap().is_none() {
            return;
        }
        if self.oversized {
            observer.skipped();
        } else if !self.pending.is_empty() {
            observer.line(&self.pending);
        }
    }
}

pub(super) async fn read_stream<R>(mut reader: R, capture: Capture, mut observer: impl Observer)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 16384];
    let mut lines = Lines::default();
    let mut first = true;
    let mut reached_eof = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => {
                reached_eof = true;
                break;
            }
            Err(_) => break,
            Ok(n) => {
                if first {
                    observer.first_bytes();
                    first = false;
                }
                capture.push(&chunk[..n]);
                lines.feed(&chunk[..n], &mut observer);
            }
        }
    }
    lines.finish(&mut observer);
    if reached_eof {
        observer.eof();
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
