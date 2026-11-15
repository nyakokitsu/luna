//! Yandex Mini 2 RGB sysfs channels.
//!
//! `/sys/class/leds/mini2:{red,green,blue}:ring1..6` expose `brightness` and
//! `trigger`. The stock indicator writer sets `brightness` directly with the
//! trigger at `none`. The calibration preview detaches that owner for the
//! lifetime of its lease, so this backend saves the brightness and trigger of
//! the ring before the first preview frame and puts both back on release.
//! It never overwrites a zone that another writer changed in the meantime.

use anyhow::{bail, Context, Result};
use luna::LedBackend;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

const RING_COUNT: usize = 6;
const CHANNEL_COUNT: usize = RING_COUNT * 3;

pub fn create_led_backend() -> Result<Box<dyn LedBackend>> {
    Ok(Box::new(SysfsRing::open(Path::new("/sys/class/leds"))))
}

struct Saved {
    values: Vec<(PathBuf, String, String)>,
    last: Vec<u8>,
}

struct SysfsRing {
    paths: Vec<(usize, PathBuf)>,
    saved: Mutex<Option<Saved>>,
}

fn selected(text: &str) -> Result<String> {
    if let Some(name) = text
        .split_whitespace()
        .find_map(|s| s.strip_prefix('[')?.strip_suffix(']'))
    {
        return Ok(name.to_owned());
    }
    // Most kernels render the trigger as "none [ledfb-ring]"; a sysfs fixture
    // or an emulator bridge may return the active trigger as a bare word.
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.as_slice() {
        [name] => Ok((*name).to_owned()),
        _ => bail!("LED trigger has no selected value"),
    }
}

impl SysfsRing {
    fn open(root: &Path) -> Self {
        let mut paths = Vec::new();
        for index in 1..=RING_COUNT {
            for (channel_index, channel) in ["red", "green", "blue"].iter().enumerate() {
                let path = root
                    .join(format!("mini2:{channel}:ring{index}"))
                    .join("brightness");
                if path.exists() {
                    paths.push(((index - 1) * 3 + channel_index, path));
                }
            }
        }
        Self {
            paths,
            saved: Mutex::new(None),
        }
    }

    fn restore(&self) -> Result<()> {
        let mut guard = self.saved.lock().unwrap();
        let Some(saved) = guard.take() else {
            return Ok(());
        };
        // Yield if a subsequent writer changed this zone. A sysfs fixture does
        // not render [none], while the kernel does; accept both spellings.
        let ours = saved.values.iter().zip(&saved.last).all(|((p, _, _), v)| {
            let current = fs::read_to_string(p).unwrap_or_default();
            let trigger = fs::read_to_string(p.with_file_name("trigger")).unwrap_or_default();
            current.trim().parse::<u8>().ok() == Some(*v)
                && (trigger.trim() == "none" || selected(&trigger).ok().as_deref() == Some("none"))
        });
        if ours {
            for (p, value, trigger) in saved.values {
                fs::write(&p, value)?;
                fs::write(p.with_file_name("trigger"), trigger)?;
            }
        }
        Ok(())
    }
}

impl LedBackend for SysfsRing {
    fn show(&self, frame: &[u8]) -> Result<()> {
        if frame.len() != CHANNEL_COUNT {
            bail!("unexpected Yandex Mini 2 LED frame length");
        }
        if self.paths.is_empty() {
            bail!("Yandex Mini 2 LED sysfs nodes are unavailable");
        }
        let mut saved = self.saved.lock().unwrap();
        if saved.is_none() {
            let values = self
                .paths
                .iter()
                .map(|(_, path)| {
                    let trigger = path.with_file_name("trigger");
                    Ok((
                        path.clone(),
                        fs::read_to_string(path)
                            .with_context(|| format!("read {}", path.display()))?
                            .trim()
                            .to_owned(),
                        selected(
                            &fs::read_to_string(&trigger)
                                .with_context(|| format!("read {}", trigger.display()))?,
                        )?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            *saved = Some(Saved {
                values,
                last: Vec::new(),
            });
        }
        for (frame_index, path) in &self.paths {
            fs::write(path, frame[*frame_index].to_string())
                .with_context(|| format!("write {}", path.display()))?;
        }
        saved.as_mut().unwrap().last = frame.to_vec();
        Ok(())
    }

    fn restore_ring_trigger(&self) -> Result<()> {
        self.restore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("luna-mini2-{name}-{}", std::process::id()));
        for i in 1..=RING_COUNT {
            for c in ["red", "green", "blue"] {
                let p = root.join(format!("mini2:{c}:ring{i}"));
                fs::create_dir_all(&p).unwrap();
                fs::write(p.join("brightness"), "17").unwrap();
                fs::write(p.join("trigger"), "none").unwrap();
            }
        }
        root
    }

    #[test]
    fn release_restores_the_previous_frame_and_trigger() {
        let root = fixture("restore");
        let b = SysfsRing::open(&root);
        b.show(&vec![42; CHANNEL_COUNT]).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("mini2:red:ring1/brightness")).unwrap(),
            "42"
        );
        b.restore_ring_trigger().unwrap();
        for i in 1..=RING_COUNT {
            for c in ["red", "green", "blue"] {
                let p = root.join(format!("mini2:{c}:ring{i}"));
                assert_eq!(fs::read_to_string(p.join("brightness")).unwrap(), "17");
                assert_eq!(fs::read_to_string(p.join("trigger")).unwrap(), "none");
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn release_does_not_overwrite_a_new_owner() {
        let root = fixture("owner");
        let b = SysfsRing::open(&root);
        b.show(&vec![42; CHANNEL_COUNT]).unwrap();
        fs::write(root.join("mini2:red:ring1/brightness"), "99").unwrap();
        b.restore_ring_trigger().unwrap();
        assert_eq!(
            fs::read_to_string(root.join("mini2:red:ring1/brightness")).unwrap(),
            "99"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validates_frame_length_and_accepts_both_trigger_spellings() {
        let root = fixture("validate");
        let b = SysfsRing::open(&root);
        assert!(b.show(&[0; CHANNEL_COUNT - 1]).is_err());
        assert!(b.show(&[0; CHANNEL_COUNT + 1]).is_err());
        assert_eq!(selected("[none] timer").unwrap(), "none");
        assert_eq!(selected("none\n").unwrap(), "none");
        assert!(selected("timer heartbeat").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
