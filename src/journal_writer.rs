//! Bounded, single-owner journal appends on a background thread.
//!
//! A ticket completes only after `Journal::append` has flushed and synced its
//! frame. Dropping a ticket only drops its acknowledgment receiver; accepted
//! work remains owned by the writer and is still drained on shutdown.

use crate::recovery::Journal;
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
};

struct Shared {
    pending_bytes: AtomicUsize,
    pending_items: AtomicUsize,
    /// First unreported worker error, consumed by `take_error`.
    pending_error: Mutex<Option<String>>,
    /// First worker error, retained for `shutdown` even if polled earlier.
    terminal_error: Mutex<Option<String>>,
}

struct AppendRequest {
    payload: Vec<u8>,
    charged_bytes: usize,
    completion: mpsc::Sender<Result<(), String>>,
}

/// A nonblocking FIFO producer for one journal-owning worker.
pub struct JournalWriter {
    sender: Option<SyncSender<AppendRequest>>,
    worker: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    max_queued_items: usize,
    max_queued_bytes: usize,
}

/// Error returned when an append cannot be accepted without blocking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// The bounded message queue has no free slot.
    QueueFull,
    /// Queued and in-flight payload capacities reached the configured byte cap.
    ByteLimit,
    /// The worker has stopped or could not be created.
    WorkerUnavailable,
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => f.write_str("journal writer queue is full"),
            Self::ByteLimit => f.write_str("journal writer byte limit is full"),
            Self::WorkerUnavailable => f.write_str("journal writer worker is unavailable"),
        }
    }
}

impl std::error::Error for SubmitError {}

/// A durable-completion acknowledgment for one accepted append.
pub struct JournalTicket {
    receiver: Option<Receiver<Result<(), String>>>,
    result: Option<Result<(), String>>,
}

impl JournalTicket {
    /// Poll the append. `Ok(None)` means the durable append is still pending.
    /// The resolved result is retained, so repeated polls return the same value.
    pub fn try_result(&mut self) -> Result<Option<()>, String> {
        if let Some(result) = &self.result {
            return result.clone().map(Some);
        }
        let Some(receiver) = &self.receiver else {
            return Err("journal writer acknowledgment is unavailable".to_owned());
        };
        let polled = receiver.try_recv();
        match polled {
            Ok(result) => {
                self.receiver = None;
                self.result = Some(result.clone());
                result.map(Some)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                let error = "journal writer stopped before acknowledging the append".to_owned();
                self.receiver = None;
                self.result = Some(Err(error.clone()));
                Err(error)
            }
        }
    }
}

impl JournalWriter {
    /// Start a single worker that exclusively owns `journal`.
    pub fn new(
        journal: Journal,
        queue_capacity: usize,
        max_queued_bytes: usize,
    ) -> io::Result<Self> {
        if queue_capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "journal writer queue capacity must be nonzero",
            ));
        }
        Self::new_inner(journal, queue_capacity, max_queued_bytes, None)
    }

    fn new_inner(
        journal: Journal,
        queue_capacity: usize,
        max_queued_bytes: usize,
        #[cfg(test)] start_gate: Option<Receiver<()>>,
        #[cfg(not(test))] _start_gate: Option<()>,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let shared = Arc::new(Shared {
            pending_bytes: AtomicUsize::new(0),
            pending_items: AtomicUsize::new(0),
            pending_error: Mutex::new(None),
            terminal_error: Mutex::new(None),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("kalium-journal-writer".to_owned())
            .spawn(move || {
                #[cfg(test)]
                if let Some(gate) = start_gate {
                    let _ = gate.recv();
                }
                run_worker(journal, receiver, worker_shared);
            })?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            shared,
            max_queued_items: queue_capacity,
            max_queued_bytes,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_paused(
        journal: Journal,
        queue_capacity: usize,
        max_queued_bytes: usize,
    ) -> (Self, mpsc::Sender<()>) {
        let (gate_sender, gate_receiver) = mpsc::channel();
        let writer = Self::new_inner(
            journal,
            queue_capacity,
            max_queued_bytes,
            Some(gate_receiver),
        )
        .expect("test worker thread should start");
        (writer, gate_sender)
    }

    /// Try to enqueue one payload without waiting for queue space or disk I/O.
    /// Capacity bytes are reserved until the worker finishes processing it.
    pub fn try_append(&mut self, payload: Vec<u8>) -> Result<JournalTicket, SubmitError> {
        let sender = self.sender.as_ref().ok_or(SubmitError::WorkerUnavailable)?;
        if !reserve_items(&self.shared.pending_items, self.max_queued_items) {
            return Err(SubmitError::QueueFull);
        }
        let charged_bytes = payload.capacity();
        if !reserve_bytes(&self.shared.pending_bytes, charged_bytes, self.max_bytes()) {
            release_items(&self.shared.pending_items);
            return Err(SubmitError::ByteLimit);
        }
        let (completion, receiver) = mpsc::channel();
        let request = AppendRequest {
            payload,
            charged_bytes,
            completion,
        };
        match sender.try_send(request) {
            Ok(()) => Ok(JournalTicket {
                receiver: Some(receiver),
                result: None,
            }),
            Err(TrySendError::Full(_request)) => {
                release_bytes(&self.shared.pending_bytes, charged_bytes);
                release_items(&self.shared.pending_items);
                Err(SubmitError::QueueFull)
            }
            Err(TrySendError::Disconnected(_request)) => {
                release_bytes(&self.shared.pending_bytes, charged_bytes);
                release_items(&self.shared.pending_items);
                Err(SubmitError::WorkerUnavailable)
            }
        }
    }

    /// Number of payload-capacity bytes reserved by queued or active work.
    #[cfg(test)]
    pub fn pending_bytes(&self) -> usize {
        self.shared.pending_bytes.load(Ordering::Acquire)
    }

    /// Take the first worker error not yet reported to the caller.
    pub fn take_error(&mut self) -> Option<String> {
        self.shared
            .pending_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Stop accepting new appends while allowing accepted work to drain.
    pub fn stop_accepting(&mut self) {
        self.sender.take();
    }

    /// Whether the worker has drained its queue and exited.
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stop accepting, drain all accepted appends, and join the worker.
    pub fn shutdown(mut self) -> Result<(), String> {
        self.stop_accepting();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                record_error(&self.shared, "journal writer worker panicked".to_owned());
            }
        }
        self.shared
            .terminal_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map_or(Ok(()), Err)
    }

    fn max_bytes(&self) -> usize {
        self.max_queued_bytes
    }
}

impl Drop for JournalWriter {
    fn drop(&mut self) {
        // Closing the last sender lets the worker drain accepted requests and
        // then exit. Explicit `shutdown` is the lifecycle barrier that joins it.
        self.sender.take();
    }
}

fn run_worker(journal: Journal, receiver: Receiver<AppendRequest>, shared: Arc<Shared>) {
    let mut journal = journal;
    let mut terminal_error: Option<String> = None;
    while let Ok(request) = receiver.recv() {
        let result = match &terminal_error {
            Some(error) => Err(error.clone()),
            None => match journal.append(&request.payload) {
                Ok(_) => Ok(()),
                Err(error) => {
                    let error = error.to_string();
                    record_error(&shared, error.clone());
                    terminal_error = Some(error.clone());
                    Err(error)
                }
            },
        };
        release_bytes(&shared.pending_bytes, request.charged_bytes);
        release_items(&shared.pending_items);
        let _ = request.completion.send(result);
    }
}

fn record_error(shared: &Shared, error: String) {
    let mut terminal = shared
        .terminal_error
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if terminal.is_none() {
        *terminal = Some(error.clone());
    }
    drop(terminal);
    let mut pending = shared
        .pending_error
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if pending.is_none() {
        *pending = Some(error);
    }
}

fn reserve_bytes(counter: &AtomicUsize, bytes: usize, limit: usize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(bytes) else {
            return false;
        };
        if next > limit {
            return false;
        }
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

fn reserve_items(counter: &AtomicUsize, limit: usize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(1) else {
            return false;
        };
        if next > limit {
            return false;
        }
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

fn release_items(counter: &AtomicUsize) {
    let previous = counter.fetch_sub(1, Ordering::AcqRel);
    debug_assert!(previous > 0, "journal writer item reservation underflow");
}

fn release_bytes(counter: &AtomicUsize, bytes: usize) {
    let previous = counter.fetch_sub(bytes, Ordering::AcqRel);
    debug_assert!(
        previous >= bytes,
        "journal writer byte reservation underflow"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery;
    use std::{
        fs,
        path::PathBuf,
        thread,
        time::{Duration, Instant},
    };
    use uuid::Uuid;

    struct TestJournal {
        path: PathBuf,
        project_id: Uuid,
        digest: [u8; 32],
        journal: Option<Journal>,
    }

    impl TestJournal {
        fn new() -> Self {
            let project_id = Uuid::new_v4();
            let path = std::env::temp_dir().join(format!(
                "kalium-journal-writer-{}-{project_id}.journal",
                std::process::id()
            ));
            let digest = [0x5a; 32];
            let journal = Journal::create(&path, project_id, digest, 1).unwrap();
            Self {
                path,
                project_id,
                digest,
                journal: Some(journal),
            }
        }

        fn take(&mut self) -> Journal {
            self.journal.take().unwrap()
        }

        fn records(&self) -> Vec<recovery::Record> {
            recovery::scan(&self.path, self.project_id, self.digest, 1).unwrap()
        }
    }

    impl Drop for TestJournal {
        fn drop(&mut self) {
            drop(self.journal.take());
            let _ = fs::remove_file(&self.path);
        }
    }

    fn await_ticket(ticket: &mut JournalTicket) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = ticket.try_result()? {
                return Ok(result);
            }
            assert!(Instant::now() < deadline, "journal ticket timed out");
            thread::yield_now();
        }
    }

    #[test]
    fn fifo_acknowledgments_follow_durable_scanned_records() {
        let mut fixture = TestJournal::new();
        let mut writer = JournalWriter::new(fixture.take(), 3, 128).unwrap();
        let payloads = [b"first".to_vec(), b"second".to_vec(), b"third".to_vec()];
        let mut tickets: Vec<_> = payloads
            .iter()
            .map(|payload| writer.try_append(payload.clone()).unwrap())
            .collect();

        for ticket in &mut tickets {
            await_ticket(ticket).unwrap();
        }
        writer.shutdown().unwrap();
        let records = fixture.records();
        assert_eq!(
            records
                .iter()
                .map(|record| record.payload.as_slice())
                .collect::<Vec<_>>(),
            payloads.iter().map(Vec::as_slice).collect::<Vec<_>>()
        );
    }

    #[test]
    fn queue_count_and_capacity_bytes_apply_nonblocking_backpressure() {
        let mut fixture = TestJournal::new();
        let (mut writer, release_worker) = JournalWriter::new_paused(fixture.take(), 1, 8);
        let mut first = writer
            .try_append(Vec::with_capacity(4))
            .expect("first request should fit");
        assert_eq!(writer.pending_bytes(), 4);
        assert!(matches!(
            writer.try_append(Vec::new()),
            Err(SubmitError::QueueFull)
        ));
        assert_eq!(writer.pending_bytes(), 4);
        release_worker.send(()).unwrap();
        await_ticket(&mut first).unwrap();
        writer.shutdown().unwrap();

        let mut fixture = TestJournal::new();
        let (mut writer, release_worker) = JournalWriter::new_paused(fixture.take(), 2, 5);
        let first = writer
            .try_append(Vec::with_capacity(3))
            .expect("first request should fit");
        assert_eq!(writer.pending_bytes(), 3);
        assert!(matches!(
            writer.try_append(Vec::with_capacity(3)),
            Err(SubmitError::ByteLimit)
        ));
        assert_eq!(writer.pending_bytes(), 3);
        release_worker.send(()).unwrap();
        drop(first);
        writer.shutdown().unwrap();
    }

    #[test]
    fn dropping_ticket_does_not_cancel_an_accepted_write() {
        let mut fixture = TestJournal::new();
        let (mut writer, release_worker) = JournalWriter::new_paused(fixture.take(), 1, 16);
        drop(writer.try_append(b"still-written".to_vec()).unwrap());
        release_worker.send(()).unwrap();
        writer.shutdown().unwrap();
        let records = fixture.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].payload, b"still-written");
    }

    #[test]
    fn shutdown_drains_all_accepted_requests() {
        let mut fixture = TestJournal::new();
        let (mut writer, release_worker) = JournalWriter::new_paused(fixture.take(), 2, 32);
        let _first = writer.try_append(b"one".to_vec()).unwrap();
        let _second = writer.try_append(b"two".to_vec()).unwrap();
        release_worker.send(()).unwrap();
        writer.shutdown().unwrap();
        let records = fixture.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].payload, b"one");
        assert_eq!(records[1].payload, b"two");
    }

    #[test]
    fn worker_error_is_reported_and_retained_for_shutdown() {
        let mut fixture = TestJournal::new();
        fixture
            .journal
            .as_mut()
            .unwrap()
            .set_remaining_capacity_for_test(0);
        let mut writer = JournalWriter::new(fixture.take(), 2, 64).unwrap();
        let mut first = writer.try_append(b"cannot-fit".to_vec()).unwrap();
        let mut second = writer.try_append(b"must-not-follow".to_vec()).unwrap();
        let ticket_error = await_ticket(&mut first).unwrap_err();
        assert_eq!(await_ticket(&mut second).unwrap_err(), ticket_error);
        assert_eq!(writer.take_error().as_deref(), Some(ticket_error.as_str()));
        assert_eq!(writer.shutdown(), Err(ticket_error));
    }

    #[test]
    fn zero_queue_capacity_is_rejected() {
        let mut fixture = TestJournal::new();
        assert!(JournalWriter::new(fixture.take(), 0, 16).is_err());
    }
}
