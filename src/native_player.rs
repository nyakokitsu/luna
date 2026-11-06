//! Bounded, explicit pause of the stock Quasar player. Not a media-provider
//! command: selecting a plugin provider must never redirect native-player pause.
use anyhow::{anyhow, bail, Context, Result};
use std::{
    io::{Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, OnceLock, Arc, atomic::{AtomicUsize, Ordering}},
    thread,
    time::{Duration, Instant},
};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

// CLI startup consumes the same absolute deadline as queueing/submission.
// Do not restart the ten-second request clock or impose a shorter independent
// connect deadline which can reject a still-valid request near readiness.
const IDLE_LEASE: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

enum Operation { Pause, Release }
struct Request {
    owner: String,
    operation: Operation,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<()>>,
}

pub fn available() -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata("/system/vendor/quasar/q")
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn service() -> &'static mpsc::SyncSender<Request> {
    static SERVICE: OnceLock<mpsc::SyncSender<Request>> = OnceLock::new();
    SERVICE.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Request>(8);
        thread::spawn(move || {
            let mut active: Option<(String, Session)> = None;
            loop {
                let request = match rx.recv_timeout(IDLE_LEASE) {
                    Ok(r) => r,
                    Err(mpsc::RecvTimeoutError::Timeout) => { active = None; continue; }
                    Err(_) => break,
                };
                let result = handle(&mut active, &request);
                let _ = request.reply.try_send(result);
            }
        });
        tx
    })
}

fn connect_budget(deadline: Instant, now: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(now);
    if remaining.is_zero() { bail!("native player request expired"); }
    Ok(remaining)
}

fn handle(active: &mut Option<(String, Session)>, r: &Request) -> Result<()> {
    if Instant::now() >= r.deadline { bail!("native player request expired"); }
    if active.as_ref().is_some_and(|(owner, _)| owner != &r.owner) {
        bail!("native player is leased by another invocation");
    }
    match r.operation {
        Operation::Release => { *active = None; Ok(()) }
        Operation::Pause => {
            if active.is_none() {
                *active = Some((r.owner.clone(), Session::start("/system/vendor/quasar/q", connect_budget(r.deadline, Instant::now())?)?));
            }
            if Instant::now() >= r.deadline { *active = None; bail!("native player request expired"); }
            let result = active.as_mut().unwrap().1.pause();
            if result.is_err() { *active = None; }
            result
        }
    }
}

fn request(owner: String, operation: Operation) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel(1);
    service().try_send(Request {
        owner, operation, deadline: Instant::now() + REQUEST_TIMEOUT, reply: tx,
    }).map_err(|_| anyhow!("native player service busy"))?;
    rx.recv_timeout(REQUEST_TIMEOUT).context("native player service timed out")?
}
pub fn pause(owner: String) -> Result<()> { request(owner, Operation::Pause) }
pub fn release(owner: String) -> Result<()> { request(owner, Operation::Release) }

struct Session { child: Child, input: ChildStdin }
impl Session {
    fn start(path: &str, timeout: Duration) -> Result<Self> {
        let mut command = Command::new(path);
        command.arg("cli").env_remove("LD_LIBRARY_PATH").env_remove("LD_PRELOAD")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().context("start stock Quasar CLI")?;
        let input = child.stdin.take().context("stock CLI stdin missing")?;
        let (tx, rx) = mpsc::sync_channel(2);
        // Drain both streams with bounded memory; readiness may be on either.
        let stdout_bytes = Arc::new(AtomicUsize::new(0));
        let stderr_bytes = Arc::new(AtomicUsize::new(0));
        if let Some(stdout) = child.stdout.take() { drain_counted(stdout, tx.clone(), stdout_bytes.clone()); }
        if let Some(stderr) = child.stderr.take() { drain_counted(stderr, tx, stderr_bytes.clone()); }
        let mut session = Self { child, input };
        let started = Instant::now();
        if let Err(error) = rx.recv_timeout(timeout) {
            let wait = match error {
                mpsc::RecvTimeoutError::Timeout => "timeout",
                mpsc::RecvTimeoutError::Disconnected => "disconnected",
            };
            let child = session.child.try_wait()?
                .map(|status| status.to_string()).unwrap_or_else(|| "running".into());
            // Only timings, counts and exit status: never raw vendor output,
            // environment, arguments or runtime credentials.
            bail!("stock Quasar CLI not ready: wait={} elapsed_ms={} child={} stdout_bytes={} stderr_bytes={}",
                wait, started.elapsed().as_millis(), child,
                stdout_bytes.load(Ordering::Relaxed), stderr_bytes.load(Ordering::Relaxed));
        }
        Ok(session)
    }
    fn pause(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_some() { bail!("stock Quasar CLI exited"); }
        // Explicit pause, never toggle or resume. This confirms submission to
        // the CLI, not an independently measured playback-state transition.
        self.input.write_all(b"o\n").context("submit native pause")?;
        self.input.flush()?;
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.input.write_all(b"x\n");
        #[cfg(unix)]
        unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL); }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[cfg(test)]
fn drain(stream: impl Read + Send + 'static, ready: mpsc::SyncSender<()>) {
    drain_counted(stream, ready, Arc::new(AtomicUsize::new(0)));
}
fn drain_counted(mut stream: impl Read + Send + 'static, ready: mpsc::SyncSender<()>, bytes: Arc<AtomicUsize>) {
    thread::spawn(move || {
        let marker = b"Connected to DirectiveSequencer";
        let mut tail = Vec::new();
        let mut buffer = [0u8;4096];
        loop {
            let n = match stream.read(&mut buffer) { Ok(0) | Err(_) => break, Ok(n) => n };
            bytes.fetch_add(n, Ordering::Relaxed);
            tail.extend_from_slice(&buffer[..n]);
            if tail.windows(marker.len()).any(|w| w == marker) { let _ = ready.try_send(()); }
            let keep = marker.len()-1;
            if tail.len() > keep { tail.drain(..tail.len()-keep); }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ready_marker_survives_chunk_boundaries() {
        let (tx,rx)=mpsc::sync_channel(2);
        drain(std::io::Cursor::new(b"Connected to DirectiveSequencer\n".to_vec()),tx);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }
    #[test]
    fn missing_marker_is_not_ready() {
        let (tx,rx)=mpsc::sync_channel(2);
        drain(std::io::Cursor::new(b"UserInput: Player Pause\n".to_vec()),tx);
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }
    fn fixture(body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!("luna-cli-fixture-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&path, format!("#!/bin/sh\n{}\n", body)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    #[test]
    fn startup_disconnect_is_not_timeout() {
        let path = fixture("exit 7");
        let error = match Session::start(path.to_str().unwrap(), Duration::from_secs(1)) {
            Ok(_) => panic!("unexpected readiness"), Err(error) => error.to_string(),
        };
        std::fs::remove_file(path).unwrap();
        assert!(error.contains("wait=disconnected"), "{}", error);
        assert!(!error.contains("wait=timeout"));
    }
    #[test]
    fn startup_timeout_is_not_disconnect() {
        let path = fixture("sleep 2");
        let error = match Session::start(path.to_str().unwrap(), Duration::from_millis(40)) {
            Ok(_) => panic!("unexpected readiness"), Err(error) => error.to_string(),
        };
        std::fs::remove_file(path).unwrap();
        assert!(error.contains("wait=timeout"), "{}", error);
        assert!(error.contains("child=running"));
    }
    #[test]
    fn diagnostic_stream_counts_without_retaining_secrets() {
        let (tx,rx) = mpsc::sync_channel(2);
        let bytes = Arc::new(AtomicUsize::new(0));
        drain_counted(std::io::Cursor::new(b"private-fixture-no-marker".to_vec()), tx, bytes.clone());
        assert!(rx.recv_timeout(Duration::from_secs(1)).is_err());
        assert_eq!(bytes.load(Ordering::Relaxed), 25);
    }
    #[test]
    fn connect_uses_existing_request_budget_not_a_shorter_clock() {
        let now = Instant::now();
        assert_eq!(connect_budget(now + Duration::from_secs(9), now).unwrap(), Duration::from_secs(9));
    }
    #[test]
    fn queue_time_is_subtracted_from_connect_budget() {
        let start = Instant::now();
        assert_eq!(connect_budget(start + Duration::from_secs(10), start + Duration::from_secs(7)).unwrap(), Duration::from_secs(3));
    }
    #[test]
    fn exhausted_connect_budget_cannot_start_a_session() {
        let now = Instant::now();
        assert!(connect_budget(now, now).is_err());
        assert!(connect_budget(now, now + Duration::from_millis(1)).is_err());
    }
    #[test]
    fn expired_request_cannot_start_player() {
        let (tx,_rx)=mpsc::sync_channel(1);
        let r=Request {owner:"fixture".into(),operation:Operation::Pause,deadline:Instant::now()-Duration::from_secs(1),reply:tx};
        let mut active=None;
        assert!(handle(&mut active,&r).is_err());
        assert!(active.is_none());
    }
    #[test]
    fn release_without_session_is_idempotent() {
        let (tx,_rx)=mpsc::sync_channel(1);
        let r=Request {owner:"fixture".into(),operation:Operation::Release,deadline:Instant::now()+Duration::from_secs(1),reply:tx};
        let mut active=None;
        handle(&mut active,&r).unwrap();
        handle(&mut active,&r).unwrap();
    }
}
