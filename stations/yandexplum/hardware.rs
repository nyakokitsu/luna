use anyhow::{bail, Context, Result};
use luna::LedBackend;
use std::{fs, path::Path};

const COUNT: usize = 3;

pub fn create_led_backend() -> Result<Box<dyn LedBackend>> {
    Ok(Box::new(SysfsRing))
}

struct SysfsRing;

impl LedBackend for SysfsRing {
    fn show(&self, frame: &[u8]) -> Result<()> {
        if frame.len() != COUNT * 3 {
            bail!("unexpected Yandex Plum LED frame length");
        }
        for index in 0..COUNT {
            for (channel, value) in ["red", "green", "blue"]
                .iter()
                .zip(&frame[index * 3..index * 3 + 3])
            {
                let path = format!(
                    "/sys/class/leds/plum:{channel}:ring{}/brightness",
                    index + 1
                );
                fs::write(&path, value.to_string()).with_context(|| format!("write {path}"))?;
            }
        }
        Ok(())
    }

    fn restore_ring_trigger(&self) -> Result<()> {
        restore_ring_trigger(Path::new("/sys/class/leds"))
    }
}

fn restore_ring_trigger(root: &Path) -> Result<()> {
    // Brightness writes detach the firmware renderer on both Light 2 variants.
    // Validate every RGB channel before touching any; never access white clock
    // segments or clear the firmware framebuffer while returning ownership.
    let mut paths = Vec::new();
    for index in 1..=COUNT {
        for channel in ["red", "green", "blue"] {
            let path = root.join(format!("plum:{channel}:ring{index}/trigger"));
            let options = fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            if !options.split_whitespace().any(|name| name.trim_matches(['[', ']']) == "led_ring_fb") {
                bail!("verified Plum framebuffer trigger is unavailable")
            }
            paths.push(path);
        }
    }
    for path in paths {
        fs::write(&path, "led_ring_fb").with_context(|| format!("restore {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_returns_all_rgb_without_touching_clock() {
        let root = std::env::temp_dir().join(format!("luna-plum-release-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for index in 1..=COUNT {
            for channel in ["red", "green", "blue"] {
                let dir = root.join(format!("plum:{channel}:ring{index}"));
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join("trigger"), "[none] timer led_ring_fb").unwrap();
            }
        }
        let clock = root.join("plum:white:clockA0");
        fs::create_dir_all(&clock).unwrap();
        fs::write(clock.join("brightness"), "73").unwrap();
        restore_ring_trigger(&root).unwrap();
        for index in 1..=COUNT {
            for channel in ["red", "green", "blue"] {
                assert_eq!(fs::read_to_string(root.join(format!("plum:{channel}:ring{index}/trigger"))).unwrap(), "led_ring_fb");
            }
        }
        assert_eq!(fs::read_to_string(clock.join("brightness")).unwrap(), "73");
        // A missing last channel must be caught before any earlier write.
        for index in 1..=COUNT {
            for channel in ["red", "green", "blue"] {
                fs::write(root.join(format!("plum:{channel}:ring{index}/trigger")), "[none] led_ring_fb").unwrap();
            }
        }
        fs::remove_file(root.join("plum:blue:ring3/trigger")).unwrap();
        assert!(restore_ring_trigger(&root).is_err());
        assert_eq!(fs::read_to_string(root.join("plum:red:ring1/trigger")).unwrap(), "[none] led_ring_fb");
        fs::remove_dir_all(root).unwrap();
    }
}
