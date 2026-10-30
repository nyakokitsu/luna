use anyhow::{bail, Context, Result};
use luna::LedBackend;
use std::{fs, path::PathBuf};

const COUNT: usize = 3;
const NATIVE_TRIGGER: &str = "led_ring_fb";

pub fn create_led_backend() -> Result<Box<dyn LedBackend>> {
    Ok(Box::new(SysfsRing {
        root: PathBuf::from("/sys/class/leds"),
    }))
}

struct SysfsRing {
    root: PathBuf,
}

impl SysfsRing {
    fn channel_path(&self, channel: &str, index: usize, attribute: &str) -> PathBuf {
        self.root
            .join(format!("plum:{channel}:ring{index}/{attribute}"))
    }
}

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
                let path = self.channel_path(channel, index + 1, "brightness");
                fs::write(&path, value.to_string())
                    .with_context(|| format!("write {}", path.display()))?;
            }
        }
        Ok(())
    }

    fn restore_ring_trigger(&self) -> Result<()> {
        // Brightness writes detach platformd's native framebuffer trigger.
        // Plum/Prune advertise led_ring_fb (not Midi's ledfb-ring).
        // Validate the entire ring before writing; never touch clock LEDs.
        let mut paths = Vec::new();
        for index in 1..=COUNT {
            for channel in ["red", "green", "blue"] {
                let path = self.channel_path(channel, index, "trigger");
                let triggers = fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?;
                if !triggers
                    .split_whitespace()
                    .any(|name| name.trim_matches(['[', ']']) == NATIVE_TRIGGER)
                {
                    bail!("native LED trigger unavailable at {}", path.display());
                }
                paths.push(path);
            }
        }
        for path in paths {
            fs::write(&path, NATIVE_TRIGGER)
                .with_context(|| format!("restore {}", path.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(SysfsRing);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "luna-plum-led-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            let ring = SysfsRing { root };
            for index in 1..=COUNT {
                for channel in ["red", "green", "blue"] {
                    let path = ring.channel_path(channel, index, "trigger");
                    fs::create_dir(path.parent().unwrap()).unwrap();
                    fs::write(path, "[none] timer led_ring_fb heartbeat\n").unwrap();
                    fs::write(ring.channel_path(channel, index, "brightness"), "23").unwrap();
                }
            }
            let clock = ring.root.join("plum:white:clock1");
            fs::create_dir(&clock).unwrap();
            fs::write(clock.join("trigger"), "[clock]").unwrap();
            fs::write(clock.join("brightness"), "77").unwrap();
            Self(ring)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0.root).unwrap();
        }
    }

    #[test]
    fn restores_native_ring_without_clearing_brightness_or_clock() {
        let f = Fixture::new();
        f.0.restore_ring_trigger().unwrap();
        f.0.restore_ring_trigger().unwrap();
        for index in 1..=COUNT {
            for channel in ["red", "green", "blue"] {
                assert_eq!(
                    fs::read_to_string(f.0.channel_path(channel, index, "trigger")).unwrap(),
                    NATIVE_TRIGGER
                );
                assert_eq!(
                    fs::read_to_string(f.0.channel_path(channel, index, "brightness")).unwrap(),
                    "23"
                );
            }
        }
        assert_eq!(
            fs::read_to_string(f.0.root.join("plum:white:clock1/trigger")).unwrap(),
            "[clock]"
        );
        assert_eq!(
            fs::read_to_string(f.0.root.join("plum:white:clock1/brightness")).unwrap(),
            "77"
        );
    }

    #[test]
    fn unsupported_trigger_refuses_before_any_write() {
        let f = Fixture::new();
        fs::write(
            f.0.channel_path("blue", COUNT, "trigger"),
            "[none] timer ledfb-ring",
        )
        .unwrap();
        assert!(f.0.restore_ring_trigger().is_err());
        assert_eq!(
            fs::read_to_string(f.0.channel_path("red", 1, "trigger")).unwrap(),
            "[none] timer led_ring_fb heartbeat\n"
        );
    }
}
