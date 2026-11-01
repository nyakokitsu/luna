//! Station 3 ring: a 36x1 packed-RGB framebuffer, not a sysfs LED array.
//!
//! `/sys/class/leds/orion:{red,green,blue}:ring0..35` exist, but they carry the
//! `ledfb-ring` trigger, which mirrors `/dev/fb2` into them, so a direct
//! `brightness` write is reverted a moment later. Luna therefore paints the
//! framebuffer, exactly like the Mini 3 ring (`fb0 6x1`, 24 bpp).
//!
//! The panel is three bytes per LED (RGB), 36 pixels: 108 bytes. The emulator
//! and the firmware both read the framebuffer that way.
//!
//! The firmware and other indicators also paint this framebuffer. Luna only
//! writes a blank framebuffer or one previously written by this backend.

use anyhow::{bail, Context, Result};
use luna::LedBackend;
use std::{
    fs::{self, File},
    os::unix::fs::FileExt,
    path::Path,
    sync::Mutex,
};

const LED_COUNT: usize = 36;
const FRAME_BYTES: usize = LED_COUNT * 3;
const FB_PATH: &str = "/dev/fb2";
const FB_SIZE_PATH: &str = "/sys/class/graphics/fb2/virtual_size";
const FB_DEPTH_PATH: &str = "/sys/class/graphics/fb2/bits_per_pixel";

pub fn create_led_backend() -> Result<Box<dyn LedBackend>> {
    Ok(Box::new(open(
        Path::new(FB_PATH),
        Path::new(FB_SIZE_PATH),
        Path::new(FB_DEPTH_PATH),
    )?))
}

fn open(path: &Path, size_path: &Path, depth_path: &Path) -> Result<Framebuffer> {
    let geometry = fs::read_to_string(size_path)
        .with_context(|| format!("read {}", size_path.display()))?;
    if geometry.trim() != "36,1" {
        bail!("unexpected Station 3 framebuffer geometry {:?}", geometry.trim());
    }
    let depth = fs::read_to_string(depth_path)
        .with_context(|| format!("read {}", depth_path.display()))?;
    if depth.trim() != "24" {
        bail!("unexpected Station 3 framebuffer depth {:?}", depth.trim());
    }
    let file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    Ok(Framebuffer {
        file: Mutex::new(file),
        last_frame: Mutex::new(None),
    })
}

struct Framebuffer {
    file: Mutex<File>,
    last_frame: Mutex<Option<Vec<u8>>>,
}

impl LedBackend for Framebuffer {
    fn show(&self, frame: &[u8]) -> Result<()> {
        if frame.len() != FRAME_BYTES {
            bail!("unexpected Station 3 ring frame length {}", frame.len());
        }
        let file = self.file.lock().unwrap();
        let mut current = [0u8; FRAME_BYTES];
        file.read_exact_at(&mut current, 0)
            .context("read Station 3 framebuffer")?;
        let mut last = self.last_frame.lock().unwrap();
        let ours = last.as_deref() == Some(&current[..]);
        if !ours && current.iter().any(|value| *value != 0) {
            *last = None;
            bail!("Station 3 framebuffer belongs to firmware or another indicator");
        }
        if ours && current[..] == frame[..] {
            return Ok(());
        }
        file.write_all_at(frame, 0)
            .context("write Station 3 framebuffer")?;
        *last = Some(frame.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str, geometry: &str, depth: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "luna-station3-{name}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let fb = dir.join("fb2");
        fs::write(&fb, vec![0u8; FRAME_BYTES]).unwrap();
        let size = dir.join("virtual_size");
        fs::write(&size, geometry).unwrap();
        let depth_path = dir.join("bits_per_pixel");
        fs::write(&depth_path, depth).unwrap();
        (fb, size, depth_path)
    }

    #[test]
    fn rejects_other_geometry() {
        let (fb, size, depth) = fixture("geometry", "24,1", "24");
        assert!(open(&fb, &size, &depth).is_err());
        fs::remove_dir_all(fb.parent().unwrap()).unwrap();
    }

    #[test]
    fn rejects_other_depth() {
        let (fb, size, depth) = fixture("depth", "36,1", "32");
        assert!(open(&fb, &size, &depth).is_err());
        fs::remove_dir_all(fb.parent().unwrap()).unwrap();
    }

    #[test]
    fn writes_packed_rgb_and_skips_redundant_frames() {
        let (fb, size, depth) = fixture("write", "36,1", "24");
        let backend = open(&fb, &size, &depth).unwrap();
        let frame: Vec<u8> = (0..LED_COUNT).flat_map(|_| [200u8, 0, 200]).collect();
        backend.show(&frame).unwrap();
        let written = fs::read(&fb).unwrap();
        assert_eq!(written.len(), FRAME_BYTES);
        assert_eq!(&written[..3], &[200, 0, 200]);
        assert_eq!(&written[FRAME_BYTES - 3..], &[200, 0, 200]);
        let before = fs::metadata(&fb).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        backend.show(&frame).unwrap();
        assert_eq!(before, fs::metadata(&fb).unwrap().modified().unwrap());
        fs::remove_dir_all(fb.parent().unwrap()).unwrap();
    }

    #[test]
    fn yields_to_foreign_frame() {
        let (fb, size, depth) = fixture("foreign", "36,1", "24");
        let backend = open(&fb, &size, &depth).unwrap();
        let mut foreign = vec![0u8; FRAME_BYTES];
        foreign[..3].copy_from_slice(&[0, 90, 0]);
        fs::write(&fb, &foreign).unwrap();
        let frame = vec![200u8; FRAME_BYTES];
        assert!(backend.show(&frame).is_err());
        assert_eq!(fs::read(&fb).unwrap(), foreign);
        fs::remove_dir_all(fb.parent().unwrap()).unwrap();
    }
}
