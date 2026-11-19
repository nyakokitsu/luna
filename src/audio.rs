use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};
use url::Url;

const MAX_AUDIO_BYTES: usize = 64 * 1024 * 1024;
const MAX_PLAYBACK_SECONDS: u64 = 600;
const MAX_URL_REDIRECTS: usize = 5;
const MAX_STREAM_HEADER_BYTES: usize = 1024 * 1024;
const URL_CONNECT_TIMEOUT_SECONDS: u64 = 10;
const URL_TIMEOUT_SECONDS: u64 = 120;
const STREAM_TIMEOUT_SECONDS: u64 = MAX_PLAYBACK_SECONDS + 30;
const STREAM_HEADER_TIMEOUT_SECONDS: u64 = 30;

/// Host allowlist for HTTP(S) audio sources.
///
/// Unlike `alice.http`, audio fetches may target private or loopback addresses
/// when the package manifest or the administrator explicitly lists the host:
/// the response is only decoded as WAV and played, never returned to the
/// caller. The allowlist remains the consent boundary.
#[derive(Debug, Clone, Default)]
pub(crate) struct UrlPolicy {
    pub(crate) allow: Vec<String>,
    pub(crate) allow_insecure_http: bool,
}

pub(crate) fn url_allowed(policy: &UrlPolicy, value: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid audio URL")?;
    match url.scheme() {
        "https" => {}
        "http" if policy.allow_insecure_http => {}
        "http" => bail!("HTTP audio URLs require allow_insecure_http = true"),
        _ => bail!("only HTTP(S) audio URLs are allowed"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("audio URLs with credentials are not allowed")
    }
    let host = url
        .host_str()
        .context("audio URL host is required")?
        .to_ascii_lowercase();
    if policy.allow.is_empty() {
        bail!("audio URL host list is empty")
    }
    let allowed = policy.allow.iter().any(|entry| {
        let entry = entry.to_ascii_lowercase();
        entry == "*"
            || entry == host
            || entry
                .strip_prefix("*.")
                .is_some_and(|suffix| host.ends_with(&format!(".{suffix}")))
    });
    if !allowed {
        bail!("audio URL host is not allowed")
    }
    Ok(url)
}

fn http_client(total_timeout: Option<Duration>) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(URL_CONNECT_TIMEOUT_SECONDS))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("luna/", env!("CARGO_PKG_VERSION")));
    if let Some(timeout) = total_timeout {
        builder = builder.timeout(timeout);
    }
    Ok(builder.build()?)
}

fn open_stream(
    policy: &UrlPolicy,
    value: &str,
    total_timeout: Option<Duration>,
) -> Result<reqwest::blocking::Response> {
    let client = http_client(total_timeout)?;
    let mut url = url_allowed(policy, value)?;
    for _ in 0..=MAX_URL_REDIRECTS {
        let response = client
            .get(url.clone())
            .send()
            .with_context(|| format!("request audio URL {url}"))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .context("audio URL redirect has no location")?;
            let next = url
                .join(location)
                .context("invalid audio URL redirect target")?;
            url = url_allowed(policy, next.as_str())?;
            continue;
        }
        if !response.status().is_success() {
            bail!("audio URL returned HTTP {}", response.status())
        }
        return Ok(response);
    }
    bail!("audio URL has too many redirects")
}

/// Download a bounded WAV file from an allowlisted HTTP(S) URL.
pub(crate) fn fetch_url(policy: &UrlPolicy, value: &str) -> Result<Vec<u8>> {
    let response = open_stream(
        policy,
        value,
        Some(Duration::from_secs(URL_TIMEOUT_SECONDS)),
    )?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_AUDIO_BYTES as u64)
    {
        bail!("audio file exceeds 64 MiB")
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_AUDIO_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("read audio URL")?;
    if bytes.len() > MAX_AUDIO_BYTES {
        bail!("audio file exceeds 64 MiB")
    }
    validate_wav(&bytes)?;
    Ok(bytes)
}

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

fn spawn_alsa(pcm: &str, input: Option<&Path>) -> Result<Child> {
    let mut command = Command::new("aplay");
    command.args(["-q", "-D", pcm]);
    match input {
        Some(path) => {
            command.arg(path);
        }
        None => {
            command.arg("-");
            command.stdin(Stdio::piped());
        }
    }
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start ALSA playback")
}

fn wait_alsa(child: &mut Child, deadline: Instant, cancelled: Option<&AtomicBool>) -> Result<()> {
    loop {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            terminate_alsa(child);
            bail!("playback cancelled")
        }
        if let Some(status) = child.try_wait().context("wait for ALSA playback")? {
            if !status.success() {
                bail!("ALSA playback failed: {status}")
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            terminate_alsa(child);
            bail!("ALSA playback timed out")
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(crate) fn play_alsa(path: &Path, pcm: &str) -> Result<()> {
    play_alsa_cancel(path, pcm, &AtomicBool::new(false))
}

fn terminate_alsa(child: &mut std::process::Child) {
    // Let ALSA detach dmix shared memory before escalating. SIGKILL can leave
    // an unused segment whose stale playback pointers block the next client.
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn play_alsa_cancel(path: &Path, pcm: &str, cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        bail!("playback cancelled")
    }
    let duration = wav_duration(&fs::read(path)?)?;
    let mut child = spawn_alsa(pcm, Some(path))?;
    wait_alsa(
        &mut child,
        Instant::now() + duration + Duration::from_secs(5),
        Some(cancelled),
    )
}

/// Play an already validated WAV payload through ALSA without a temporary file.
pub(crate) fn play_alsa_bytes(bytes: &[u8], pcm: &str) -> Result<()> {
    let duration = wav_duration(bytes)?;
    let mut child = spawn_alsa(pcm, None)?;
    {
        let mut stdin = child
            .stdin
            .take()
            .context("ALSA player stdin is unavailable")?;
        stdin
            .write_all(bytes)
            .context("write audio payload to ALSA player")?;
    }
    wait_alsa(
        &mut child,
        Instant::now() + duration + Duration::from_secs(5),
        None,
    )
}

struct StreamFormat {
    duration: Option<Duration>,
}

fn spawn_stream_reader(
    mut response: reqwest::blocking::Response,
) -> Receiver<Result<Vec<u8>, String>> {
    // Bound the read-ahead so a fast source cannot buffer the whole stream in
    // memory while aplay drains it at playback speed.
    let (sender, receiver) = mpsc::sync_channel(8);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 16 * 1024];
        loop {
            match response.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if sender.send(Ok(buffer[..read].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });
    receiver
}

/// Stream a WAV source from an allowlisted HTTP(S) URL into ALSA.
///
/// Known-length WAV files keep the 600-second cap. A `data` chunk with an
/// unknown length (zero or `0xFFFFFFFF`, as used by live streams) is accepted
/// and capped by wall-clock time and cancellation instead.
pub(crate) fn play_url_stream(
    policy: &UrlPolicy,
    value: &str,
    pcm: &str,
    cancelled: &AtomicBool,
) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        bail!("playback cancelled")
    }
    // The blocking client applies `.timeout` to the whole response, so use a
    // generous cap and enforce the real limit while copying. Cancellation kills
    // the player immediately; the reader thread exits by itself on this timeout.
    let response = open_stream(
        policy,
        value,
        Some(Duration::from_secs(STREAM_TIMEOUT_SECONDS)),
    )?;
    if response.content_length() == Some(0) {
        bail!("audio stream is empty")
    }
    let chunks = spawn_stream_reader(response);
    let header_deadline = Instant::now() + Duration::from_secs(STREAM_HEADER_TIMEOUT_SECONDS);
    let mut prefix = Vec::new();
    let format = loop {
        if cancelled.load(Ordering::Relaxed) {
            bail!("playback cancelled")
        }
        if Instant::now() >= header_deadline {
            bail!("audio stream header timed out")
        }
        if prefix.len() > MAX_STREAM_HEADER_BYTES {
            bail!("audio stream WAV header is too large")
        }
        if let Some(format) = scan_stream_header(&prefix)? {
            break format;
        }
        match chunks.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(chunk)) => prefix.extend_from_slice(&chunk),
            Ok(Err(error)) => bail!("read audio stream: {error}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                bail!("audio stream ended before the WAV data chunk")
            }
        }
    };
    let deadline = Instant::now()
        + format
            .duration
            .unwrap_or(Duration::from_secs(MAX_PLAYBACK_SECONDS))
        + Duration::from_secs(5);
    let mut child = spawn_alsa(pcm, None)?;
    let mut stdin = child
        .stdin
        .take()
        .context("ALSA player stdin is unavailable")?;
    if stdin.write_all(&prefix).is_err() {
        let _ = stdin;
        wait_alsa(&mut child, deadline, Some(cancelled))?;
        return Ok(());
    }
    loop {
        if cancelled.load(Ordering::Relaxed) {
            terminate_alsa(&mut child);
            bail!("playback cancelled")
        }
        if Instant::now() >= deadline {
            terminate_alsa(&mut child);
            bail!("audio stream playback timed out")
        }
        match chunks.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(chunk)) => {
                if stdin.write_all(&chunk).is_err() {
                    break;
                }
            }
            Ok(Err(error)) => {
                terminate_alsa(&mut child);
                bail!("read audio stream: {error}")
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(stdin);
    wait_alsa(&mut child, deadline, Some(cancelled))
}

pub(crate) fn validate_wav(bytes: &[u8]) -> Result<()> {
    wav_duration(bytes).map(|_| ())
}

fn parse_wav_format(bytes: &[u8]) -> Result<(u64, u64, u64)> {
    let encoding = u16::from_le_bytes(bytes[0..2].try_into()?);
    let channels = u16::from_le_bytes(bytes[2..4].try_into()?);
    let rate = u32::from_le_bytes(bytes[4..8].try_into()?);
    let bits = u16::from_le_bytes(bytes[14..16].try_into()?);
    if ![1, 3].contains(&encoding)
        || !(1..=2).contains(&channels)
        || !(8_000..=192_000).contains(&rate)
        || ![8, 16, 24, 32].contains(&bits)
    {
        bail!("WAV must be mono/stereo PCM or float at a supported sample rate")
    }
    Ok((u64::from(channels), u64::from(rate), u64::from(bits)))
}

fn scan_stream_header(buffer: &[u8]) -> Result<Option<StreamFormat>> {
    if buffer.len() >= 4 && &buffer[..4] != b"RIFF" {
        bail!("audio playback accepts PCM WAV files only")
    }
    if buffer.len() < 12 {
        return Ok(None);
    }
    if &buffer[8..12] != b"WAVE" {
        bail!("audio playback accepts PCM WAV files only")
    }
    let mut offset = 12;
    let mut format = None;
    loop {
        if offset + 8 > buffer.len() {
            return Ok(None);
        }
        let chunk = &buffer[offset..offset + 4];
        let declared = u32::from_le_bytes(buffer[offset + 4..offset + 8].try_into()?) as usize;
        let start = offset + 8;
        if chunk == b"fmt " {
            if declared < 16 {
                bail!("WAV format chunk is invalid")
            }
            let end = start
                .checked_add(declared)
                .context("WAV chunk size overflow")?;
            if end > buffer.len() {
                return Ok(None);
            }
            format = Some(parse_wav_format(&buffer[start..start + 16])?);
            offset = end + (declared & 1);
            continue;
        }
        if chunk == b"data" {
            let (channels, rate, bits) = format.context("WAV format chunk is missing")?;
            if declared == 0 || declared == u32::MAX as usize {
                return Ok(Some(StreamFormat { duration: None }));
            }
            let bytes_per_second = channels * rate * bits / 8;
            if bytes_per_second == 0 {
                bail!("WAV audio data is empty")
            }
            let duration =
                Duration::from_millis((declared as u64 * 1000).div_ceil(bytes_per_second));
            if duration > Duration::from_secs(MAX_PLAYBACK_SECONDS) {
                bail!("audio playback is limited to 600 seconds")
            }
            return Ok(Some(StreamFormat {
                duration: Some(duration),
            }));
        }
        if declared == u32::MAX as usize {
            bail!("invalid WAV chunk length")
        }
        let end = start
            .checked_add(declared)
            .context("WAV chunk size overflow")?;
        if end > buffer.len() {
            return Ok(None);
        }
        offset = end + (declared & 1);
    }
}

fn wav_duration(bytes: &[u8]) -> Result<Duration> {
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
            format = Some(parse_wav_format(&bytes[start..start + 16])?);
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
    Ok(Duration::from_millis(
        (data_len * 1000).div_ceil(bytes_per_second),
    ))
}

pub(crate) fn validate_file(path: &Path) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("read audio file {}", path.display()))?;
    validate_wav(&bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{scan_stream_header, url_allowed, validate_wav, wav_duration, UrlPolicy};

    fn wav(data_len: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(b"RIFF");
        bytes.extend((36u32.saturating_add(data_len)).to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(16u32.to_le_bytes());
        bytes.extend(1u16.to_le_bytes());
        bytes.extend(1u16.to_le_bytes());
        bytes.extend(8000u32.to_le_bytes());
        bytes.extend(16000u32.to_le_bytes());
        bytes.extend(2u16.to_le_bytes());
        bytes.extend(16u16.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend(data_len.to_le_bytes());
        bytes
    }

    #[test]
    fn rejects_non_wav_audio() {
        assert!(validate_wav(b"not audio").is_err());
    }

    #[test]
    fn deadline_tracks_the_actual_audio_not_ten_minutes() {
        let duration = wav_duration(include_bytes!("../assets/audio-test.wav")).unwrap();
        assert!(duration.as_secs_f64() > 3.9 && duration.as_secs_f64() < 4.1);
    }

    #[test]
    fn audio_urls_require_an_allowlisted_host() {
        let policy = UrlPolicy {
            allow: vec!["media.example.org".into(), "*.example.net".into()],
            allow_insecure_http: false,
        };
        assert!(url_allowed(&policy, "https://media.example.org/song.wav").is_ok());
        assert!(url_allowed(&policy, "https://a.example.net/song.wav").is_ok());
        assert!(url_allowed(&policy, "https://other.example.org/song.wav").is_err());
        assert!(url_allowed(&policy, "http://media.example.org/song.wav").is_err());
        assert!(url_allowed(&policy, "https://user:pass@media.example.org/song.wav").is_err());
    }

    #[test]
    fn audio_urls_allow_private_hosts_when_listed() {
        let policy = UrlPolicy {
            allow: vec!["192.168.0.83".into()],
            allow_insecure_http: true,
        };
        assert!(url_allowed(&policy, "http://192.168.0.83/audio.wav").is_ok());
        assert!(url_allowed(&UrlPolicy::default(), "http://192.168.0.83/audio.wav").is_err());
    }

    #[test]
    fn stream_header_accepts_known_and_live_data_chunks() {
        let known = wav(1600);
        let format = scan_stream_header(&known).unwrap().unwrap();
        assert_eq!(format.duration.unwrap().as_millis(), 100);

        let mut live = wav(0);
        live.truncate(live.len() - 4);
        live.extend(u32::MAX.to_le_bytes());
        let format = scan_stream_header(&live).unwrap().unwrap();
        assert!(format.duration.is_none());
    }

    #[test]
    fn stream_header_waits_for_incomplete_input() {
        let bytes = wav(1600);
        assert!(scan_stream_header(&bytes[..10]).unwrap().is_none());
        assert!(scan_stream_header(&bytes[..40]).unwrap().is_none());
    }

    #[test]
    fn stream_header_rejects_other_formats_and_long_audio() {
        assert!(scan_stream_header(b"OggSnot a wave").is_err());
        let too_long = 600 * 16000 + 1;
        let long = wav(too_long);
        assert!(scan_stream_header(&long).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_allows_the_player_to_release_resources() {
        let root = std::env::temp_dir().join(format!("luna-player-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let marker = root.join("closed");
        let ready = root.join("ready");
        let mut child=std::process::Command::new("/bin/sh").args(["-c","trap 'echo closed > \"$1\"; exit 0' TERM; echo ready > \"$2\"; while :; do :; done","player"])
            .arg(&marker).arg(&ready).spawn().unwrap();
        for _ in 0..100 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        super::terminate_alsa(&mut child);
        assert_eq!(std::fs::read_to_string(marker).unwrap().trim(), "closed");
        std::fs::remove_dir_all(root).unwrap();
    }
}
