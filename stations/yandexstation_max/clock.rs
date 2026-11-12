//! Experimental Max matrix clock; ownership and physical layout need hardware validation.
use anyhow::{bail, Result};
use luna::{ClockBackend, ClockCommand};
use std::{fs::File, os::unix::fs::FileExt, sync::Mutex};

const WIDTH: usize = 25;
const HEIGHT: usize = 16;
const BYTES: usize = WIDTH * HEIGHT * 3;

pub fn create_clock_backend() -> Option<Box<dyn ClockBackend>> {
    let file = File::options().read(true).write(true).open("/dev/graphics/fb4").ok()?;
    Some(Box::new(Matrix { state: Mutex::new((file, None)) }))
}

struct Matrix { state: Mutex<(File, Option<Vec<u8>>)> }

fn render(text: &str, brightness: u8) -> Result<Vec<u8>> {
    let data = text.as_bytes();
    if data.len() != 5 || data[2] != b':' || data.iter().enumerate().any(|(i, b)| i != 2 && !b.is_ascii_digit()) {
        bail!("time must be HH:MM");
    }
    let hour = (data[0] - b'0') * 10 + data[1] - b'0';
    let minute = (data[3] - b'0') * 10 + data[4] - b'0';
    if hour > 23 || minute > 59 { bail!("time out of range"); }
    let font: [[u8; 5]; 10] = [
        [7,5,5,5,7], [2,6,2,2,7], [7,1,7,4,7], [7,1,7,1,7], [5,5,7,1,1],
        [7,4,7,1,7], [7,4,7,5,7], [7,1,1,1,1], [7,5,7,5,7], [7,5,7,1,7],
    ];
    let mut pixels = vec![0; BYTES];
    let x_positions = [4, 8, 14, 18];
    for (index, digit) in [data[0], data[1], data[3], data[4]].iter().enumerate() {
        for (y, row) in font[(digit - b'0') as usize].iter().enumerate() {
            for x in 0..3 {
                if row & (1 << (2-x)) != 0 {
                    let offset = ((y + 5) * WIDTH + x_positions[index] + x) * 3;
                    pixels[offset..offset+3].fill(brightness);
                }
            }
        }
    }
    for y in [6, 8] { let offset = (y * WIDTH + 12) * 3; pixels[offset..offset+3].fill(brightness); }
    Ok(pixels)
}

impl Matrix {
    fn write(&self, frame: &[u8]) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("matrix lock poisoned"))?;
        let mut current = vec![0; BYTES];
        state.0.read_exact_at(&mut current, 0)?;
        if state.1.as_deref() != Some(current.as_slice()) && current.iter().any(|&b| b != 0) {
            state.1 = None;
            bail!("matrix is in use by firmware");
        }
        state.0.write_all_at(frame, 0)?;
        state.1 = Some(frame.to_vec());
        Ok(())
    }
    fn release(&self) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("matrix lock poisoned"))?;
        let mut current = vec![0; BYTES];
        state.0.read_exact_at(&mut current, 0)?;
        if state.1.as_deref() == Some(current.as_slice()) { state.0.write_all_at(&[0; BYTES], 0)?; }
        state.1 = None;
        Ok(())
    }
}

impl ClockBackend for Matrix {
    fn segments(&self) -> Vec<String> { vec![] }
    fn apply(&self, command: ClockCommand) -> Result<()> {
        match command {
            ClockCommand::Off => self.release(),
            ClockCommand::Time { text, brightness } => self.write(&render(&text, brightness)?),
            ClockCommand::Segments(_) => bail!("matrix does not expose individual clock segments"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn time_frame_is_bounded_and_validated() {
        let frame = render("12:34", 50).unwrap();
        assert_eq!(frame.len(), BYTES);
        assert!(frame.iter().any(|b| *b == 50));
        assert!(render("24:00", 50).is_err());
        assert!(render("1:00", 50).is_err());
    }
    #[test]
    fn release_leaves_foreign_frame_untouched() {
        let path = std::env::temp_dir().join(format!("luna-max-matrix-{}", std::process::id()));
        std::fs::write(&path, [0; BYTES]).unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        let matrix = Matrix { state: Mutex::new((file, None)) };
        matrix.write(&render("12:34", 50).unwrap()).unwrap();
        std::fs::write(&path, [2; BYTES]).unwrap();
        matrix.release().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), vec![2; BYTES]);
        std::fs::remove_file(path).unwrap();
    }
}
