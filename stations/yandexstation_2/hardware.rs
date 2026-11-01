//! Station 2 ring: an 89x1 framebuffer, not a sysfs LED array.
//!
//! The board has no `station2:*:ring*` nodes that matter: the ring is a
//! framebuffer (`fb0`, named "ring") of 89 pixels. The emulator exposes it at
//! 24 bpp (three bytes per LED), while the firmware framebuffer is 32 bpp, so
//! the backend follows `bits_per_pixel` and writes packed RGB or RGBX as the
//! device expects. Writing four bytes into a three-byte framebuffer shifted
//! every colour from the second LED on.
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

const LED_COUNT: usize = 89;
const FB_PATH: &str = "/dev/fb0";
const FB_SIZE_PATH: &str = "/sys/class/graphics/fb0/virtual_size";
const FB_DEPTH_PATH: &str = "/sys/class/graphics/fb0/bits_per_pixel";

pub fn create_led_backend() -> Result<Box<dyn LedBackend>> {
    Ok(Box::new(open(
        Path::new(FB_PATH),
        Path::new(FB_SIZE_PATH),
        Path::new(FB_DEPTH_PATH),
    )?))
}

fn open(path: &Path, size_path: &Path, depth_path: &Path) -> Result<Framebuffer> {
    let geometry =
        fs::read_to_string(size_path).with_context(|| format!("read {}", size_path.display()))?;
    if geometry.trim() != "89,1" {
        bail!(
            "unexpected Station 2 framebuffer geometry {:?}",
            geometry.trim()
        );
    }
    let depth =
        fs::read_to_string(depth_path).with_context(|| format!("read {}", depth_path.display()))?;
    let bytes_per_pixel = match depth.trim() {
        "24" => 3,
        "32" => 4,
        other => bail!("unsupported Station 2 framebuffer depth {other:?}"),
    };
    let file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    Ok(Framebuffer {
        file: Mutex::new(file),
        last_frame: Mutex::new(None),
        bytes_per_pixel,
        frame_bytes: LED_COUNT * bytes_per_pixel,
    })
}

struct Framebuffer {
    file: Mutex<File>,
    last_frame: Mutex<Option<Vec<u8>>>,
    bytes_per_pixel: usize,
    frame_bytes: usize,
}

impl Framebuffer {
    fn encode(&self, frame: &[u8]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity(self.frame_bytes);
        for led in frame.chunks_exact(3) {
            pixels.extend_from_slice(led);
            // 32-bpp framebuffers keep one unused high byte per pixel.
            if self.bytes_per_pixel == 4 {
                pixels.push(0xff);
            }
        }
        pixels
    }
}

impl LedBackend for Framebuffer {
    fn show(&self, frame: &[u8]) -> Result<()> {
        if frame.len() != LED_COUNT * 3 {
            bail!("unexpected Station 2 LED frame length {}", frame.len());
        }
        let file = self.file.lock().unwrap();
        let mut current = vec![0u8; self.frame_bytes];
        file.read_exact_at(&mut current, 0)
            .context("read Station 2 framebuffer")?;
        let mut last = self.last_frame.lock().unwrap();
        let ours = last.as_deref() == Some(&current[..]);
        if !ours && current.iter().any(|value| *value != 0) {
            *last = None;
            bail!("Station 2 framebuffer belongs to firmware or another indicator");
        }
        let pixels = self.encode(frame);
        if ours && current == pixels {
            return Ok(());
        }
        file.write_all_at(&pixels, 0)
            .context("write Station 2 framebuffer")?;
        *last = Some(pixels);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str, geometry: &str, depth: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("luna-station2-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let fb = dir.join("fb0");
        let bytes = if depth == "24" {
            LED_COUNT * 3
        } else {
            LED_COUNT * 4
        };
        fs::write(&fb, vec![0u8; bytes]).unwrap();
        let size = dir.join("virtual_size");
        fs::write(&size, geometry).unwrap();
        let depth_path = dir.join("bits_per_pixel");
        fs::write(&depth_path, depth).unwrap();
        (fb, size, depth_path)
    }

    fn cleanup(fb: &Path) {
        fs::remove_dir_all(fb.parent().unwrap()).unwrap();
    }

    #[test]
    fn rejects_other_geometry() {
        let (fb, size, depth) = fixture("geometry", "24,1", "24");
        assert!(open(&fb, &size, &depth).is_err());
        cleanup(&fb);
    }

    #[test]
    fn rejects_unusable_depth() {
        let (fb, size, depth) = fixture("depth", "89,1", "16");
        assert!(open(&fb, &size, &depth).is_err());
        cleanup(&fb);
    }

    #[test]
    fn writes_packed_rgb_at_24_bpp() {
        let (fb, size, depth) = fixture("24", "89,1", "24");
        let backend = open(&fb, &size, &depth).unwrap();
        let frame: Vec<u8> = (0..LED_COUNT).flat_map(|_| [200u8, 0, 200]).collect();
        backend.show(&frame).unwrap();
        let written = fs::read(&fb).unwrap();
        assert_eq!(written.len(), LED_COUNT * 3);
        assert_eq!(&written[..3], &[200, 0, 200]);
        assert_eq!(&written[LED_COUNT * 3 - 3..], &[200, 0, 200]);
        cleanup(&fb);
    }

    #[test]
    fn writes_rgbx_at_32_bpp() {
        let (fb, size, depth) = fixture("32", "89,1", "32");
        let backend = open(&fb, &size, &depth).unwrap();
        let frame: Vec<u8> = (0..LED_COUNT).flat_map(|_| [10u8, 20, 30]).collect();
        backend.show(&frame).unwrap();
        let written = fs::read(&fb).unwrap();
        assert_eq!(written.len(), LED_COUNT * 4);
        assert_eq!(&written[..4], &[10, 20, 30, 0xff]);
        assert_eq!(&written[LED_COUNT * 4 - 4..], &[10, 20, 30, 0xff]);
        cleanup(&fb);
    }

    #[test]
    fn skips_redundant_frames() {
        let (fb, size, depth) = fixture("redundant", "89,1", "24");
        let backend = open(&fb, &size, &depth).unwrap();
        let frame: Vec<u8> = (0..LED_COUNT).flat_map(|_| [1u8, 2, 3]).collect();
        backend.show(&frame).unwrap();
        let before = fs::metadata(&fb).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        backend.show(&frame).unwrap();
        assert_eq!(before, fs::metadata(&fb).unwrap().modified().unwrap());
        cleanup(&fb);
    }

    #[test]
    fn yields_to_foreign_frame() {
        let (fb, size, depth) = fixture("foreign", "89,1", "24");
        let backend = open(&fb, &size, &depth).unwrap();
        let mut foreign = vec![0u8; LED_COUNT * 3];
        foreign[..3].copy_from_slice(&[0, 90, 0]);
        fs::write(&fb, &foreign).unwrap();
        let frame = vec![200u8; LED_COUNT * 3];
        assert!(backend.show(&frame).is_err());
        assert_eq!(fs::read(&fb).unwrap(), foreign);
        cleanup(&fb);
    }
}
