use super::*;
use crate::provider::{Lease, Provider, new_shared_buf};
use std::path::Path;
use tokio::io::AsyncWriteExt;

#[derive(Default)]
struct Seen {
    lines: Vec<Vec<u8>>,
    first: bool,
}
struct Recording {
    seen: Arc<Mutex<Seen>>,
    stop: bool,
}
impl Observer for Recording {
    fn first_bytes(&mut self) {
        self.seen.lock().expect("test operation succeeds").first = true;
    }
    fn line_cap(&self) -> Option<usize> {
        (!self.stop).then_some(8)
    }
    fn line(&mut self, line: &[u8]) {
        self.seen
            .lock()
            .expect("test operation succeeds")
            .lines
            .push(line.to_vec());
        if line == b"stop" {
            self.stop = true;
        }
    }
}

#[test]
fn framing_is_independent_of_chunk_boundaries_and_discards_whole_oversized_lines() {
    let input = b"12345678\n123456789{}\nok\nlast";
    for split in 0..=input.len() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut observer = Recording {
            seen: std::sync::Arc::clone(&seen),
            stop: false,
        };
        let mut lines = Lines::default();
        lines.feed(&input[..split], &mut observer);
        assert!(lines.pending.len() <= 8);
        lines.feed(&input[split..], &mut observer);
        lines.finish(&mut observer);
        assert_eq!(
            seen.lock().expect("test operation succeeds").lines,
            [b"12345678".to_vec(), b"ok".to_vec(), b"last".to_vec()]
        );
    }
    let mut observer = Recording {
        seen: Arc::default(),
        stop: false,
    };
    let mut lines = Lines::default();
    lines.feed(b"123456789", &mut observer);
    lines.feed(b"{}", &mut observer);
    lines.finish(&mut observer);
    assert!(
        observer
            .seen
            .lock()
            .expect("test operation succeeds")
            .lines
            .is_empty()
    );
}

#[tokio::test]
async fn capture_and_observation_finish_independently() {
    for input in [b"stop\nafter\n".as_slice(), b"one\ntwo\nlast".as_slice()] {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let capture = Capture::head(new_shared_buf(), 5);
        read_stream(
            input,
            capture.clone(),
            Recording {
                seen: std::sync::Arc::clone(&seen),
                stop: false,
            },
        )
        .await;
        assert_eq!(capture.snapshot(), &input[..5]);
        let seen = seen.lock().expect("test operation succeeds");
        assert!(seen.first);
        assert_eq!(
            seen.lines.len(),
            if input.starts_with(b"stop") { 1 } else { 3 }
        );
    }
    let capture = Capture::head(new_shared_buf(), 100);
    read_stream(
        b"stop\nafter\n".as_slice(),
        capture.clone(),
        Recording {
            seen: Arc::default(),
            stop: false,
        },
    )
    .await;
    assert_eq!(capture.snapshot(), b"stop\nafter\n");
}

#[test]
fn head_tail_snapshots_reject_cut_fragments_and_preserve_true_boundaries() {
    let capture = Capture::head_tail(new_shared_buf(), 2, 2);
    capture.push(b"{}aaaaaaaaaa{}\n");
    assert!(
        capture.snapshot().is_empty(),
        "neither JSON-looking cut is a line"
    );
    assert!(
        capture.snapshot().is_empty(),
        "snapshot does not mutate state"
    );
    let capture = Capture::head_tail(new_shared_buf(), 2, 3);
    capture.push(b"x\nabc\nok\n");
    assert_eq!(capture.snapshot(), b"x\nok\n", "trim exactly after newline");
    let capture = Capture::head_tail(new_shared_buf(), 2, 0);
    capture.push(b"{}more");
    assert!(capture.snapshot().is_empty());
    let capture = Capture::head_tail(new_shared_buf(), 10, 2);
    capture.push(b"partial");
    assert_eq!(
        capture.snapshot(),
        b"partial",
        "untruncated EOF partial survives"
    );
}

#[tokio::test]
async fn codex_first_byte_and_session_facts_survive_reader_abort() {
    let owner = Lease::learned(
        Provider::Codex,
        Path::new("/nonexistent"),
        &crate::data::DataDir::unavailable(),
        &crate::sessions::handoff::RunId::default(),
        None,
        None,
    );
    let lease = Lease::clone(&owner);
    let (mut writer, reader) = tokio::io::duplex(256);
    let capture = Capture::head(new_shared_buf(), 0);
    let task = tokio::spawn(read_stream(
        reader,
        capture,
        crate::provider::codex::CodexObserver {
            lease: lease.clone(),
            scanning: true,
        },
    ));
    writer
        .write_all(b"x")
        .await
        .expect("test operation succeeds");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !lease.is_ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first-byte readiness published");
    assert!(lease.is_ready(), "no line delimiter required");
    writer
        .write_all(b"\n{\"type\":\"thread.started\",\"thread_id\":\"safe\"}\n")
        .await
        .expect("test operation succeeds");
    let mut sessions = lease.session_watch();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while sessions.borrow_and_update().is_none() {
            sessions.changed().await.expect("session publisher alive");
        }
    })
    .await
    .expect("session published before abort");
    task.abort();
    let _ = task.await;
    assert_eq!(lease.session().as_deref(), Some("safe"));
}

struct ErrorAfter(std::io::Cursor<Vec<u8>>);
impl tokio::io::AsyncRead for ErrorAfter {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.0.position() == self.0.get_ref().len() as u64 {
            return std::task::Poll::Ready(Err(std::io::Error::other("read failed")));
        }
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

#[tokio::test]
async fn read_error_offers_eligible_partial_once() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    read_stream(
        ErrorAfter(std::io::Cursor::new(b"last".to_vec())),
        Capture::head(new_shared_buf(), 0),
        Recording {
            seen: std::sync::Arc::clone(&seen),
            stop: false,
        },
    )
    .await;
    assert_eq!(
        seen.lock().expect("test operation succeeds").lines,
        [b"last".to_vec()]
    );
}

#[tokio::test]
async fn collection_joins_reader_cancellation_before_snapshot() {
    struct Dropped(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let guard = Dropped(Arc::clone(&dropped));
    let (ready, started) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _guard = guard;
        ready.send(()).expect("collector listening");
        std::future::pending::<()>().await;
    });
    started.await.expect("reader started");
    crate::provider::collect_reader(
        task,
        &new_shared_buf(),
        std::time::Duration::ZERO,
        "test reader",
    )
    .await;
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
}
