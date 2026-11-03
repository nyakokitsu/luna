//! Station Street (mango) RGB sysfs channels.
//!
//! `/sys/class/leds/mango:{red,green,blue}:ring1..4` and
//! `/sys/class/leds/mango:{red,green,blue}:logo0..2` exist, but both zones
//! start under a firmware trigger (`ledfb-ring` / `ledfb-logo`). A direct
//! `brightness` write is ignored until the trigger is detached, so this
//! backend saves the original brightness and trigger of a zone, writes with
//! the trigger set to `none`, and puts both back when the indicator is
//! released. It never takes over a zone that another writer has changed.
//!
//! The framebuffer devices (`/dev/fb0` ring 4x1, `/dev/fb1` logo 3x1) do not
//! mirror into the sysfs LEDs on this firmware, so sysfs is the usable route.

use anyhow::{bail, Result};
use luna::LedBackend;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

const RING_COUNT: usize = 4;
const LOGO_COUNT: usize = 3;
/// Ring nodes are numbered from 1 (`ring1..ring4`), logo nodes from 0.
const RING_FIRST: usize = 1;
const LOGO_FIRST: usize = 0;

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
    if let Some(name) = text
        .split_whitespace()
        .find_map(|s| s.strip_prefix('[')?.strip_suffix(']'))
    {
        return Ok(name.to_owned());
    }
    // Most kernels render the trigger as "none [ledfb-ring]"; the emulator's
    // sysfs bridge returns the active trigger as a bare word instead.
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.as_slice() {
        [name] => Ok((*name).to_owned()),
        _ => bail!("LED trigger has no selected value"),
    }
}

impl Indicators {
    fn paths(&self, zone: &str, count: usize, first: usize) -> Vec<PathBuf> {
        (0..count)
            .flat_map(|i| {
                let index = first + i;
                ["red", "green", "blue"]
                    .map(move |c| self.root.join(format!("mango:{c}:{zone}{index}")))
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
            .paths("ring", RING_COUNT, RING_FIRST)
            .into_iter()
            .chain(out.paths("logo", LOGO_COUNT, LOGO_FIRST))
        {
            if !p.join("brightness").is_file() || !p.join("trigger").is_file() {
                bail!("incomplete mango RGB interface: {}", p.display());
            }
        }
        Ok(out)
    }

    fn write(
        &self,
        zone: &str,
        count: usize,
        first: usize,
        frame: &[u8],
        state: &Mutex<Option<Saved>>,
    ) -> Result<()> {
        if frame.len() != count * 3 {
            bail!("incorrect mango {zone} frame length");
        }
        let mut saved = state.lock().unwrap();
        let paths = self.paths(zone, count, first);
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
        self.write("ring", RING_COUNT, RING_FIRST, frame, &self.ring)
    }

    fn show_logo(&self, frame: &[u8]) -> Result<()> {
        self.write("logo", LOGO_COUNT, LOGO_FIRST, frame, &self.logo)
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
        let root = std::env::temp_dir().join(format!("luna-mango-{name}-{}", std::process::id()));
        for (zone, first, count) in [
            ("ring", RING_FIRST, RING_COUNT),
            ("logo", LOGO_FIRST, LOGO_COUNT),
        ] {
            for i in 0..count {
                for c in ["red", "green", "blue"] {
                    let p = root.join(format!("mango:{c}:{zone}{}", first + i));
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
        b.show(&vec![42; RING_COUNT * 3]).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("mango:red:logo0/brightness")).unwrap(),
            "17"
        );
        b.show_logo(&vec![23; LOGO_COUNT * 3]).unwrap();
        b.restore_ring_trigger().unwrap();
        b.restore_logo_trigger().unwrap();
        for p in b
            .paths("ring", RING_COUNT, RING_FIRST)
            .into_iter()
            .chain(b.paths("logo", LOGO_COUNT, LOGO_FIRST))
        {
            assert_eq!(fs::read_to_string(p.join("brightness")).unwrap(), "17");
            assert!(fs::read_to_string(p.join("trigger"))
                .unwrap()
                .starts_with("ledfb-"));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepts_both_trigger_spellings() {
        assert_eq!(selected("[ledfb-ring] none").unwrap(), "ledfb-ring");
        assert_eq!(selected("ledfb-ring\n").unwrap(), "ledfb-ring");
        assert_eq!(selected("none").unwrap(), "none");
        assert!(selected("ledfb-ring none").is_err());
        assert!(selected("").is_err());
    }

    #[test]
    fn validates_layout_and_lengths() {
        let root = fixture("validate");
        let b = Indicators::open(&root).unwrap();
        assert!(b.show(&[0; RING_COUNT * 3 - 1]).is_err());
        assert!(b.show_logo(&[0; LOGO_COUNT * 3 + 1]).is_err());
        fs::remove_file(root.join("mango:blue:ring4/brightness")).unwrap();
        assert!(Indicators::open(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn does_not_overwrite_a_new_owner() {
        let root = fixture("owner");
        let b = Indicators::open(&root).unwrap();
        b.show(&[42; RING_COUNT * 3]).unwrap();
        fs::write(root.join("mango:red:ring1/brightness"), "99").unwrap();
        b.restore_ring_trigger().unwrap();
        assert_eq!(
            fs::read_to_string(root.join("mango:red:ring1/brightness")).unwrap(),
            "99"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
