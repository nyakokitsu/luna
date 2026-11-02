//! Pickle RGB sysfs channels. Restore the exact pre-existing triggers and
//! brightness, without taking over a zone that another indicator has changed.
use anyhow::{bail, Context, Result};
use luna::LedBackend;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub fn create_led_backend() -> Result<Box<dyn LedBackend>> {
    Ok(Box::new(Indicators::open(Path::new("/sys/class/leds"))?))
}
struct Saved {
    values: Vec<(PathBuf, String, String)>,
    last: Vec<u8>,
}
struct Indicators {
    root: PathBuf,
    ring: Mutex<Option<Saved>>,
    logo: Mutex<Option<Saved>>,
}
fn selected(text: &str) -> Result<String> {
    text.split_whitespace()
        .find_map(|s| s.strip_prefix('[')?.strip_suffix(']'))
        .map(str::to_owned)
        .context("LED trigger has no selected value")
}
impl Indicators {
    fn paths(&self, zone: &str, count: usize) -> Vec<PathBuf> {
        (0..count)
            .flat_map(|i| {
                ["red", "green", "blue"]
                    .map(move |c| self.root.join(format!("pickle:{c}:{zone}{i}")))
            })
            .collect()
    }
    fn open(root: &Path) -> Result<Self> {
        let out = Self {
            root: root.into(),
            ring: Mutex::new(None),
            logo: Mutex::new(None),
        };
        for p in out
            .paths("ring", 16)
            .into_iter()
            .chain(out.paths("logo", 6))
        {
            if !p.join("brightness").is_file() || !p.join("trigger").is_file() {
                bail!("incomplete pickle RGB interface: {}", p.display());
            }
        }
        Ok(out)
    }
    fn write(
        &self,
        zone: &str,
        count: usize,
        frame: &[u8],
        state: &Mutex<Option<Saved>>,
    ) -> Result<()> {
        if frame.len() != count * 3 {
            bail!("incorrect pickle {zone} frame length");
        }
        let mut saved = state.lock().unwrap();
        let paths = self.paths(zone, count);
        if saved.is_none() {
            let values = paths
                .iter()
                .map(|p| {
                    Ok((
                        p.clone(),
                        fs::read_to_string(p.join("brightness"))?.trim().to_owned(),
                        selected(&fs::read_to_string(p.join("trigger"))?)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            *saved = Some(Saved {
                values,
                last: Vec::new(),
            });
        }
        for (p, v) in paths.iter().zip(frame) {
            fs::write(p.join("trigger"), "none")?;
            fs::write(p.join("brightness"), v.to_string())?;
        }
        saved.as_mut().unwrap().last = frame.to_vec();
        Ok(())
    }
    fn restore(&self, state: &Mutex<Option<Saved>>) -> Result<()> {
        let mut guard = state.lock().unwrap();
        let Some(saved) = guard.take() else {
            return Ok(());
        };
        // Yield if a subsequent writer changed this zone. A sysfs fixture does
        // not render [none], while the kernel does; accept both spellings.
        let ours = saved.values.iter().zip(&saved.last).all(|((p, _, _), v)| {
            let current = fs::read_to_string(p.join("brightness")).unwrap_or_default();
            let trigger = fs::read_to_string(p.join("trigger")).unwrap_or_default();
            current.trim().parse::<u8>().ok() == Some(*v)
                && (trigger.trim() == "none" || selected(&trigger).ok().as_deref() == Some("none"))
        });
        if ours {
            for (p, value, trigger) in saved.values {
                fs::write(p.join("brightness"), value)?;
                fs::write(p.join("trigger"), trigger)?;
            }
        }
        Ok(())
    }
}
impl LedBackend for Indicators {
    fn show(&self, frame: &[u8]) -> Result<()> {
        self.write("ring", 16, frame, &self.ring)
    }
    fn show_logo(&self, frame: &[u8]) -> Result<()> {
        self.write("logo", 6, frame, &self.logo)
    }
    fn restore_ring_trigger(&self) -> Result<()> {
        self.restore(&self.ring)
    }
    fn restore_logo_trigger(&self) -> Result<()> {
        self.restore(&self.logo)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("luna-pickle-{name}-{}", std::process::id()));
        for (zone, count) in [("ring", 16), ("logo", 6)] {
            for i in 0..count {
                for c in ["red", "green", "blue"] {
                    let p = root.join(format!("pickle:{c}:{zone}{i}"));
                    fs::create_dir_all(&p).unwrap();
                    fs::write(p.join("brightness"), "17").unwrap();
                    fs::write(p.join("trigger"), format!("none [ledfb-{zone}]")).unwrap();
                }
            }
        }
        root
    }
    #[test]
    fn zones_are_separate_and_restore_original_state() {
        let root = fixture("zones");
        let b = Indicators::open(&root).unwrap();
        b.show(&vec![42; 48]).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("pickle:red:logo0/brightness")).unwrap(),
            "17"
        );
        b.show_logo(&vec![23; 18]).unwrap();
        b.restore_ring_trigger().unwrap();
        b.restore_logo_trigger().unwrap();
        for p in b.paths("ring", 16).into_iter().chain(b.paths("logo", 6)) {
            assert_eq!(fs::read_to_string(p.join("brightness")).unwrap(), "17");
            assert!(fs::read_to_string(p.join("trigger"))
                .unwrap()
                .starts_with("ledfb-"));
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn validates_layout_and_lengths() {
        let root = fixture("validate");
        let b = Indicators::open(&root).unwrap();
        assert!(b.show(&[0; 18]).is_err());
        assert!(b.show_logo(&[0; 48]).is_err());
        fs::remove_file(root.join("pickle:blue:ring15/brightness")).unwrap();
        assert!(Indicators::open(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn does_not_overwrite_a_new_owner() {
        let root = fixture("owner");
        let b = Indicators::open(&root).unwrap();
        b.show(&[42; 48]).unwrap();
        fs::write(root.join("pickle:red:ring0/brightness"), "99").unwrap();
        b.restore_ring_trigger().unwrap();
        assert_eq!(
            fs::read_to_string(root.join("pickle:red:ring0/brightness")).unwrap(),
            "99"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
