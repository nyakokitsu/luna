//! Experimental Max ring. Byte order and firmware ownership need device tests.
use anyhow::{bail, Context, Result};
use luna::LedBackend;
use std::{fs::{self, File}, os::unix::fs::FileExt, path::Path, sync::Mutex};

const BYTES: usize = 24 * 3;

fn check(root: &Path) -> Result<()> {
    for (file, expected) in [("name", "YNDX LED ring"), ("virtual_size", "24,1"), ("bits_per_pixel", "24"), ("stride", "72")] {
        let value = fs::read_to_string(root.join(file)).with_context(|| format!("read {file}"))?;
        if value.trim() != expected { bail!("unexpected Max framebuffer {file}: {:?}", value.trim()); }
    }
    Ok(())
}

pub fn preflight() -> Result<()> {
    check(Path::new("/sys/class/graphics/fb5"))?;
    let screen = Path::new("/sys/class/graphics/fb4");
    for (file, expected) in [("name", "YNDX LED screen"), ("virtual_size", "25,16"), ("bits_per_pixel", "24"), ("stride", "75")] {
        let value = fs::read_to_string(screen.join(file)).with_context(|| format!("read screen {file}"))?;
        if value.trim() != expected { bail!("unexpected Max screen {file}: {:?}", value.trim()); }
    }
    Ok(())
}

pub fn create_led_backend() -> Result<Box<dyn LedBackend>> {
    check(Path::new("/sys/class/graphics/fb5"))?;
    let file = File::options().read(true).write(true).open("/dev/graphics/fb5")?;
    Ok(Box::new(Ring { state: Mutex::new((file, None)) }))
}

struct Ring { state: Mutex<(File, Option<Vec<u8>>)> }
impl LedBackend for Ring {
    fn show(&self, frame: &[u8]) -> Result<()> {
        if frame.len() != BYTES { bail!("invalid ring frame length"); }
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("ring lock poisoned"))?;
        let mut current = vec![0; BYTES];
        state.0.read_exact_at(&mut current, 0)?;
        let owned = state.1.as_deref() == Some(current.as_slice());
        if !owned && current.iter().any(|&b| b != 0) {
            state.1 = None;
            bail!("ring is in use by firmware");
        }
        state.0.write_all_at(frame, 0)?;
        state.1 = Some(frame.to_vec());
        Ok(())
    }
    fn restore_ring_trigger(&self) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("ring lock poisoned"))?;
        let mut current = vec![0; BYTES];
        state.0.read_exact_at(&mut current, 0)?;
        if state.1.as_deref() == Some(current.as_slice()) {
            state.0.write_all_at(&[0; BYTES], 0)?;
        }
        state.1 = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_all_framebuffer_metadata_before_open() {
        let dir = std::env::temp_dir().join(format!("luna-max-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for (key, value) in [("name", "YNDX LED ring"), ("virtual_size", "24,1"), ("bits_per_pixel", "24"), ("stride", "72")] { fs::write(dir.join(key), value).unwrap(); }
        assert!(check(&dir).is_ok());
        fs::write(dir.join("stride"), "96").unwrap();
        assert!(check(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_foreign_frame_and_does_not_erase_it_on_release() {
        let path = std::env::temp_dir().join(format!("luna-max-ring-{}", std::process::id()));
        fs::write(&path, [0; BYTES]).unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        let ring = Ring { state: Mutex::new((file, None)) };
        ring.show(&[1; BYTES]).unwrap();
        fs::write(&path, [2; BYTES]).unwrap();
        assert!(ring.show(&[3; BYTES]).is_err());
        ring.restore_ring_trigger().unwrap();
        assert_eq!(fs::read(&path).unwrap(), vec![2; BYTES]);
        fs::remove_file(path).unwrap();
    }
}
