use anyhow::{bail, Context, Result};
use luna::{ClockBackend, ClockCommand};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

const ROOT: &str = "/sys/class/leds";
const PREFIX: &str = "plum:white:clock";

const PARTS: [&str; 8] = ["A", "B", "C", "D", "E", "F", "G", "H"];
const EXTRA_SEGMENTS: [&str; 4] = ["DEGREE", "DH", "DM", "DL"];

pub fn create_clock_backend() -> Option<Box<dyn ClockBackend>> {
    let clock = Clock::detect(Path::new(ROOT))?;
    Some(Box::new(clock))
}

/// Four digits, positions numbered from 0, eight segments per digit plus the
/// degree marker and the three separator dots between the digit pairs.
fn segment_names() -> Vec<String> {
    let mut names = Vec::with_capacity(PARTS.len() * 4 + EXTRA_SEGMENTS.len());
    for position in 0..4 {
        for part in PARTS {
            names.push(format!("{part}{position}"));
        }
    }
    names.extend(EXTRA_SEGMENTS.into_iter().map(String::from));
    names
}

struct FrameOwnership {
    system: Vec<(PathBuf, String)>,
    ours: Vec<(PathBuf, String)>,
}

struct Clock {
    paths: Vec<(String, PathBuf)>,
    ownership: Mutex<Option<FrameOwnership>>,
}

impl Clock {
    fn detect(root: &Path) -> Option<Self> {
        let paths = segment_names()
            .into_iter()
            .map(|name| {
                let path = root.join(format!("{PREFIX}{name}/brightness"));
                (name, path)
            })
            .collect::<Vec<_>>();
        paths.iter().all(|(_, path)| path.is_file()).then(|| Self {
            paths,
            ownership: Mutex::new(None),
        })
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

    fn write(&self, frame: &HashMap<String, u8>) -> Result<()> {
        let known = self
            .paths
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<HashSet<_>>();
        if let Some(name) = frame.keys().find(|name| !known.contains(name.as_str())) {
            bail!("unknown clock segment {name:?}");
        }
        let mut ownership = self.ownership.lock().unwrap_or_else(|e| e.into_inner());
        let current = self.read()?;
        // A fresh write starts a new ownership window, unless the firmware has
        // repainted the display on its own since our previous frame.
        if ownership.as_ref().is_none_or(|owned| owned.ours != current) {
            *ownership = Some(FrameOwnership {
                system: current,
                ours: Vec::new(),
            });
        }
        for (name, path) in &self.paths {
            let value = frame.get(name).copied().unwrap_or(0);
            fs::write(path, value.to_string())
                .with_context(|| format!("write clock segment {name}"))?;
        }
        if let Some(owned) = ownership.as_mut() {
            owned.ours = self.read()?;
        }
        Ok(())
    }

    /// Gives the display back to the firmware, but only when it still shows the
    /// frame we painted. platformd redraws the clock once a minute, so an
    /// external repaint wins and must not be overwritten.
    fn release(&self) -> Result<()> {
        let mut ownership = self.ownership.lock().unwrap_or_else(|e| e.into_inner());
        let Some(owned) = ownership.take() else {
            return Ok(());
        };
        if self.read()? != owned.ours {
            return Ok(());
        }
        for (path, value) in owned.system {
            fs::write(&path, value)
                .with_context(|| format!("restore clock segment {}", path.display()))?;
        }
        Ok(())
    }

    fn time_frame(text: &str, brightness: u8) -> Result<HashMap<String, u8>> {
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
                frame.insert(format!("{part}{position}"), brightness);
            }
        }
        // The colon between the two digit pairs is the upper and lower
        // separator dots; DM is the middle dot and stays for explicit frames.
        frame.insert("DH".into(), brightness);
        frame.insert("DL".into(), brightness);
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

    fn apply(&self, command: ClockCommand) -> Result<()> {
        match command {
            ClockCommand::Off => self.release(),
            ClockCommand::Segments(values) => self.write(&values),
            ClockCommand::Time { text, brightness } => {
                self.write(&Self::time_frame(&text, brightness)?)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        clock: Clock,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "luna-plum-clock-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            for name in segment_names() {
                let dir = root.join(format!("{PREFIX}{name}"));
                fs::create_dir(&dir).unwrap();
                fs::write(dir.join("brightness"), "0").unwrap();
            }
            let clock = Clock::detect(&root).unwrap();
            Self { root, clock }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(format!("{PREFIX}{name}/brightness"))
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

    #[test]
    fn detects_only_a_complete_layout() {
        let fixture = Fixture::new();
        assert_eq!(fixture.clock.segments().len(), 36);
        assert!(fixture.clock.segments().contains(&"H3".to_string()));
        assert!(fixture.clock.segments().contains(&"DEGREE".to_string()));

        fs::remove_file(fixture.path("DM")).unwrap();
        assert!(Clock::detect(&fixture.root).is_none());
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
}
