use super::*;
use std::sync::Arc;

#[derive(Clone)]
struct FakeClock(Arc<Mutex<FakeState>>);

struct FakeState {
    enabled: bool,
    fail_show: bool,
    fail_hide: bool,
    fail_before_send: bool,
    calls: Vec<bool>,
}

impl ClockIo for FakeClock {
    fn preferences(&self) -> Result<Preferences> {
        let state = self.0.lock().unwrap();
        Ok(Preferences {
            clock_enabled: state.enabled,
            is_clock_on: state.enabled,
        })
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), DirectiveError> {
        let mut state = self.0.lock().unwrap();
        if !enabled && state.fail_before_send {
            return Err(DirectiveError::Unsent(anyhow!("fixture connection refused")));
        }
        state.calls.push(enabled);
        if !enabled && state.fail_hide {
            return Err(DirectiveError::Unconfirmed(anyhow!(
                "fixture hide acknowledgement timeout"
            )));
        }
        if enabled && state.fail_show {
            return Err(DirectiveError::Unconfirmed(anyhow!("fixture show failure")));
        }
        state.enabled = enabled;
        Ok(())
    }
}

struct Fixture {
    dir: PathBuf,
    io: FakeClock,
    now: Instant,
}

impl Fixture {
    fn new(enabled: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("luna-native-clock-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        Self {
            dir,
            io: FakeClock(Arc::new(Mutex::new(FakeState {
                enabled,
                fail_show: false,
                fail_hide: false,
                fail_before_send: false,
                calls: Vec::new(),
            }))),
            now: Instant::now(),
        }
    }

    fn lease(&self) -> NativeClockLease {
        NativeClockLease::with_io(&self.dir, Box::new(self.io.clone()), self.now).unwrap()
    }

    fn calls(&self) -> Vec<bool> {
        self.io.0.lock().unwrap().calls.clone()
    }
    fn journal(&self) -> PathBuf {
        self.dir.join(JOURNAL)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).unwrap();
    }
}

#[test]
fn clock_commands_match_verified_wire_frames() {
    for (enabled, name) in [(false, b"hide_clock"), (true, b"show_clock")] {
        let mut want = b"\x30\0\0\0\xe2\x0f\x2a\xfa\x05\x27\x0a\x25\x0a\x23\x0a\x13ClockCapabilityType\x12\x0a".to_vec();
        want.extend_from_slice(name);
        want.extend_from_slice(b"\x7a\0\xe8\x0f\x58");
        assert_eq!(packet(enabled), want);
    }
}

#[test]
fn owns_only_the_native_preference_it_hides_and_restores_on_release() {
    let fixture = Fixture::new(true);
    let lease = fixture.lease();
    assert!(lease.refresh_at(fixture.now).unwrap());
    assert!(fixture.journal().is_file());
    assert!(lease
        .refresh_at(fixture.now + Duration::from_secs(1))
        .unwrap());
    assert_eq!(fixture.calls(), vec![false]);
    lease.release().unwrap();
    assert_eq!(fixture.calls(), vec![false, true]);
    assert!(!fixture.journal().exists());
}

#[test]
fn leaves_originally_hidden_clock_hidden() {
    let fixture = Fixture::new(false);
    let lease = fixture.lease();
    assert!(lease.refresh_at(fixture.now).unwrap());
    lease.release().unwrap();
    assert!(fixture.calls().is_empty());
    assert!(!fixture.journal().exists());
}

#[test]
fn refresh_extends_deadline_and_stale_frames_restore_clock() {
    let fixture = Fixture::new(true);
    let lease = fixture.lease();
    lease.refresh_at(fixture.now).unwrap();
    lease
        .refresh_at(fixture.now + Duration::from_secs(2))
        .unwrap();
    lease.poll(fixture.now + Duration::from_millis(4999));
    assert_eq!(fixture.calls(), vec![false]);
    lease.poll(fixture.now + Duration::from_secs(5));
    assert_eq!(fixture.calls(), vec![false, true]);
    assert!(!fixture.journal().exists());
}

#[test]
fn external_show_wins_for_the_current_session() {
    let fixture = Fixture::new(true);
    let lease = fixture.lease();
    lease.refresh_at(fixture.now).unwrap();
    fixture.io.0.lock().unwrap().enabled = true;
    assert!(!lease
        .refresh_at(fixture.now + Duration::from_secs(1))
        .unwrap());
    assert!(!lease
        .refresh_at(fixture.now + Duration::from_secs(2))
        .unwrap());
    assert_eq!(fixture.calls(), vec![false]);
    assert!(!fixture.journal().exists());
    lease.release().unwrap();
    assert!(lease
        .refresh_at(fixture.now + Duration::from_secs(3))
        .unwrap());
    assert_eq!(fixture.calls(), vec![false, false]);
}

#[test]
fn failed_show_keeps_journal_and_retries() {
    let fixture = Fixture::new(true);
    let lease = fixture.lease();
    lease.refresh_at(fixture.now).unwrap();
    fixture.io.0.lock().unwrap().fail_show = true;
    assert!(lease.release_at(fixture.now).is_err());
    assert!(fixture.journal().is_file());
    assert!(!lease.refresh_at(fixture.now).unwrap());
    fixture.io.0.lock().unwrap().fail_show = false;
    lease.poll(fixture.now + Duration::from_secs(2));
    assert_eq!(fixture.calls(), vec![false, true, true]);
    assert!(!fixture.journal().exists());
}

#[test]
fn startup_recovers_interrupted_ownership_but_not_an_unknown_journal() {
    let fixture = Fixture::new(false);
    fs::write(fixture.journal(), JOURNAL_VALUE).unwrap();
    let lease = fixture.lease();
    assert_eq!(fixture.calls(), vec![true]);
    assert!(!fixture.journal().exists());
    drop(lease);
    fs::write(fixture.journal(), b"unexpected").unwrap();
    assert!(
        NativeClockLease::with_io(&fixture.dir, Box::new(fixture.io.clone()), fixture.now).is_err()
    );
    assert_eq!(fixture.calls(), vec![true]);
    assert_eq!(fs::read(fixture.journal()).unwrap(), b"unexpected");
}

#[test]
fn unconfirmed_hide_keeps_marker_until_late_change_is_reconciled() {
    let fixture = Fixture::new(true);
    let lease = fixture.lease();
    fixture.io.0.lock().unwrap().fail_hide = true;
    assert!(lease.refresh_at(fixture.now).is_err());
    assert!(fixture.journal().is_file());
    lease.release_at(fixture.now).unwrap();
    assert_eq!(fixture.calls(), vec![false]);
    assert!(fixture.journal().is_file());
    // The device accepted hide only after the caller gave up on its ACK.
    fixture.io.0.lock().unwrap().enabled = false;
    lease.poll(fixture.now + Duration::from_secs(1));
    assert!(fixture.journal().is_file());
    lease.poll(fixture.now + Duration::from_secs(3));
    assert_eq!(fixture.calls(), vec![false, true]);
    assert!(!fixture.journal().exists());
    assert!(fixture.io.0.lock().unwrap().enabled);
}

#[test]
fn unsent_hide_discards_marker_and_preserves_a_later_native_disable() {
    let fixture = Fixture::new(true);
    let lease = fixture.lease();
    fixture.io.0.lock().unwrap().fail_before_send = true;
    assert!(lease.refresh_at(fixture.now).is_err());
    assert!(!fixture.journal().exists());

    fixture.io.0.lock().unwrap().enabled = false;
    lease.poll(fixture.now + Duration::from_secs(3));
    lease.release().unwrap();
    assert!(fixture.calls().is_empty());
    assert!(!fixture.io.0.lock().unwrap().enabled);
}
