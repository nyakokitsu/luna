use anyhow::{bail, Context, Result};
use luna::{ClockBackend, ClockCommand, ClockMusicMode, ClockMusicPolicy, NativeClockLease, CLOCK_FRAME_LEASE};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

const ROOT: &str = "/sys/class/leds";
const CLOCK_SETTINGS: &str = "/data/quasar/data/clock_display_settings.dat";
const CLOCK_SETTINGS_MAX_BYTES: u64 = 4096;
const HIDDEN_CLOCK_SETTLE: Duration = Duration::from_millis(150);
const NATIVE_DISPLAY_RETRY: Duration = Duration::from_secs(1);
/// Hardware geometry is detected from actual segment files, not firmware versions.
#[derive(Clone, Copy)]
pub enum Layout { Lite2, Mini3, Midi }

impl Layout {
    fn prefix(self) -> &'static str {
        match self {
            Self::Lite2 => "plum:white:clock",
            Self::Mini3 => "bergamot:white:clock",
            Self::Midi => "cucumber:white:clock-",
        }
    }

    fn parts(self) -> &'static [&'static str] {
        match self {
            Self::Lite2 => &["A", "B", "C", "D", "E", "F", "G", "H"],
            Self::Mini3 | Self::Midi => &["A", "B", "C", "D", "E", "F", "G"],
        }
    }
    fn digit_base(self) -> usize {
        usize::from(matches!(self, Self::Midi))
    }

    fn extra_segments(self) -> &'static [&'static str] {
        match self {
            Self::Midi => &["PLUS1", "PLUS2", "DEGREE", "H1", "H2", "L1", "L2",
                "M1", "M2", "P1", "P2", "P4", "P5", "Q1", "Q2", "Q4", "Q5"],
            _ => &["DEGREE", "DH", "DM", "DL"],
        }
    }

    fn colon(self) -> [&'static str; 2] {
        match self {
            Self::Midi => ["PLUS1", "PLUS2"],
            _ => ["DH", "DL"],
        }
    }

    fn segment_path(self, root: &Path, segment: &str) -> PathBuf {
        let physical = match (self, segment) {
            (Self::Midi, "PLUS1") => "p3".into(),
            (Self::Midi, "PLUS2") => "q3".into(),
            (Self::Midi, "DEGREE") => "deg".into(),
            (Self::Midi, name) => name.to_ascii_lowercase(),
            (_, name) => name.to_owned(),
        };
        root.join(format!("{}{physical}/brightness", self.prefix()))
    }
}

pub fn create_clock_backend(layout: Layout) -> Option<Box<dyn ClockBackend>> {
    let clock = Clock::detect(Path::new(ROOT), layout)?;
    Some(Box::new(clock))
}

/// Four digits, positions numbered from 0, model-specific segments plus the
/// degree marker and the three separator dots between the digit pairs.
fn segment_names(layout: Layout) -> Vec<String> {
    let mut names = Vec::with_capacity(layout.parts().len() * 4 + layout.extra_segments().len());
    for position in layout.digit_base()..layout.digit_base() + 4 {
        for part in layout.parts() {
            names.push(format!("{part}{position}"));
        }
    }
    names.extend(layout.extra_segments().iter().copied().map(String::from));
    names
}

struct FrameOwnership {
    system: Vec<(PathBuf, String)>,
    ours: Vec<(PathBuf, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MusicPhase { Wave, Clock }

struct MusicCycle {
    policy: ClockMusicPolicy,
    phase: MusicPhase,
    started: Instant,
    last_frame: Instant,
}

#[derive(Default)]
struct ClockState {
    ownership: Option<FrameOwnership>,
    yielding_since: Option<Instant>,
    yielding_blank_since: Option<Instant>,
    music: Option<MusicCycle>,
    native_retry_after: Option<Instant>,
}

impl ClockState {
    fn clear_yield(&mut self) {
        self.yielding_since = None;
        self.yielding_blank_since = None;
    }

    fn begin_yield(&mut self, now: Instant) {
        self.yielding_since = Some(now);
        self.yielding_blank_since = None;
    }
}

trait NativeControl: Send + Sync {
    fn refresh(&self) -> Result<bool>;
    fn release(&self) -> Result<()>;
    fn poll(&self, _now: Instant) {}
}

impl NativeControl for NativeClockLease {
    fn refresh(&self) -> Result<bool> { self.acquire_or_refresh() }
    fn release(&self) -> Result<()> { NativeClockLease::release(self) }
    fn poll(&self, now: Instant) { NativeClockLease::poll(self, now); }
}

struct Clock {
    layout: Layout,
    paths: Vec<(String, PathBuf)>,
    clock_settings_path: PathBuf,
    latches: Vec<PathBuf>,
    native_control: Mutex<Option<Box<dyn NativeControl>>>,
    state: Mutex<ClockState>,
}

impl Clock {
    fn detect(root: &Path, layout: Layout) -> Option<Self> {
        Self::detect_with_settings_path(root, layout, PathBuf::from(CLOCK_SETTINGS))
    }

    fn detect_with_settings_path(root: &Path, layout: Layout, clock_settings_path: PathBuf) -> Option<Self> {
        let paths = segment_names(layout)
            .into_iter()
            .map(|name| {
                let path = layout.segment_path(root, &name);
                (name, path)
            })
            .collect::<Vec<_>>();
        let latches = if matches!(layout, Layout::Midi) {
            fs::read_dir(root).ok()?.flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("cucumber:sync:clock-"))
                .map(|entry| entry.path().join("brightness"))
                .collect()
        } else {
            Vec::new()
        };
        paths.iter().all(|(_, path)| path.is_file()).then(|| Self {
            layout,
            paths,
            clock_settings_path,
            latches,
            native_control: Mutex::new(None),
            state: Mutex::new(ClockState::default()),
        })
    }

    fn native_clock_hidden(&self) -> bool {
        if !fs::symlink_metadata(&self.clock_settings_path).is_ok_and(|metadata| metadata.is_file()) {
            return false;
        }
        let Ok(file) = fs::File::open(&self.clock_settings_path) else { return false };
        let mut bytes = Vec::new();
        if file.take(CLOCK_SETTINGS_MAX_BYTES + 1).read_to_end(&mut bytes).is_err()
            || bytes.len() as u64 > CLOCK_SETTINGS_MAX_BYTES {
            return false;
        }
        serde_json::from_slice::<serde_json::Value>(&bytes).is_ok_and(|value| {
            value["clockEnabled"].as_bool() == Some(false)
                && value["isClockOn"].as_bool() == Some(false)
        })
    }

    fn refresh_native_control(&self) {
        if let Some(control) = self.native_control.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            // A missing acknowledgement leaves the existing cooperative path
            // usable. The controller owns backoff and pending restoration.
            let _ = control.refresh();
        }
    }

    fn release_native_control(&self) -> Result<()> {
        if let Some(control) = self.native_control.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            control.release()?;
        }
        Ok(())
    }

    fn unlatch(&self) -> Result<()> {
        for path in &self.latches {
            if fs::read_to_string(path)?.trim() != "0" {
                fs::write(path, "0")?;
            }
        }
        Ok(())
    }

    fn read(&self) -> Result<Vec<(PathBuf, String)>> {
        self.paths
            .iter()
            .map(|(name, path)| {
                let value = fs::read_to_string(path)
                    .with_context(|| format!("read clock segment {name}"))?;
                Ok((path.clone(), value.trim().to_string()))
            })
            .collect()
    }

    fn validate(&self, frame: &HashMap<String, u8>) -> Result<()> {
        if let Some(name) = frame.keys().find(|name| !self.paths.iter().any(|(known, _)| known == *name)) {
            bail!("unknown clock segment {name:?}");
        }
        Ok(())
    }

    fn write(&self, frame: &HashMap<String, u8>) -> Result<()> {
        self.validate(frame)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.music = None;
        state.clear_yield();
        let _ = self.release_native_control();
        let current = self.read()?;
        self.paint_owned(&mut state, current, frame, false)
    }

    fn paint_owned(
        &self,
        state: &mut ClockState,
        current: Vec<(PathBuf, String)>,
        frame: &HashMap<String, u8>,
        skip_unchanged: bool,
    ) -> Result<()> {
        let already_owned = state.ownership.as_ref().is_some_and(|owned| owned.ours == current);
        if skip_unchanged && already_owned && self.paths.iter().zip(&current).all(|((name, _), (_, value))| {
            value.parse::<u8>() == Ok(frame.get(name).copied().unwrap_or(0))
        }) {
            return self.unlatch();
        }
        if !already_owned {
            state.ownership = Some(FrameOwnership { system: current, ours: Vec::new() });
        }
        self.unlatch()?;
        for (name, path) in &self.paths {
            let value = frame.get(name).copied().unwrap_or(0);
            fs::write(path, value.to_string())
                .with_context(|| format!("write clock segment {name}"))?;
        }
        if let Some(owned) = state.ownership.as_mut() {
            // Record our intended frame, never adopt a concurrent native repaint.
            owned.ours = self.paths.iter().map(|(name, path)| {
                (path.clone(), frame.get(name).copied().unwrap_or(0).to_string())
            }).collect();
        }
        Ok(())
    }

    fn write_music_at(&self, frame: &HashMap<String, u8>, policy: ClockMusicPolicy, now: Instant) -> Result<()> {
        policy.validate()?;
        self.validate(frame)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let result = self.write_music_frame(&mut state, frame, policy, now);
        if result.is_ok() {
            if let Some(cycle) = state.music.as_mut() {
                cycle.last_frame = now;
            }
        }
        if result.is_err() && policy.mode == ClockMusicMode::Wave {
            // Retries from the renderer must not keep time hidden while the
            // segment backend cannot read or draw a frame.
            state.native_retry_after = Some(now + NATIVE_DISPLAY_RETRY);
            let _ = self.release_native_control();
        }
        result
    }

    fn write_music_frame(&self, state: &mut ClockState, frame: &HashMap<String, u8>, policy: ClockMusicPolicy, now: Instant) -> Result<()> {
        if policy.mode == ClockMusicMode::Clock {
            return self.release_music(state);
        }
        let new_policy = state.music.as_ref().is_none_or(|cycle| cycle.policy != policy);
        if new_policy {
            state.clear_yield();
            state.music = Some(MusicCycle { policy, phase: MusicPhase::Wave, started: now, last_frame: now });
        }
        if policy.mode == ClockMusicMode::Wave {
            if state.native_retry_after.is_none_or(|retry| now >= retry) {
                self.refresh_native_control();
            }
        } else {
            let _ = self.release_native_control();
        }
        let current = self.read()?;
        let foreign = state.ownership.as_ref().is_none_or(|owned| owned.ours != current);
        let clock_visible = foreign && self.is_clock(&current);
        let volume_visible = foreign && !clock_visible && self.is_volume(&current);
        if let Some(since) = state.yielding_since {
            if volume_visible {
                state.begin_yield(now);
                return Ok(());
            }
            // Hidden time cannot provide an HH:MM handoff after an overlay or
            // a firmware repaint. A stable blank is sufficient in wave mode.
            let blank_settled = if policy.mode == ClockMusicMode::Wave
                && current.iter().all(|(_, value)| value == "0") {
                let start = *state.yielding_blank_since.get_or_insert(now);
                now.saturating_duration_since(start) >= HIDDEN_CLOCK_SETTLE && self.native_clock_hidden()
            } else {
                state.yielding_blank_since = None;
                false
            };
            if !clock_visible && !blank_settled && now.saturating_duration_since(since) < Duration::from_secs(5) {
                return Ok(());
            }
            state.clear_yield();
            if clock_visible && policy.mode == ClockMusicMode::Alternating {
                state.music.as_mut().unwrap().phase = MusicPhase::Clock;
                state.music.as_mut().unwrap().started = now;
                return Ok(());
            }
        } else if foreign && (volume_visible
            || (!clock_visible && (state.ownership.is_some()
                || state.music.as_ref().unwrap().phase == MusicPhase::Clock))) {
            state.begin_yield(now);
            state.ownership = None;
            return Ok(());
        }
        if policy.mode == ClockMusicMode::Alternating {
            let cycle = state.music.as_ref().unwrap();
            if cycle.phase == MusicPhase::Wave && !new_policy && foreign && clock_visible {
                state.ownership = None;
                state.music.as_mut().unwrap().phase = MusicPhase::Clock;
                state.music.as_mut().unwrap().started = now;
                return Ok(());
            }
            let elapsed = now.saturating_duration_since(cycle.started);
            match cycle.phase {
                MusicPhase::Wave if elapsed >= Duration::from_secs(u64::from(policy.wave_seconds)) => {
                    self.release_owned(state)?;
                    state.music.as_mut().unwrap().phase = MusicPhase::Clock;
                    state.music.as_mut().unwrap().started = now;
                    return Ok(());
                }
                MusicPhase::Clock if elapsed < Duration::from_secs(u64::from(policy.time_seconds)) => return Ok(()),
                MusicPhase::Clock => {
                    state.music.as_mut().unwrap().phase = MusicPhase::Wave;
                    state.music.as_mut().unwrap().started = now;
                }
                MusicPhase::Wave => {}
            }
        }
        self.paint_owned(state, current, frame, true)
    }

    /// Gives the display back to the firmware, but only when it still shows the
    /// frame we painted. platformd redraws the clock once a minute, so an
    /// external repaint wins and must not be overwritten.
    fn release(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.release_music(&mut state)
    }

    fn release_music(&self, state: &mut ClockState) -> Result<()> {
        state.clear_yield();
        state.music = None;
        let frame_result = self.release_owned(state);
        let control_result = self.release_native_control();
        frame_result.and(control_result)
    }

    fn release_owned(&self, state: &mut ClockState) -> Result<()> {
        let Some(owned) = state.ownership.take() else {
            return Ok(());
        };
        if self.read()? != owned.ours {
            return Ok(());
        }
        self.unlatch()?;
        for (path, value) in owned.system {
            fs::write(&path, value)
                .with_context(|| format!("restore clock segment {}", path.display()))?;
        }
        Ok(())
    }

    fn lit_segments(&self, frame: &[(PathBuf, String)]) -> Option<HashSet<String>> {
        self.paths
            .iter()
            .zip(frame)
            .try_fold(HashSet::new(), |mut lit, ((name, _), (_, value))| {
                if value.parse::<u8>().ok()? > 0 {
                    lit.insert(name.clone());
                }
                Some(lit)
            })
    }

    fn digits(&self, lit: &HashSet<String>) -> Option<[Option<u8>; 4]> {
        let mut digits = [None; 4];
        for (position, digit) in digits.iter_mut().enumerate() {
            let parts = self.layout.parts()
                .iter().copied()
                .filter(|part| lit.contains(&format!("{part}{}", position + self.layout.digit_base())))
                .collect::<HashSet<_>>();
            if parts.is_empty() {
                continue;
            }
            *digit = Some((0..=9).find(|value| {
                glyph(b'0' + value).iter().copied().collect::<HashSet<_>>() == parts
            })?);
        }
        Some(digits)
    }

    fn is_volume(&self, frame: &[(PathBuf, String)]) -> bool {
        let Some(lit) = self.lit_segments(frame) else {
            return false;
        };
        if self.layout.extra_segments().iter().any(|name| lit.contains(*name)) {
            return false;
        }
        let Some(digits) = self.digits(&lit) else {
            return false;
        };
        let occupied = digits
            .iter()
            .enumerate()
            .filter_map(|(index, value)| value.map(|_| index))
            .collect::<Vec<_>>();
        let Some(&first) = occupied.first() else {
            return false;
        };
        let last = *occupied.last().unwrap();
        occupied.len() <= 3
            && (first == 0 || last == 3)
            && last - first + 1 == occupied.len()
            && digits[first..=last]
                .iter()
                .flatten()
                .fold(0u16, |value, digit| value * 10 + u16::from(*digit))
                <= 100
    }

    fn is_clock(&self, frame: &[(PathBuf, String)]) -> bool {
        let Some(lit) = self.lit_segments(frame) else {
            return false;
        };
        let colon = self.layout.colon();
        if !colon.iter().all(|part| lit.contains(*part))
            || self.layout.extra_segments().iter().any(|part| !colon.contains(part) && lit.contains(*part))
        {
            return false;
        }
        matches!(self.digits(&lit), Some([a, Some(b), Some(c), Some(d)]) if a.unwrap_or(0) * 10 + b < 24 && c * 10 + d < 60)
    }

    fn time_frame_for_layout(&self, text: &str, brightness: u8) -> Result<HashMap<String, u8>> {
        match self.layout {
            Layout::Lite2 => Self::time_frame(text, brightness),
            Layout::Midi => Self::strict_time_frame(text, brightness, Layout::Midi),
            Layout::Mini3 => {
                // Preserve Mini3's existing short-hour, space and dash inputs.
                let (left, right) = text.split_once(':')
                    .context("clock time must look like HH:MM")?;
                if left.len() > 2 || right.len() != 2 {
                    bail!("clock time must look like HH:MM");
                }
                let mut frame = HashMap::new();
                for (position, ch) in format!("{left:>2}{right}").chars().enumerate() {
                    let parts = match ch {
                        '0'..='9' => glyph(ch as u8),
                        '-' => &["G"],
                        ' ' => &[],
                        _ => bail!("clock supports digits, '-' and spaces"),
                    };
                    for part in parts {
                        frame.insert(format!("{part}{position}"), brightness);
                    }
                }
                frame.insert("DH".into(), brightness);
                frame.insert("DL".into(), brightness);
                Ok(frame)
            }
        }
    }

    fn time_frame(text: &str, brightness: u8) -> Result<HashMap<String, u8>> {
        Self::strict_time_frame(text, brightness, Layout::Lite2)
    }

    fn strict_time_frame(text: &str, brightness: u8, layout: Layout) -> Result<HashMap<String, u8>> {
        let bytes = text.as_bytes();
        if bytes.len() != 5
            || bytes[2] != b':'
            || ![bytes[0], bytes[1], bytes[3], bytes[4]]
                .iter()
                .all(u8::is_ascii_digit)
        {
            bail!("clock time must have HH:MM format");
        }
        let mut frame = HashMap::new();
        for (position, digit) in [bytes[0], bytes[1], bytes[3], bytes[4]].iter().enumerate() {
            for part in glyph(*digit) {
                frame.insert(format!("{part}{}", position + layout.digit_base()), brightness);
            }
        }
        for part in layout.colon() {
            frame.insert(part.into(), brightness);
        }
        Ok(frame)
    }
}

fn glyph(digit: u8) -> &'static [&'static str] {
    match digit {
        b'0' => &["A", "B", "C", "D", "E", "F"],
        b'1' => &["B", "C"],
        b'2' => &["A", "B", "D", "E", "G"],
        b'3' => &["A", "B", "C", "D", "G"],
        b'4' => &["B", "C", "F", "G"],
        b'5' => &["A", "C", "D", "F", "G"],
        b'6' => &["A", "C", "D", "E", "F", "G"],
        b'7' => &["A", "B", "C"],
        b'8' => &["A", "B", "C", "D", "E", "F", "G"],
        b'9' => &["A", "B", "C", "D", "F", "G"],
        _ => &[],
    }
}

impl ClockBackend for Clock {
    fn segments(&self) -> Vec<String> {
        self.paths.iter().map(|(name, _)| name.clone()).collect()
    }

    fn music_policy_available(&self) -> bool { true }

    fn text_available(&self) -> bool { matches!(self.layout, Layout::Midi) }

    fn configure_native_control(&self, data_dir: &Path) -> Result<()> {
        let lease = NativeClockLease::new(data_dir)?;
        *self.native_control.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(lease));
        Ok(())
    }

    fn native_control_available(&self) -> bool {
        self.native_control.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn poll_clock(&self, now: Instant) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let stale = state.music.as_ref().is_some_and(|cycle| {
            now.saturating_duration_since(cycle.last_frame) >= CLOCK_FRAME_LEASE
        });
        let result = if stale {
            // Under the frame lock, restore segments before re-enabling time.
            self.release_music(&mut state)
        } else {
            Ok(())
        };
        if let Some(control) = self.native_control.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            control.poll(now);
        }
        result
    }

    fn apply_music(&self, command: ClockCommand, policy: ClockMusicPolicy) -> Result<()> {
        policy.validate()?;
        match command {
            ClockCommand::Segments(values) => self.write_music_at(&values, policy, Instant::now()),
            ClockCommand::Off => self.release(),
            ClockCommand::Time { .. } => bail!("clock music requires segments, not time"),
        }
    }

    fn apply(&self, command: ClockCommand) -> Result<()> {
        match command {
            ClockCommand::Off => self.release(),
            ClockCommand::Segments(values) => self.write(&values),
            ClockCommand::Time { text, brightness } => {
                self.write(&self.time_frame_for_layout(&text, brightness)?)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    macro_rules! shared_tests { () => {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        clock: Clock,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "luna-segment-clock-{}-{}-{}",
                LAYOUT.prefix(),
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            for name in segment_names(LAYOUT) {
                let dir = root.join(format!("{}{name}", LAYOUT.prefix()));
                fs::create_dir(&dir).unwrap();
                fs::write(dir.join("brightness"), "0").unwrap();
            }
            let clock = Clock::detect_with_settings_path(
                &root,
                LAYOUT,
                root.join("clock_display_settings.dat"),
            ).unwrap();
            Self { root, clock }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(format!("{}{name}/brightness", LAYOUT.prefix()))
        }

        fn set(&self, name: &str, value: &str) {
            fs::write(self.path(name), value).unwrap();
        }

        fn get(&self, name: &str) -> String {
            fs::read_to_string(self.path(name)).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    fn paint(fixture: &Fixture, frame: &HashMap<String, u8>) {
        for name in segment_names(LAYOUT) {
            fixture.set(&name, &frame.get(&name).copied().unwrap_or(0).to_string());
        }
    }

    fn volume(text: &str, start: usize) -> HashMap<String, u8> {
        let mut frame = HashMap::new();
        for (position, digit) in text.bytes().enumerate() {
            for part in glyph(digit) {
                frame.insert(format!("{part}{}", start + position), 170);
            }
        }
        frame
    }

    fn wave() -> ClockCommand {
        ClockCommand::Segments(HashMap::from([("A0".into(), 100)]))
    }

    #[test]
    fn music_wave_yields_until_firmware_clock_and_restores_latest_clock() {
        let fixture = Fixture::new();
        paint(&fixture, &Clock::time_frame("12:34", 170).unwrap());
        music(&fixture, wave_policy(), Instant::now());
        paint(&fixture, &volume("15", 2));
        let native = fixture.clock.read().unwrap();
        for _ in 0..4 {
            music(&fixture, wave_policy(), Instant::now());
            assert_eq!(fixture.clock.read().unwrap(), native);
        }
        paint(&fixture, &HashMap::new());
        music(&fixture, wave_policy(), Instant::now());
        assert_eq!(fixture.get("A0"), "0");
        paint(&fixture, &Clock::time_frame("12:35", 170).unwrap());
        let latest = fixture.clock.read().unwrap();
        music(&fixture, wave_policy(), Instant::now());
        assert_eq!(fixture.get("A0"), "100");
        fixture.clock.apply(ClockCommand::Off).unwrap();
        assert_eq!(fixture.clock.read().unwrap(), latest);
    }

    #[test]
    fn music_partial_native_repaint_yields_and_off_preserves_it() {
        let fixture = Fixture::new();
        paint(&fixture, &Clock::time_frame("12:34", 170).unwrap());
        music(&fixture, wave_policy(), Instant::now());
        paint(&fixture, &HashMap::from([("B3".into(), 2)]));
        let partial = fixture.clock.read().unwrap();
        assert!(!fixture.clock.is_volume(&partial));
        assert!(!fixture.clock.is_clock(&partial));
        music(&fixture, wave_policy(), Instant::now());
        assert_eq!(fixture.clock.read().unwrap(), partial);
        {
            let state = fixture.clock.state.lock().unwrap();
            assert!(state.ownership.is_none());
            assert!(state.yielding_since.is_some());
        }
        fixture.clock.apply(ClockCommand::Off).unwrap();
        assert_eq!(fixture.clock.read().unwrap(), partial);
    }

    fn wave_policy() -> ClockMusicPolicy {
        ClockMusicPolicy { mode: ClockMusicMode::Wave, ..ClockMusicPolicy::default() }
    }

    fn music(fixture: &Fixture, policy: ClockMusicPolicy, now: Instant) {
        let ClockCommand::Segments(frame) = wave() else { unreachable!() };
        fixture.clock.write_music_at(&frame, policy, now).unwrap();
    }

    fn set_clock_hidden(fixture: &Fixture) {
        fs::write(
            fixture.clock.clock_settings_path.as_path(),
            br#"{"clockEnabled":false,"isClockOn":false}"#,
        ).unwrap();
    }

    #[test]
    fn stale_music_releases_its_frame_and_preserves_firmware_repaints() {
        for mode in [ClockMusicMode::Wave, ClockMusicMode::Alternating] {
            for firmware_repaints in [false, true] {
                let fixture = Fixture::new();
                set_clock_hidden(&fixture);
                let now = Instant::now();
                let policy = ClockMusicPolicy { mode, ..ClockMusicPolicy::default() };
                let before = fixture.clock.read().unwrap();
                music(&fixture, policy, now);
                music(&fixture, policy, now + Duration::from_secs(2));
                fixture.clock.poll_clock(now + Duration::from_secs(3)).unwrap();
                assert_eq!(fixture.get("A0"), "100", "recent frame lost ownership");

                let expected = if firmware_repaints {
                    paint(&fixture, &volume("15", 2));
                    fixture.clock.read().unwrap()
                } else {
                    before
                };
                fixture.clock.poll_clock(now + Duration::from_secs(5)).unwrap();
                assert_eq!(fixture.clock.read().unwrap(), expected, "stale music frame");
                assert!(fixture.clock.native_clock_hidden());

                music(&fixture, policy, now + Duration::from_secs(6));
                fixture.clock.apply(ClockCommand::Segments(HashMap::from([("B0".into(), 42)]))).unwrap();
                fixture.clock.poll_clock(now + Duration::from_secs(9)).unwrap();
                assert_eq!(fixture.get("B0"), "42", "manual frame expired with music");
            }
        }
    }

    fn begin_hidden_volume_yield(fixture: &Fixture, policy: ClockMusicPolicy, now: Instant) {
        paint(fixture, &volume("15", 2));
        music(fixture, policy, now);
        assert_eq!(fixture.get("C2"), "170");
    }

    #[test]
    fn wave_music_resumes_after_hidden_native_volume_and_stable_blank() {
        let fixture = Fixture::new();
        set_clock_hidden(&fixture);
        let now = Instant::now();
        let policy = ClockMusicPolicy { mode: ClockMusicMode::Wave, ..ClockMusicPolicy::default() };
        begin_hidden_volume_yield(&fixture, policy, now);
        paint(&fixture, &HashMap::new());
        music(&fixture, policy, now + Duration::from_millis(10));
        assert_eq!(fixture.get("A0"), "0", "blank must settle before resuming");
        music(&fixture, policy, now + Duration::from_millis(159));
        assert_eq!(fixture.get("A0"), "0", "blank must remain stable for 150ms");
        music(&fixture, policy, now + Duration::from_millis(160));
        assert_eq!(fixture.get("A0"), "100");
    }

    #[test]
    fn hidden_wave_keeps_yielding_while_volume_is_visible_and_partial_resets_settle() {
        let fixture = Fixture::new();
        set_clock_hidden(&fixture);
        let now = Instant::now();
        let policy = ClockMusicPolicy { mode: ClockMusicMode::Wave, ..ClockMusicPolicy::default() };
        begin_hidden_volume_yield(&fixture, policy, now);
        music(&fixture, policy, now + Duration::from_secs(10));
        assert_eq!(fixture.get("C2"), "170", "visible volume must always win");

        paint(&fixture, &HashMap::new());
        music(&fixture, policy, now + Duration::from_secs(11));
        paint(&fixture, &HashMap::from([("B3".into(), 2)]));
        music(&fixture, policy, now + Duration::from_millis(11150));
        assert_eq!(fixture.get("B3"), "2", "partial frame must reset blank settle");
        paint(&fixture, &HashMap::new());
        music(&fixture, policy, now + Duration::from_millis(11200));
        assert_eq!(fixture.get("A0"), "0");
        music(&fixture, policy, now + Duration::from_millis(11349));
        assert_eq!(fixture.get("A0"), "0");
        music(&fixture, policy, now + Duration::from_millis(11350));
        assert_eq!(fixture.get("A0"), "100");
    }

    #[test]
    fn hidden_wave_keeps_five_second_fallback_when_settings_are_unavailable_or_enabled() {
        for (name, settings) in [
            ("missing", None),
            ("corrupt", Some(b"{".as_slice())),
            ("clock_enabled", Some(br#"{"clockEnabled":true,"isClockOn":false}"#.as_slice())),
            ("clock_on", Some(br#"{"clockEnabled":false,"isClockOn":true}"#.as_slice())),
        ] {
            let fixture = Fixture::new();
            if let Some(settings) = settings {
                fs::write(&fixture.clock.clock_settings_path, settings).unwrap();
            }
            let now = Instant::now();
            let policy = ClockMusicPolicy { mode: ClockMusicMode::Wave, ..ClockMusicPolicy::default() };
            begin_hidden_volume_yield(&fixture, policy, now);
            paint(&fixture, &HashMap::new());
            music(&fixture, policy, now + Duration::from_millis(10));
            music(&fixture, policy, now + Duration::from_millis(160));
            assert_eq!(fixture.get("A0"), "0", "{name} settings must retain fallback");
            music(&fixture, policy, now + Duration::from_millis(4999));
            assert_eq!(fixture.get("A0"), "0", "{name} settings must retain five-second wait");
            music(&fixture, policy, now + Duration::from_secs(5));
            assert_eq!(fixture.get("A0"), "100", "{name} settings must retain bounded fallback");
        }
    }

    #[test]
    fn hidden_wave_recovers_from_a_blank_repaint_but_alternating_keeps_the_fallback() {
        let fixture = Fixture::new();
        set_clock_hidden(&fixture);
        let now = Instant::now();
        let wave_policy = wave_policy();
        music(&fixture, wave_policy, now);
        paint(&fixture, &HashMap::from([("B3".into(), 2)]));
        music(&fixture, wave_policy, now + Duration::from_millis(10));
        paint(&fixture, &HashMap::new());
        music(&fixture, wave_policy, now + Duration::from_millis(20));
        music(&fixture, wave_policy, now + Duration::from_millis(169));
        assert_eq!(fixture.get("A0"), "0");
        music(&fixture, wave_policy, now + Duration::from_millis(170));
        assert_eq!(fixture.get("A0"), "100", "a blank repaint must not trigger a five-second pause");

        let fixture = Fixture::new();
        set_clock_hidden(&fixture);
        let now = Instant::now();
        let alternating = ClockMusicPolicy::default();
        begin_hidden_volume_yield(&fixture, alternating, now);
        paint(&fixture, &HashMap::new());
        music(&fixture, alternating, now + Duration::from_millis(10));
        music(&fixture, alternating, now + Duration::from_millis(160));
        assert_eq!(fixture.get("A0"), "0", "alternating mode must retain fallback");
    }

    struct TestNativeControl {
        refreshes: std::sync::Arc<AtomicU64>,
        releases: std::sync::Arc<AtomicU64>,
        fail_refresh: bool,
    }

    impl NativeControl for TestNativeControl {
        fn refresh(&self) -> Result<bool> {
            self.refreshes.fetch_add(1, Ordering::Relaxed);
            if self.fail_refresh { bail!("fixture native clock unavailable"); }
            Ok(true)
        }

        fn release(&self) -> Result<()> {
            self.releases.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn control_fixture(fixture: &Fixture, fail_refresh: bool) -> (std::sync::Arc<AtomicU64>, std::sync::Arc<AtomicU64>) {
        let refreshes = std::sync::Arc::new(AtomicU64::new(0));
        let releases = std::sync::Arc::new(AtomicU64::new(0));
        *fixture.clock.native_control.lock().unwrap() = Some(Box::new(TestNativeControl {
            refreshes: refreshes.clone(), releases: releases.clone(), fail_refresh,
        }));
        (refreshes, releases)
    }

    #[test]
    fn wave_uses_native_control_and_other_modes_release_it() {
        let fixture = Fixture::new();
        let (refreshes, releases) = control_fixture(&fixture, false);
        let now = Instant::now();
        let wave = ClockMusicPolicy { mode: ClockMusicMode::Wave, ..ClockMusicPolicy::default() };
        music(&fixture, wave, now);
        music(&fixture, wave, now + Duration::from_millis(50));
        assert_eq!(refreshes.load(Ordering::Relaxed), 2);
        assert_eq!(releases.load(Ordering::Relaxed), 0);
        music(&fixture, ClockMusicPolicy::default(), now + Duration::from_millis(100));
        assert_eq!(refreshes.load(Ordering::Relaxed), 2);
        assert_eq!(releases.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn native_control_failure_keeps_wave_and_release_survives_segment_failure() {
        let fixture = Fixture::new();
        let (refreshes, releases) = control_fixture(&fixture, true);
        let wave = ClockMusicPolicy { mode: ClockMusicMode::Wave, ..ClockMusicPolicy::default() };
        music(&fixture, wave, Instant::now());
        assert_eq!(refreshes.load(Ordering::Relaxed), 1);
        assert_eq!(fixture.get("A0"), "100");
        fs::remove_file(fixture.path("G3")).unwrap();
        assert!(fixture.clock.apply(ClockCommand::Off).is_err());
        assert_eq!(releases.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_wave_releases_native_control_and_backs_off() {
        let fixture = Fixture::new();
        let (refreshes, releases) = control_fixture(&fixture, false);
        let now = Instant::now();
        let policy = ClockMusicPolicy { mode: ClockMusicMode::Wave, ..ClockMusicPolicy::default() };
        let ClockCommand::Segments(frame) = wave() else { unreachable!() };
        music(&fixture, policy, now);
        fs::remove_file(fixture.path("G3")).unwrap();

        assert!(fixture.clock.write_music_at(&frame, policy, now + Duration::from_millis(50)).is_err());
        assert_eq!(releases.load(Ordering::Relaxed), 1);
        let attempted = refreshes.load(Ordering::Relaxed);
        assert!(fixture.clock.write_music_at(&frame, policy, now + Duration::from_millis(100)).is_err());
        assert_eq!(refreshes.load(Ordering::Relaxed), attempted);

        fixture.set("G3", "0");
        music(&fixture, policy, now + Duration::from_millis(150));
        assert_eq!(refreshes.load(Ordering::Relaxed), attempted);
        music(&fixture, policy, now + Duration::from_millis(1100));
        assert_eq!(refreshes.load(Ordering::Relaxed), attempted + 1);
    }

    #[test]
    fn music_default_cycle_has_exact_fifteen_and_five_second_boundaries() {
        let fixture = Fixture::new();
        paint(&fixture, &Clock::time_frame("12:34", 170).unwrap());
        let firmware = fixture.clock.read().unwrap();
        let now = Instant::now();
        let policy = ClockMusicPolicy::default();
        assert!(fixture.clock.music_policy_available());
        assert_eq!(policy.mode, ClockMusicMode::Alternating);
        assert_eq!((policy.wave_seconds, policy.time_seconds), (15, 5));
        music(&fixture, policy, now);
        assert_eq!(fixture.get("A0"), "100");
        music(&fixture, policy, now + Duration::from_millis(14999));
        assert_eq!(fixture.get("A0"), "100");
        music(&fixture, policy, now + Duration::from_secs(15));
        assert_eq!(fixture.clock.read().unwrap(), firmware);
        music(&fixture, policy, now + Duration::from_millis(19999));
        assert_eq!(fixture.clock.read().unwrap(), firmware);
        music(&fixture, policy, now + Duration::from_secs(20));
        assert_eq!(fixture.get("A0"), "100");
    }

    #[test]
    fn music_native_volume_in_clock_phase_restarts_full_clock_phase() {
        let fixture = Fixture::new();
        paint(&fixture, &Clock::time_frame("12:34", 170).unwrap());
        let now = Instant::now();
        let policy = ClockMusicPolicy::default();
        music(&fixture, policy, now);
        music(&fixture, policy, now + Duration::from_secs(15));
        paint(&fixture, &volume("15", 2));
        let native = fixture.clock.read().unwrap();
        music(&fixture, policy, now + Duration::from_secs(19));
        music(&fixture, policy, now + Duration::from_secs(30));
        assert_eq!(fixture.clock.read().unwrap(), native);
        paint(&fixture, &Clock::time_frame("12:35", 170).unwrap());
        let firmware = fixture.clock.read().unwrap();
        music(&fixture, policy, now + Duration::from_secs(31));
        music(&fixture, policy, now + Duration::from_millis(35999));
        assert_eq!(fixture.clock.read().unwrap(), firmware);
        music(&fixture, policy, now + Duration::from_secs(36));
        assert_eq!(fixture.get("A0"), "100");
    }

    #[test]
    fn music_partial_repaint_then_foreign_clock_gets_full_time_phase() {
        let fixture = Fixture::new();
        let now = Instant::now();
        let policy = ClockMusicPolicy::default();
        music(&fixture, policy, now);
        paint(&fixture, &HashMap::from([("B3".into(), 2)]));
        let partial = fixture.clock.read().unwrap();
        music(&fixture, policy, now + Duration::from_secs(2));
        assert_eq!(fixture.clock.read().unwrap(), partial);
        paint(&fixture, &Clock::time_frame("12:35", 170).unwrap());
        let firmware = fixture.clock.read().unwrap();
        music(&fixture, policy, now + Duration::from_secs(3));
        music(&fixture, policy, now + Duration::from_millis(7999));
        assert_eq!(fixture.clock.read().unwrap(), firmware);
        music(&fixture, policy, now + Duration::from_secs(8));
        assert_eq!(fixture.get("A0"), "100");
        // A complete foreign clock during wave also starts a full time phase.
        paint(&fixture, &Clock::time_frame("12:36", 170).unwrap());
        let firmware = fixture.clock.read().unwrap();
        music(&fixture, policy, now + Duration::from_secs(9));
        music(&fixture, policy, now + Duration::from_millis(13999));
        assert_eq!(fixture.clock.read().unwrap(), firmware);
        music(&fixture, policy, now + Duration::from_secs(14));
        assert_eq!(fixture.get("A0"), "100");
    }

    #[test]
    fn music_modes_resets_and_invalid_policy_preserve_hardware_contract() {
        let fixture = Fixture::new();
        paint(&fixture, &Clock::time_frame("12:34", 170).unwrap());
        let firmware = fixture.clock.read().unwrap();
        let now = Instant::now();
        let policy = ClockMusicPolicy::default();
        music(&fixture, policy, now);
        let ours = fixture.clock.read().unwrap();
        let ClockCommand::Segments(frame) = wave() else { unreachable!() };
        for (wave_seconds, time_seconds) in [(0, 5), (3601, 5), (15, 0), (15, 3601)] {
            let invalid = ClockMusicPolicy { wave_seconds, time_seconds, ..policy };
            assert!(fixture.clock.write_music_at(&frame, invalid, now).is_err());
            assert_eq!(fixture.clock.read().unwrap(), ours);
            assert_eq!(fixture.clock.state.lock().unwrap().music.as_ref().unwrap().policy, policy);
        }
        assert!(fixture.clock.write_music_at(&HashMap::from([("NOPE".into(), 100)]),
            ClockMusicPolicy { mode: ClockMusicMode::Clock, ..policy }, now).is_err());
        assert_eq!(fixture.clock.read().unwrap(), ours);
        assert_eq!(fixture.clock.state.lock().unwrap().music.as_ref().unwrap().policy, policy);
        let clock_only = ClockMusicPolicy { mode: ClockMusicMode::Clock, ..policy };
        music(&fixture, clock_only, now + Duration::from_secs(1));
        assert_eq!(fixture.clock.read().unwrap(), firmware);
        music(&fixture, clock_only, now + Duration::from_secs(100));
        assert_eq!(fixture.clock.read().unwrap(), firmware);
        let wave_only = ClockMusicPolicy { mode: ClockMusicMode::Wave, ..policy };
        music(&fixture, wave_only, now + Duration::from_secs(101));
        music(&fixture, wave_only, now + Duration::from_secs(200));
        assert_eq!(fixture.get("A0"), "100");
        paint(&fixture, &Clock::time_frame("12:35", 170).unwrap());
        music(&fixture, wave_only, now + Duration::from_secs(201));
        assert_eq!(fixture.get("A0"), "100");
        paint(&fixture, &volume("15", 2));
        let native = fixture.clock.read().unwrap();
        music(&fixture, wave_only, now + Duration::from_millis(201100));
        music(&fixture, wave_only, now + Duration::from_millis(201200));
        assert_eq!(fixture.clock.read().unwrap(), native);
        paint(&fixture, &Clock::time_frame("12:35", 170).unwrap());
        music(&fixture, wave_only, now + Duration::from_millis(201300));
        assert_eq!(fixture.get("A0"), "100");
        music(&fixture, policy, now + Duration::from_secs(202));
        music(&fixture, policy, now + Duration::from_secs(216));
        assert_eq!(fixture.get("A0"), "100");
        fixture.clock.apply(ClockCommand::Off).unwrap();
        assert!(fixture.clock.state.lock().unwrap().music.is_none());
        music(&fixture, policy, now + Duration::from_secs(217));
        fixture.clock.apply(wave()).unwrap();
        let state = fixture.clock.state.lock().unwrap();
        assert!(state.music.is_none());
        assert!(state.yielding_since.is_none());
    }

    #[test]
    fn music_first_write_yields_and_explicit_write_overrides() {
        let fixture = Fixture::new();
        paint(&fixture, &volume("0", 0));
        music(&fixture, wave_policy(), Instant::now());
        assert_eq!(fixture.get("A0"), "170");
        fixture.clock.apply(wave()).unwrap();
        assert_eq!(fixture.get("A0"), "100");
        assert_eq!(fixture.get("B0"), "0");
        music(&fixture, wave_policy(), Instant::now());
        assert_eq!(fixture.get("A0"), "100");
    }

    #[test]
    fn music_numeric_frame_we_own_does_not_trigger_yield() {
        let fixture = Fixture::new();
        fixture.clock.write_music_at(&volume("15", 2), wave_policy(), Instant::now()).unwrap();
        music(&fixture, wave_policy(), Instant::now());
        assert_eq!(fixture.get("A0"), "100");
        assert!(fixture.clock.state.lock().unwrap().yielding_since.is_none());
    }

    #[test]
    fn music_time_request_overrides_yield() {
        let fixture = Fixture::new();
        paint(&fixture, &volume("15", 2));
        let now = Instant::now();
        music(&fixture, wave_policy(), now);
        assert!(fixture.clock.write_music_at(
            &HashMap::from([("NOPE".into(), 100)]), wave_policy(), now,
        ).is_err());
        assert_eq!(fixture.clock.state.lock().unwrap().yielding_since, Some(now));
        fixture.clock.apply(ClockCommand::Time { text: "12:34".into(), brightness: 100 }).unwrap();
        assert_eq!(fixture.get("DH"), "100");
        assert!(fixture.clock.state.lock().unwrap().yielding_since.is_none());
    }

    #[test]
    fn native_volume_requires_complete_edge_aligned_digits_in_range() {
        let fixture = Fixture::new();
        for (text, start) in [("0", 0), ("9", 3), ("15", 0), ("15", 2), ("100", 1)] {
            paint(&fixture, &volume(text, start));
            assert!(fixture.clock.is_volume(&fixture.clock.read().unwrap()));
        }
        for (text, start) in [("1", 1), ("15", 1), ("101", 1), ("1234", 0)] {
            paint(&fixture, &volume(text, start));
            assert!(!fixture.clock.is_volume(&fixture.clock.read().unwrap()));
        }
        if matches!(LAYOUT, Layout::Lite2) {
            paint(&fixture, &volume("15", 2));
            fixture.set("H2", "170");
            assert!(!fixture.clock.is_volume(&fixture.clock.read().unwrap()));
        }
        for text in ["12:35", "23:59"] {
            paint(&fixture, &Clock::time_frame(text, 170).unwrap());
            let frame = fixture.clock.read().unwrap();
            assert!(!fixture.clock.is_volume(&frame));
            assert!(fixture.clock.is_clock(&frame));
        }
        for text in ["24:00", "12:60"] {
            paint(&fixture, &Clock::time_frame(text, 170).unwrap());
            assert!(!fixture.clock.is_clock(&fixture.clock.read().unwrap()));
        }
    }

    #[test]
    fn firmware_clock_can_omit_only_the_first_hour_digit() {
        let fixture = Fixture::new();
        let mut clock = Clock::time_frame("09:43", 170).unwrap();
        clock.retain(|name, _| !name.ends_with('0'));
        paint(&fixture, &clock);
        assert!(fixture.clock.is_clock(&fixture.clock.read().unwrap()));
        paint(&fixture, &volume("15", 2));
        music(&fixture, wave_policy(), Instant::now());
        paint(&fixture, &clock);
        music(&fixture, wave_policy(), Instant::now());
        assert_eq!(fixture.get("A0"), "100");
        for position in [1u8, 2, 3] {
            let mut incomplete = Clock::time_frame("09:43", 170).unwrap();
            incomplete.retain(|name, _| !name.ends_with(char::from(b'0' + position)));
            paint(&fixture, &incomplete);
            assert!(!fixture.clock.is_clock(&fixture.clock.read().unwrap()));
        }
    }

    #[test]
    fn detects_only_a_complete_layout() {
        let fixture = Fixture::new();
        let lite2 = matches!(LAYOUT, Layout::Lite2);
        assert_eq!(fixture.clock.segments().len(), if lite2 { 36 } else { 32 });
        assert_eq!(fixture.clock.segments().contains(&"H3".to_string()), lite2);
        assert_eq!(fixture.path("A0"), fixture.root.join(format!("{}A0/brightness", LAYOUT.prefix())));
        assert!(Clock::detect(&fixture.root, match LAYOUT {
            Layout::Lite2 => Layout::Mini3,
            Layout::Mini3 | Layout::Midi => Layout::Lite2,
        }).is_none());
        assert!(fixture.clock.segments().contains(&"DEGREE".to_string()));

        fs::remove_file(fixture.path("DM")).unwrap();
        assert!(Clock::detect(&fixture.root, LAYOUT).is_none());
    }

    #[test]
    fn missing_digit_segment_disables_backend() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.path("G3")).unwrap();
        assert!(Clock::detect(&fixture.root, LAYOUT).is_none());
    }

    #[test]
    fn off_without_ownership_leaves_native_display_untouched() {
        let fixture = Fixture::new();
        paint(&fixture, &Clock::time_frame("12:34", 170).unwrap());
        let native = fixture.clock.read().unwrap();
        fixture.clock.apply(ClockCommand::Off).unwrap();
        assert_eq!(fixture.clock.read().unwrap(), native);
        let now = Instant::now();
        music(&fixture, ClockMusicPolicy { mode: ClockMusicMode::Clock,
            ..ClockMusicPolicy::default() }, now);
        assert_eq!(fixture.clock.read().unwrap(), native);
        fixture.clock.apply(ClockCommand::Off).unwrap();
        assert_eq!(fixture.clock.read().unwrap(), native);
    }

    #[test]
    fn renders_hh_mm_with_separator_and_blank_h_segments() {
        let frame = Clock::time_frame("12:34", 170).unwrap();
        assert_eq!(frame.get("B0"), Some(&170));
        assert_eq!(frame.get("C0"), Some(&170));
        assert_eq!(frame.get("A0"), None);
        assert_eq!(frame.get("H0"), None);
        assert_eq!(frame.get("DH"), Some(&170));
        assert_eq!(frame.get("DL"), Some(&170));
        assert_eq!(frame.get("DM"), None);
        for text in ["", "1:23", "12:3", "12-34", "ab:cd", "12:345"] {
            assert!(Clock::time_frame(text, 170).is_err(), "accepted {text:?}");
        }
    }

    #[test]
    fn unknown_segment_is_rejected_before_any_write() {
        let fixture = Fixture::new();
        let mut frame = HashMap::new();
        frame.insert("A0".to_string(), 255);
        frame.insert("NOPE".to_string(), 255);
        assert!(fixture.clock.write(&frame).is_err());
        assert_eq!(fixture.get("A0"), "0");
    }

    #[test]
    fn off_restores_the_firmware_frame() {
        let fixture = Fixture::new();
        fixture.set("A0", "170");
        fixture.set("DH", "255");

        fixture
            .clock
            .apply(ClockCommand::Time {
                text: "12:34".into(),
                brightness: 200,
            })
            .unwrap();
        assert_eq!(fixture.get("B0"), "200");

        fixture.clock.apply(ClockCommand::Off).unwrap();
        assert_eq!(fixture.get("A0"), "170");
        assert_eq!(fixture.get("DH"), "255");
        assert_eq!(fixture.get("B0"), "0");
    }

    #[test]
    fn off_leaves_a_firmware_repaint_alone() {
        let fixture = Fixture::new();
        fixture
            .clock
            .apply(ClockCommand::Segments(HashMap::from([("A0".into(), 100)])))
            .unwrap();
        // platformd redraws the clock and takes ownership back.
        fixture.set("A0", "42");
        fixture.clock.apply(ClockCommand::Off).unwrap();
        assert_eq!(fixture.get("A0"), "42");
    }
    }; }

    mod lite2 {
        use super::*;
        const LAYOUT: Layout = Layout::Lite2;
        shared_tests!();
    }

    #[test]
    fn midi_keeps_segment_names_and_recovers_wave_after_blank_repaint() {
        let root = std::env::temp_dir().join(format!("luna-midi-clock-{}", std::process::id()));
        let settings = root.join("settings.json");
        let layout = Layout::Midi;
        for name in segment_names(layout) {
            let path = layout.segment_path(&root, &name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "0").unwrap();
        }
        fs::write(&settings, br#"{"clockEnabled":false,"isClockOn":false}"#).unwrap();
        let latch = root.join("cucumber:sync:clock-0/brightness");
        fs::create_dir_all(latch.parent().unwrap()).unwrap();
        fs::write(&latch, "1").unwrap();
        let clock = Clock::detect_with_settings_path(&root, layout, settings).unwrap();
        assert!(clock.text_available());
        assert!(clock.segments().contains(&"A4".into()));
        assert!(!clock.segments().contains(&"A0".into()));
        clock.apply(ClockCommand::Time { text: "12:34".into(), brightness: 100 }).unwrap();
        assert_eq!(fs::read_to_string(&latch).unwrap(), "0");
        assert_eq!(fs::read_to_string(root.join("cucumber:white:clock-p3/brightness")).unwrap(), "100");
        assert_eq!(fs::read_to_string(root.join("cucumber:white:clock-q3/brightness")).unwrap(), "100");
        assert!(clock.is_clock(&clock.read().unwrap()));

        clock.apply(ClockCommand::Off).unwrap();
        let wave = HashMap::from([("D1".into(), 50)]);
        let now = Instant::now();
        let policy = ClockMusicPolicy { mode: ClockMusicMode::Wave, ..ClockMusicPolicy::default() };
        clock.write_music_at(&wave, policy, now).unwrap();
        let segment = layout.segment_path(&root, "D1");
        assert_eq!(fs::read_to_string(&segment).unwrap(), "50");
        fs::write(&segment, "0").unwrap();
        clock.write_music_at(&wave, policy, now + Duration::from_millis(10)).unwrap();
        clock.write_music_at(&wave, policy, now + Duration::from_millis(60)).unwrap();
        assert_eq!(fs::read_to_string(&segment).unwrap(), "0");
        clock.write_music_at(&wave, policy, now + Duration::from_millis(210)).unwrap();
        assert_eq!(fs::read_to_string(&segment).unwrap(), "50");
        clock.poll_clock(now + Duration::from_millis(3210)).unwrap();
        assert_eq!(fs::read_to_string(&segment).unwrap(), "0");
        fs::remove_dir_all(root).unwrap();
    }

    mod mini3 {
        use super::*;
        const LAYOUT: Layout = Layout::Mini3;
        shared_tests!();

        #[test]
        fn preserves_short_hour_spaces_and_dashes() {
            let fixture = Fixture::new();
            for text in ["1:23", " 1:23", "--:--", "  :  ", ":23"] {
                fixture.clock.apply(ClockCommand::Time { text: text.into(), brightness: 170 }).unwrap();
                assert_eq!(fixture.get("DH"), "170");
                assert_eq!(fixture.get("DL"), "170");
            }
            fixture.clock.apply(ClockCommand::Time { text: "1:23".into(), brightness: 170 }).unwrap();
            assert_eq!(fixture.get("A0"), "0");
            assert_eq!(fixture.get("B1"), "170");
            let before = fixture.clock.read().unwrap();
            for text in ["123:45", "12:3", "ab:cd", "é:23"] {
                assert!(fixture.clock.apply(ClockCommand::Time { text: text.into(), brightness: 170 }).is_err());
                assert_eq!(fixture.clock.read().unwrap(), before);
            }
        }

        #[test]
        fn rejects_nonexistent_h_segment_before_writing() {
            let fixture = Fixture::new();
            let before = fixture.clock.read().unwrap();
            assert!(fixture.clock.apply_music(ClockCommand::Segments(HashMap::from([
                ("A0".into(), 100), ("H0".into(), 100),
            ])), ClockMusicPolicy::default()).is_err());
            assert_eq!(fixture.clock.read().unwrap(), before);
            assert!(fixture.clock.state.lock().unwrap().music.is_none());
        }
    }
}
