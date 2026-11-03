use anyhow::{bail, Context, Result};
use luna::{ClockBackend, ClockCommand};
use std::{collections::HashMap, fs, path::Path, sync::Mutex};

const ROOT: &str = "/sys/class/leds";

pub fn create_clock_backend() -> Option<Box<dyn ClockBackend>> {
    let clock = Clock::detect()?;
    Some(Box::new(clock))
}

struct Clock {
    paths: Vec<(String, String)>,
    saved: Mutex<Option<(HashMap<String, String>, HashMap<String, u8>)>>,
}

impl Clock {
    fn names() -> Vec<String> {
        let mut out = Vec::new();
        for position in 0..4 {
            for part in ["A", "B", "C", "D", "E", "F", "G", "H"] {
                out.push(format!("{part}{position}"));
            }
        }
        out.extend(["DEGREE", "DH", "DL", "DM"].into_iter().map(String::from));
        out
    }

    fn detect() -> Option<Self> {
        Self::detect_at(Path::new(ROOT))
    }
    fn detect_at(root: &Path) -> Option<Self> {
        let paths = Self::names()
            .into_iter()
            .map(|name| {
                let path = root.join(format!("pickle:white:clock{name}/brightness"));
                (name, path.to_string_lossy().into_owned())
            })
            .collect::<Vec<_>>();
        paths
            .iter()
            .all(|(_, path)| Path::new(path).is_file())
            .then_some(Self {
                paths,
                saved: Mutex::new(None),
            })
    }

    fn blank(&self) -> HashMap<String, u8> {
        self.paths
            .iter()
            .map(|(name, _)| (name.clone(), 0))
            .collect()
    }
    fn glyph(ch: char) -> Option<&'static str> {
        Some(match ch {
            '0' => "ABCDEF",
            '1' => "BC",
            '2' => "ABDEG",
            '3' => "ABCDG",
            '4' => "BCFG",
            '5' => "ACDFG",
            '6' => "ACDEFG",
            '7' => "ABC",
            '8' => "ABCDEFG",
            '9' => "ABCDFG",
            '-' => "G",
            ' ' => "",
            _ => return None,
        })
    }
    fn write(&self, frame: &HashMap<String, u8>) -> Result<()> {
        let mut saved = self.saved.lock().unwrap();
        if saved.is_none() {
            let original = self
                .paths
                .iter()
                .map(|(name, path)| Ok((name.clone(), fs::read_to_string(path)?.trim().to_owned())))
                .collect::<Result<HashMap<_, _>>>()?;
            *saved = Some((original, frame.clone()));
        }
        for (name, path) in &self.paths {
            fs::write(path, frame.get(name).copied().unwrap_or(0).to_string())
                .with_context(|| format!("write clock segment {name}"))?;
        }
        saved.as_mut().unwrap().1 = frame.clone();
        Ok(())
    }
    fn release(&self) -> Result<()> {
        let mut saved = self.saved.lock().unwrap();
        let Some((original, last)) = saved.take() else {
            return Ok(());
        };
        let ours = self.paths.iter().all(|(name, path)| {
            fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
                == last.get(name).copied()
        });
        if ours {
            for (name, path) in &self.paths {
                fs::write(path, &original[name])?;
            }
        }
        Ok(())
    }
    fn time_frame(&self, text: &str, brightness: u8) -> Result<HashMap<String, u8>> {
        let (left, right) = text
            .split_once(':')
            .context("clock time must look like HH:MM")?;
        if left.len() > 2 || right.len() != 2 {
            bail!("clock time must look like HH:MM")
        }
        let chars = format!("{left:>2}{right}");
        let mut frame = self.blank();
        for (position, ch) in chars.chars().enumerate() {
            for part in Self::glyph(ch)
                .context("clock supports digits, '-' and spaces")?
                .chars()
            {
                frame.insert(format!("{part}{position}"), brightness);
            }
        }
        frame.insert("DH".into(), brightness);
        frame.insert("DL".into(), brightness);
        Ok(frame)
    }
}

impl ClockBackend for Clock {
    fn segments(&self) -> Vec<String> {
        self.paths.iter().map(|(name, _)| name.clone()).collect()
    }
    fn apply(&self, command: ClockCommand) -> Result<()> {
        match command {
            ClockCommand::Off => self.release(),
            ClockCommand::Segments(values) => {
                let mut frame = self.blank();
                for (name, value) in values {
                    if !frame.contains_key(&name) {
                        bail!("unknown clock segment {name}")
                    }
                    frame.insert(name, value);
                }
                self.write(&frame)
            }
            ClockCommand::Time { text, brightness } => {
                self.write(&self.time_frame(&text, brightness)?)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(label: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("luna-pickle-clock-{label}-{}", std::process::id()));
        for name in Clock::names() {
            let p = root.join(format!("pickle:white:clock{name}"));
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("brightness"), "9").unwrap();
        }
        root
    }
    #[test]
    fn detects_all_36_segments_and_restores_original_values() {
        let root = fixture("restore");
        let clock = Clock::detect_at(&root).unwrap();
        assert_eq!(clock.segments().len(), 36);
        assert!(clock.segments().contains(&"H0".into()));
        clock
            .apply(ClockCommand::Time {
                text: "12:34".into(),
                brightness: 23,
            })
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("pickle:white:clockH0/brightness")).unwrap(),
            "0"
        );
        clock.apply(ClockCommand::Off).unwrap();
        for (_, path) in &clock.paths {
            assert_eq!(fs::read_to_string(path).unwrap(), "9");
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejects_wrong_layout_or_segments_without_writing() {
        let root = fixture("validate");
        let clock = Clock::detect_at(&root).unwrap();
        assert!(clock
            .apply(ClockCommand::Segments(HashMap::from([("A4".into(), 1)])))
            .is_err());
        assert!(clock
            .apply(ClockCommand::Time {
                text: "x2:34".into(),
                brightness: 23
            })
            .is_err());
        for (_, path) in &clock.paths {
            assert_eq!(fs::read_to_string(path).unwrap(), "9");
        }
        fs::remove_file(root.join("pickle:white:clockH3/brightness")).unwrap();
        assert!(Clock::detect_at(&root).is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
