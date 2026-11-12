//! Temporary native clock suppression for qualified Light 2, Mini 3 and Midi boards.
//! Only the two fixed clock directives are sent, on the local IOHub socket.
use anyhow::{anyhow, ensure, Context, Result};
use serde::Deserialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

const PREFERENCES: &str = "/data/quasar/data/clock_display_settings.dat";
const QUASAR_CONFIG: &str = "/system/vendor/quasar/quasar.cfg";
const JOURNAL: &str = "native-clock-lease.json";
const JOURNAL_VALUE: &[u8] = b"{\"version\":1,\"restore_enabled\":true}\n";
pub const CLOCK_FRAME_LEASE: Duration = Duration::from_secs(3);
const VERIFY_INTERVAL: Duration = Duration::from_millis(500);
const ACQUIRE_BACKOFF: Duration = Duration::from_secs(30);
const RESTORE_BACKOFF: Duration = Duration::from_secs(1);
const IO_TIMEOUT: Duration = Duration::from_millis(250);
const ACK_TIMEOUT: Duration = Duration::from_millis(600);
const HIDE_SETTLE: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preferences {
    clock_enabled: bool,
    is_clock_on: bool,
}

impl Preferences {
    fn hidden(&self) -> bool {
        !self.clock_enabled && !self.is_clock_on
    }
}

trait ClockIo: Send + Sync {
    fn preferences(&self) -> Result<Preferences>;
    fn set_enabled(&self, enabled: bool) -> Result<(), DirectiveError>;
}

enum DirectiveError {
    Unsent(anyhow::Error),
    Unconfirmed(anyhow::Error),
}

impl DirectiveError {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Unsent(error) | Self::Unconfirmed(error) => error,
        }
    }
}

struct DeviceIo {
    address: SocketAddr,
}

impl ClockIo for DeviceIo {
    fn preferences(&self) -> Result<Preferences> {
        serde_json::from_slice(&read_bounded(Path::new(PREFERENCES), 4096)?)
            .context("read native clock preferences")
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), DirectiveError> {
        let mut stream = TcpStream::connect_timeout(&self.address, IO_TIMEOUT)
            .context("connect to local clock control")
            .map_err(DirectiveError::Unsent)?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .context("set local clock control write timeout")
            .map_err(DirectiveError::Unsent)?;
        let packet_bytes = packet(enabled);
        let sent = stream
            .write(packet_bytes)
            .context("send native clock directive")
            .map_err(DirectiveError::Unsent)?;
        if sent == 0 {
            return Err(DirectiveError::Unsent(anyhow!(
                "no native clock directive bytes sent"
            )));
        }
        stream
            .write_all(&packet_bytes[sent..])
            .context("finish sending native clock directive")
            .map_err(DirectiveError::Unconfirmed)?;
        let deadline = Instant::now() + ACK_TIMEOUT;
        loop {
            if self
                .preferences()
                .is_ok_and(|p| if enabled { p.clock_enabled } else { p.hidden() })
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                if !enabled {
                    // Order the inverse after any delayed hide on the same
                    // connection. Keep the recovery marker until reconciliation.
                    stream
                        .write_all(packet(true))
                        .context("queue inverse of unconfirmed hide")
                        .map_err(DirectiveError::Unconfirmed)?;
                    stream
                        .shutdown(std::net::Shutdown::Write)
                        .context("finish native clock rollback")
                        .map_err(DirectiveError::Unconfirmed)?;
                }
                return Err(DirectiveError::Unconfirmed(anyhow!(
                    "native clock preference did not acknowledge the directive"
                )));
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "expected a regular clock settings file"
    );
    let mut data = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= limit,
        "clock settings file is too large"
    );
    Ok(data)
}

fn packet(enabled: bool) -> &'static [u8] {
    // LE length, tagged Quasar kind 88, IOControlCommand field 95,
    // ExternalDirectives list, ClockCapabilityType name, empty raw field 15.
    if enabled {
        b"\x30\0\0\0\xe2\x0f\x2a\xfa\x05\x27\x0a\x25\x0a\x23\x0a\x13ClockCapabilityType\x12\x0ashow_clock\x7a\0\xe8\x0f\x58"
    } else {
        b"\x30\0\0\0\xe2\x0f\x2a\xfa\x05\x27\x0a\x25\x0a\x23\x0a\x13ClockCapabilityType\x12\x0ahide_clock\x7a\0\xe8\x0f\x58"
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Session {
    Owned,
    AlreadyHidden,
    Overridden,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Recovery {
    Restore,
    Discard,
}

struct State {
    active: Option<(Session, Instant)>,
    verify_after: Instant,
    acquire_after: Instant,
    restore_after: Instant,
    recovery: Option<Recovery>,
    hide_settle_until: Option<Instant>,
    restore_warning: bool,
}

struct Inner {
    journal: PathBuf,
    io: Box<dyn ClockIo>,
    state: Mutex<State>,
}

/// Owns only a native clock preference that this process actually hid.
/// A persistent marker lets the next Luna instance recover after a crash.
pub struct NativeClockLease {
    inner: Inner,
}

impl NativeClockLease {
    pub fn new(data_dir: &Path) -> Result<Self> {
        let compatible = fs::read("/proc/device-tree/compatible")?;
        ensure!(
            compatible
                .split(|byte| *byte == 0)
                .any(|entry| matches!(entry, b"yandex,plum-evt2" | b"yandex,bergamot" | b"yandex,cucumber")),
            "native clock control is not qualified on this board"
        );
        let config: serde_json::Value =
            serde_json::from_slice(&read_bounded(Path::new(QUASAR_CONFIG), 4 << 20)?)
                .context("read native clock control port")?;
        let port = config["iohub_clients"]["port"]
            .as_u64()
            .and_then(|value| u16::try_from(value).ok())
            .filter(|port| *port != 0)
            .context("iohub_clients port is unavailable")?;
        let io = DeviceIo {
            address: SocketAddr::from(([127, 0, 0, 1], port)),
        };
        Self::with_io(data_dir, Box::new(io), Instant::now())
    }

    fn with_io(data_dir: &Path, io: Box<dyn ClockIo>, now: Instant) -> Result<Self> {
        let journal = data_dir.join(JOURNAL);
        let restore_pending = match read_bounded(&journal, 128) {
            Ok(data) => {
                ensure!(
                    data == JOURNAL_VALUE,
                    "invalid native clock recovery marker"
                );
                true
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                false
            }
            Err(error) => return Err(error).context("read native clock recovery marker"),
        };
        let hide_settle_until = (restore_pending && !io.preferences().is_ok_and(|p| p.hidden()))
            .then_some(now + HIDE_SETTLE);
        let lease = Self {
            inner: Inner {
                journal,
                io,
                state: Mutex::new(State {
                    active: None,
                    verify_after: now,
                    acquire_after: now,
                    restore_after: now,
                    recovery: restore_pending.then_some(Recovery::Restore),
                    hide_settle_until,
                    restore_warning: false,
                }),
            },
        };
        lease.inner.tick(now);
        Ok(lease)
    }

    pub fn acquire_or_refresh(&self) -> Result<bool> {
        self.refresh_at(Instant::now())
    }

    fn refresh_at(&self, now: Instant) -> Result<bool> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((mut session, _)) = state.active {
            if session != Session::Overridden && now >= state.verify_after {
                state.verify_after = now + VERIFY_INTERVAL;
                if self.inner.io.preferences()?.clock_enabled {
                    // The user or the firmware explicitly showed time again.
                    // Do not hide it again in this same playback session.
                    session = Session::Overridden;
                    state.active = Some((session, now));
                    self.inner.restore(&mut state, now, true)?;
                }
            }
            state.active = Some((session, now));
            return Ok(session != Session::Overridden);
        }
        if state.recovery.is_some() || now < state.acquire_after {
            return Ok(false);
        }
        let result = self.inner.acquire(&mut state, now);
        if let Err(error) = &result {
            state.acquire_after = now + ACQUIRE_BACKOFF;
            tracing::warn!(%error, "native clock suppression unavailable; using cooperative display");
            if state.recovery == Some(Recovery::Restore) {
                state.hide_settle_until = Some(now + IO_TIMEOUT + ACK_TIMEOUT + HIDE_SETTLE);
            }
        }
        result
    }

    pub fn release(&self) -> Result<()> {
        self.release_at(Instant::now())
    }

    fn release_at(&self, now: Instant) -> Result<()> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let active = state.active.take().is_some();
        self.inner.restore(&mut state, now, active)
    }

    pub fn poll(&self, now: Instant) {
        self.inner.tick(now);
    }
}

impl Drop for NativeClockLease {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active = None;
        let _ = self.inner.restore(&mut state, Instant::now(), true);
    }
}

impl Inner {
    fn acquire(&self, state: &mut State, now: Instant) -> Result<bool> {
        let preferences = self.io.preferences()?;
        let session = if preferences.hidden() {
            Session::AlreadyHidden
        } else if preferences.clock_enabled && preferences.is_clock_on {
            self.save_journal()?;
            state.recovery = Some(Recovery::Restore);
            match self.io.set_enabled(false) {
                Ok(()) => (),
                Err(DirectiveError::Unsent(error)) => {
                    // Retrying journal removal must never send a show directive.
                    state.recovery = Some(Recovery::Discard);
                    let _ = self.restore(state, now, true);
                    return Err(error);
                }
                Err(DirectiveError::Unconfirmed(error)) => return Err(error),
            }
            Session::Owned
        } else {
            // A transient state (e.g. DND or Alice's last frame) is not an
            // ownership baseline. Recheck soon without sending a directive.
            state.acquire_after = now + VERIFY_INTERVAL;
            return Ok(false);
        };
        state.active = Some((session, now));
        state.verify_after = now + VERIFY_INTERVAL;
        Ok(true)
    }

    fn tick(&self, now: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let stale = state
            .active
            .is_some_and(|(_, last)| now.saturating_duration_since(last) >= CLOCK_FRAME_LEASE);
        if stale {
            state.active = None;
        }
        if state.active.is_none() {
            let _ = self.restore(&mut state, now, stale);
        }
    }

    fn restore(&self, state: &mut State, now: Instant, force: bool) -> Result<()> {
        let Some(recovery) = state.recovery else {
            return Ok(());
        };
        if (!force && now < state.restore_after)
            || state.hide_settle_until.is_some_and(|until| now < until) {
            return Ok(());
        }
        state.restore_after = now + RESTORE_BACKOFF;
        let result = (|| {
            if recovery == Recovery::Restore {
                let preferences = self.io.preferences()?;
                if !preferences.clock_enabled {
                    ensure!(preferences.hidden(), "native clock state is inconsistent");
                }
                if !preferences.clock_enabled || state.hide_settle_until.is_some() {
                    self.io
                        .set_enabled(true)
                        .map_err(DirectiveError::into_error)?;
                }
            }
            match fs::remove_file(&self.journal) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error).context("remove native clock recovery marker"),
            }
            File::open(
                self.journal
                    .parent()
                    .context("missing clock recovery directory")?,
            )?
            .sync_all()?;
            Ok(())
        })();
        if let Err(error) = &result {
            if !state.restore_warning {
                tracing::warn!(%error, "native clock restore pending; will retry");
                state.restore_warning = true;
            }
        } else {
            state.recovery = None;
            state.hide_settle_until = None;
            state.restore_warning = false;
        }
        result
    }

    fn save_journal(&self) -> Result<()> {
        ensure!(
            !self.journal.try_exists()?,
            "native clock recovery marker already exists"
        );
        let dir = self
            .journal
            .parent()
            .context("missing clock recovery directory")?;
        let temporary = dir.join(format!(".native-clock-{}", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let result = (|| {
            file.write_all(JOURNAL_VALUE)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.journal)?;
            if let Err(error) = File::open(dir).and_then(|file| file.sync_all()) {
                let _ = fs::remove_file(&self.journal);
                return Err(error).context("persist native clock recovery marker");
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
#[path = "native_clock_test.rs"]
mod tests;
