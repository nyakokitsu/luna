//! Station 2 ring: a single 89x1 XRGB framebuffer, no sysfs LED nodes.
//!
//! The firmware and other indicators also paint this framebuffer. Luna does not
//! claim exclusive ownership: frames are written only when the current content
//! is blank or was written by this backend, an already displayed frame is not
//! rewritten, and a foreign frame is left untouched.

use anyhow::{bail, Context, Result};
use luna::LedBackend;
use std::{
    fs::{self, File},
    os::unix::fs::FileExt,
    path::Path,
    sync::Mutex,
};

const LED_COUNT: usize = 89;
const FRAME_BYTES: usize = LED_COUNT * 4;
const FB_PATH: &str = "/dev/fb0";
const FB_SIZE_PATH: &str = "/sys/class/graphics/fb0/virtual_size";

pub fn create_led_backend() -> Result<Box<dyn LedBackend>> {
    Ok(Box::new(open(Path::new(FB_PATH), Path::new(FB_SIZE_PATH))?))
}

fn open(path: &Path, size_path: &Path) -> Result<Framebuffer> {
    let geometry = fs::read_to_string(size_path)
        .with_context(|| format!("read {}", size_path.display()))?;
    if geometry.trim() != "89,1" {
        bail!("unexpected Station 2 framebuffer geometry {:?}", geometry.trim());
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
        if frame.len() != LED_COUNT * 3 {
            bail!("unexpected Station 2 LED frame length");
        }
        let mut pixels = Vec::with_capacity(FRAME_BYTES);
        for led in frame.chunks_exact(3) {
            pixels.extend_from_slice(&[led[0], led[1], led[2], 0xff]);
        }
        let file = self.file.lock().unwrap();
        let mut current = [0u8; FRAME_BYTES];
        file.read_exact_at(&mut current, 0)
            .context("read Station 2 framebuffer")?;
        let mut last = self.last_frame.lock().unwrap();
        let ours = last.as_deref() == Some(&current[..]);
        if !ours && current.iter().any(|value| *value != 0) {
            // Firmware/Alice/Jarvis is drawing: never fight it.
            *last = None;
            bail!("Station 2 framebuffer belongs to firmware or another indicator")
        }
        if ours && current[..] == pixels[..] {
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

    fn fixture(name: &str, geometry: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "luna-station2-{name}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let fb = dir.join("fb0");
        fs::write(&fb, vec![0u8; FRAME_BYTES]).unwrap();
        let size = dir.join("virtual_size");
        fs::write(&size, geometry).unwrap();
        (fb, size)
    }

    #[test]
    fn rejects_other_geometry() {
        let (fb, size) = fixture("geometry", "24,1");
        assert!(open(&fb, &size).is_err());
        fs::remove_dir_all(fb.parent().unwrap()).unwrap();
    }

    #[test]
    fn writes_xrgb_and_skips_redundant_frames() {
        let (fb, size) = fixture("write", "89,1");
        let backend = open(&fb, &size).unwrap();
        let frame: Vec<u8> = (0..LED_COUNT)
            .flat_map(|_| [200u8, 0, 200])
            .collect();
        backend.show(&frame).unwrap();
        let written = fs::read(&fb).unwrap();
        assert_eq!(written.len(), FRAME_BYTES);
        assert_eq!(&written[..4], &[200, 0, 200, 0xff]);
        // Second identical frame must not touch the file (mtime stays).
        let before = fs::metadata(&fb).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        backend.show(&frame).unwrap();
        assert_eq!(before, fs::metadata(&fb).unwrap().modified().unwrap());
        fs::remove_dir_all(fb.parent().unwrap()).unwrap();
    }

    #[test]
    fn yields_to_foreign_frame() {
        let (fb, size) = fixture("foreign", "89,1");
        let backend = open(&fb, &size).unwrap();
        let mut foreign = vec![0u8; FRAME_BYTES];
        foreign[..4].copy_from_slice(&[0, 90, 0, 0xff]);
        fs::write(&fb, &foreign).unwrap();
        let frame = vec![200u8; LED_COUNT * 3];
        assert!(backend.show(&frame).is_err());
        assert_eq!(fs::read(&fb).unwrap(), foreign);
        fs::remove_dir_all(fb.parent().unwrap()).unwrap();
    }
}
