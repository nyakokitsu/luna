use anyhow::{bail, Context, Result};
use std::{fs, path::Path};

const MAX_AUDIO_BYTES: usize = 64 * 1024 * 1024;
const MAX_PLAYBACK_SECONDS: u64 = 600;

pub fn available() -> bool {
    if playback_pcm().is_some() {
        return std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join("aplay").is_file()))
            .unwrap_or(false);
    }
    Path::new("/system/vendor/quasar/maind").is_file()
        && Path::new("/system/vendor/quasar/quasar.cfg").is_file()
}

pub(crate) fn playback_pcm() -> Option<String> {
    std::env::var("LUNA_AUDIO_PCM")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

pub(crate) fn play_alsa(path: &Path, pcm: &str) -> Result<()> {
    play_alsa_cancel(path, pcm, &std::sync::atomic::AtomicBool::new(false))
}

pub(crate) fn play_alsa_cancel(path: &Path, pcm: &str, cancelled: &std::sync::atomic::AtomicBool) -> Result<()> {
    use std::sync::atomic::Ordering;
    if cancelled.load(Ordering::Relaxed) { bail!("playback cancelled") }
    let mut child = std::process::Command::new("aplay")
        .args(["-q", "-D", pcm])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("start ALSA playback")?;
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(MAX_PLAYBACK_SECONDS + 10);
    loop {
        if cancelled.load(Ordering::Relaxed) {
            let _ = child.kill(); let _ = child.wait(); bail!("playback cancelled")
        }
        if let Some(status) = child.try_wait().context("wait for ALSA playback")? {
            if !status.success() {
                bail!("ALSA playback failed: {status}")
            }
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("ALSA playback timed out")
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

pub(crate) fn validate_wav(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_AUDIO_BYTES {
        bail!("audio file exceeds 64 MiB")
    }
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("audio playback accepts PCM WAV files only")
    }
    let riff_end = u32::from_le_bytes(bytes[4..8].try_into()?) as usize + 8;
    if riff_end > bytes.len() {
        bail!("WAV file is truncated")
    }
    let mut offset = 12;
    let mut format = None;
    let mut data_len = None;
    while offset + 8 <= riff_end {
        let chunk_len = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize;
        let start = offset + 8;
        let end = start
            .checked_add(chunk_len)
            .context("WAV chunk size overflow")?;
        if end > riff_end {
            bail!("WAV chunk extends beyond the RIFF container")
        }
        if &bytes[offset..offset + 4] == b"fmt " && chunk_len >= 16 {
            let encoding = u16::from_le_bytes(bytes[start..start + 2].try_into()?);
            let channels = u16::from_le_bytes(bytes[start + 2..start + 4].try_into()?);
            let rate = u32::from_le_bytes(bytes[start + 4..start + 8].try_into()?);
            let bits = u16::from_le_bytes(bytes[start + 14..start + 16].try_into()?);
            if ![1, 3].contains(&encoding)
                || !(1..=2).contains(&channels)
                || !(8_000..=192_000).contains(&rate)
                || ![8, 16, 24, 32].contains(&bits)
            {
                bail!("WAV must be mono/stereo PCM or float at a supported sample rate")
            }
            format = Some((u64::from(channels), u64::from(rate), u64::from(bits)));
        }
        if &bytes[offset..offset + 4] == b"data" {
            data_len = Some(chunk_len as u64);
        }
        offset = end + (chunk_len & 1);
    }
    let (channels, rate, bits) = format.context("WAV format chunk is missing")?;
    let data_len = data_len.context("WAV audio data chunk is missing")?;
    let bytes_per_second = channels * u64::from(rate) * bits / 8;
    if bytes_per_second == 0 || data_len == 0 {
        bail!("WAV audio data is empty")
    }
    if data_len.div_ceil(bytes_per_second) > MAX_PLAYBACK_SECONDS {
        bail!("audio playback is limited to 600 seconds")
    }
    Ok(())
}

pub(crate) fn validate_file(path: &Path) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("read audio file {}", path.display()))?;
    validate_wav(&bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_wav;
    #[test]
    fn rejects_non_wav_audio() {
        assert!(validate_wav(b"not audio").is_err());
    }
}
