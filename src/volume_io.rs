//! Bounded subprocess I/O and async isolation for the station volume backend.
use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    io::Read,
    process::{Child, Command, Output, Stdio},
    sync::{mpsc, Arc, OnceLock},
    time::{Duration, Instant},
};

const MAX_OUTPUT: usize = 64 * 1024;
pub(super) const AMIXER_TIMEOUT: Duration = Duration::from_secs(2);

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe { libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL); }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn drain(mut pipe: impl Read + Send + 'static, tx: mpsc::Sender<std::io::Result<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 4096];
        let result = loop {
            match pipe.read(&mut buf) {
                Ok(0) => break Ok(kept),
                Ok(n) => {
                    // Keep memory bounded but drain excess bytes to avoid pipe deadlock.
                    let take = n.min(MAX_OUTPUT.saturating_sub(kept.len()));
                    kept.extend_from_slice(&buf[..take]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => break Err(e),
            }
        };
        let _ = tx.send(result);
    });
}

pub(super) fn bounded_output(command: &mut Command, timeout: Duration) -> Result<Output> {
    let deadline = Instant::now() + timeout;
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = ChildGuard(command.spawn().context("start volume helper")?);
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    drain(child.0.stdout.take().context("volume helper stdout")?, out_tx);
    drain(child.0.stderr.take().context("volume helper stderr")?, err_tx);
    let status = loop {
        if let Some(status) = child.0.try_wait().context("wait for volume helper")? {
            break status;
        }
        if Instant::now() >= deadline { bail!("volume helper timed out"); }
        std::thread::sleep(Duration::from_millis(10));
    };
    // A helper descendant may retain a pipe after the parent exits. Do not join
    // a reader indefinitely; the guard kills the process group on every exit.
    let stdout = out_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .context("volume helper stdout timed out")??;
    let stderr = err_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .context("volume helper stderr timed out")??;
    Ok(Output { status, stdout, stderr })
}

async fn dispatch_serialized<T, F>(operation: F) -> Result<T>
where T: Send + 'static, F: FnOnce() -> Result<T> + Send + 'static {
    static SLOT: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let slot = SLOT.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))).clone();
    let permit = tokio::time::timeout(Duration::from_secs(1), slot.acquire_owned())
        .await.context("volume backend is busy")?.context("volume backend unavailable")?;
    tokio::task::spawn_blocking(move || {
        // Keep serialization until the real operation finishes, even if its
        // HTTP caller disconnects. No unbounded queue of blocking tasks.
        let _permit = permit;
        operation()
    }).await.context("volume backend task failed")?
}

static WRITE_EPOCH: AtomicU64 = AtomicU64::new(0);
type ReadResult = std::result::Result<u8, String>;
struct ReadFlight {
    epoch: u64,
    result: tokio::sync::watch::Sender<Option<ReadResult>>,
}
static READ_FLIGHT: OnceLock<tokio::sync::Mutex<Option<Arc<ReadFlight>>>> = OnceLock::new();

pub(super) async fn dispatch<T, F>(operation: F) -> Result<T>
where T: Send + 'static, F: FnOnce() -> Result<T> + Send + 'static {
    // A read arriving after a write request must never join a pre-write read.
    WRITE_EPOCH.fetch_add(1, Ordering::AcqRel);
    dispatch_serialized(move || {
        struct FinishWrite;
        impl Drop for FinishWrite {
            fn drop(&mut self) { WRITE_EPOCH.fetch_add(1, Ordering::AcqRel); }
        }
        // This lifetime follows real backend work, not a cancelled HTTP caller.
        let _finished = FinishWrite;
        operation()
    }).await
}

pub(super) async fn read_current<F>(operation: F) -> Result<u8>
where F: FnOnce() -> Result<u8> + Send + 'static {
    let slot = READ_FLIGHT.get_or_init(|| tokio::sync::Mutex::new(None));
    let (flight, start) = {
        let mut shared = slot.lock().await;
        let epoch = WRITE_EPOCH.load(Ordering::Acquire);
        match shared.as_ref() {
            Some(current) if current.epoch == epoch && current.result.borrow().is_none() => (current.clone(), false),
            _ => {
                let (result, _) = tokio::sync::watch::channel(None);
                let flight = Arc::new(ReadFlight { epoch, result });
                *shared = Some(flight.clone());
                (flight, true)
            }
        }
    };
    if start {
        let work = flight.clone();
        tokio::spawn(async move {
            let result = dispatch_serialized(operation).await.map_err(|e| format!("{e:#}"));
            work.result.send_replace(Some(result));
            let mut shared = slot.lock().await;
            if shared.as_ref().is_some_and(|current| Arc::ptr_eq(current, &work)) {
                *shared = None;
            }
        });
    }
    let mut result = flight.result.subscribe();
    loop {
        // Only concurrent in-flight reads are shared. No completed/stale value
        // is used as an answer to a new API request.
        let available = result.borrow().clone();
        if let Some(value) = available { return value.map_err(anyhow::Error::msg); }
        result.changed().await.context("volume read task stopped")?;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    static ASYNC_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn preserves_stdout_stderr_and_nonzero_exit() {
        let output = bounded_output(Command::new("sh").args(["-c", "printf good; printf bad >&2; exit 7"]), Duration::from_secs(2)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"good");
        assert_eq!(output.stderr, b"bad");
    }

    #[test]
    fn drains_large_output_without_unbounded_memory() {
        let output = bounded_output(Command::new("sh").args(["-c", "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2"]), Duration::from_secs(3)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), MAX_OUTPUT);
        assert_eq!(output.stderr.len(), MAX_OUTPUT);
    }

    #[test]
    fn hanging_helper_times_out() {
        let started = Instant::now();
        let result = bounded_output(Command::new("sh").args(["-c", "exec sleep 30"]), Duration::from_millis(100));
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn inherited_pipe_cannot_extend_deadline() {
        let started = Instant::now();
        let result = bounded_output(Command::new("sh").args(["-c", "sleep 30 & exit 0"]), Duration::from_millis(100));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn missing_program_returns_error() {
        assert!(bounded_output(&mut Command::new("/definitely-missing-volume-helper"), Duration::from_secs(1)).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timeout_stops_parent_and_descendant() {
        let path = std::env::temp_dir().join(format!("volume-pids-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
        let script = format!("echo $$ > '{}'; sleep 30 & echo $! >> '{}'; wait", path.display(), path.display());
        let result = bounded_output(Command::new("sh").args(["-c", &script]), Duration::from_millis(500));
        assert!(result.is_err());
        let pids = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!(pids.lines().count(), 2);
        for pid in pids.lines() {
            let limit = Instant::now() + Duration::from_secs(1);
            loop {
                let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
                if status.is_empty() || status.lines().any(|s| s.starts_with("State:") && s.contains("Z (zombie)")) { break; }
                assert!(Instant::now() < limit, "helper {pid} still running");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[tokio::test]
    async fn concurrent_backend_operations_are_serialized() {
        let _serial = ASYNC_TEST_LOCK.lock().unwrap();
        use std::sync::atomic::{AtomicUsize, Ordering};
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let operation = || {
            let active = active.clone(); let peak = peak.clone();
            move || {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }
        };
        let (a,b) = tokio::join!(dispatch(operation()), dispatch(operation()));
        a.unwrap(); b.unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn backend_does_not_block_async_runtime_and_recovers_after_error() {
        let _serial = ASYNC_TEST_LOCK.lock().unwrap();
        let (tx, rx) = mpsc::channel();
        let backend = dispatch(move || {
            rx.recv_timeout(Duration::from_secs(2)).context("async runtime stalled")?;
            bail!("fixture backend failure")
        });
        let heartbeat = async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tx.send(()).unwrap();
        };
        let (result, _) = tokio::join!(backend, heartbeat);
        let result: Result<()> = result;
        assert!(result.unwrap_err().to_string().contains("fixture backend failure"));
        assert_eq!(dispatch(|| Ok(42)).await.unwrap(), 42);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_gets_share_only_one_real_read() {
        let _serial = ASYNC_TEST_LOCK.lock().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut jobs = Vec::new();
        for _ in 0..16 {
            let calls = calls.clone();
            jobs.push(tokio::spawn(read_current(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                Ok(42)
            })));
        }
        for job in jobs { assert_eq!(job.await.unwrap().unwrap(), 42); }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(read_current(|| Ok(55)).await.unwrap(), 55);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_shared_read_is_not_cached_as_success() {
        let _serial = ASYNC_TEST_LOCK.lock().unwrap();
        let mut jobs = Vec::new();
        for _ in 0..8 {
            jobs.push(tokio::spawn(read_current(|| {
                std::thread::sleep(Duration::from_millis(30));
                bail!("fixture read failed")
            })));
        }
        for job in jobs { assert!(job.await.unwrap().unwrap_err().to_string().contains("fixture read failed")); }
        assert_eq!(read_current(|| Ok(77)).await.unwrap(), 77);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_reader_does_not_spawn_duplicate_backend_work() {
        let _serial = ASYNC_TEST_LOCK.lock().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(read_current(move || {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(100));
            Ok(42)
        }));
        started_rx.await.unwrap(); caller.abort();
        assert_eq!(read_current(|| Ok(99)).await.unwrap(), 42);
        assert_eq!(read_current(|| Ok(55)).await.unwrap(), 55);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_after_queued_write_never_joins_pre_write_snapshot() {
        let _serial = ASYNC_TEST_LOCK.lock().unwrap();
        let value = Arc::new(std::sync::atomic::AtomicU8::new(10));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let old = tokio::spawn(read_current(move || {
            let _ = started_tx.send(());
            release_rx.recv_timeout(Duration::from_secs(2))?;
            Ok(10)
        }));
        started_rx.await.unwrap();
        let before = WRITE_EPOCH.load(Ordering::Acquire);
        let write_value = value.clone();
        let write = tokio::spawn(dispatch(move || { write_value.store(20, Ordering::SeqCst); Ok(()) }));
        while WRITE_EPOCH.load(Ordering::Acquire) == before { tokio::task::yield_now().await; }
        let expected_epoch = WRITE_EPOCH.load(Ordering::Acquire);
        let read_value = value.clone();
        let new = tokio::spawn(read_current(move || Ok(read_value.load(Ordering::SeqCst))));
        loop {
            let epoch = READ_FLIGHT.get().unwrap().lock().await.as_ref().map(|f| f.epoch);
            if epoch == Some(expected_epoch) { break; }
            tokio::task::yield_now().await;
        }
        release_tx.send(()).unwrap();
        assert_eq!(old.await.unwrap().unwrap(), 10);
        write.await.unwrap().unwrap();
        assert_eq!(new.await.unwrap().unwrap(), 20);
    }
}
