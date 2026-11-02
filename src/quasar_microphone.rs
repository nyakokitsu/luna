use crate::gateway::{MicrophoneBackend, MicrophoneDevice};
use anyhow::{bail, Context, Result};
use std::{
    io::{Read, Write},
    net::TcpStream,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const CONFIG: &str = "/system/vendor/quasar/quasar.cfg";
const SERVICE: &str = "iohub_clients";
const CHANNELS_FIELD: u32 = 158;
const REQUEST_FIELD: u32 = 159;
const RAW_MIC: u64 = 1;
const RATE: u32 = 16_000;

pub fn create_microphone_backend() -> Option<Box<dyn MicrophoneBackend>> {
    Some(Box::new(IoHubMicrophone))
}

struct IoHubMicrophone;

impl IoHubMicrophone {
    fn available(&self) -> bool {
        Path::new("/system/vendor/quasar/maind").is_file() && Path::new(CONFIG).is_file()
    }

    fn endpoint(&self) -> Result<String> {
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(CONFIG)?)?;
        let port = value
            .get(SERVICE)
            .and_then(|v| v.get("port"))
            .and_then(serde_json::Value::as_u64)
            .context("iohub_clients.port is not configured")?;
        Ok(format!("127.0.0.1:{port}"))
    }

    fn capture(
        &self,
        seconds: u8,
        cancelled: &AtomicBool,
        callback: &mut dyn FnMut(&[Channel]) -> Result<bool>,
    ) -> Result<()> {
        let request_id = format!(
            "luna-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let mut stream = TcpStream::connect(self.endpoint()?).context("connect Quasar IOHub")?;
        stream.set_read_timeout(Some(Duration::from_secs(u64::from(seconds) + 3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        frame_write(&mut stream, &request(&request_id, 4))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(u64::from(seconds));
        let result = (|| loop {
            if cancelled.load(Ordering::Relaxed) {
                bail!("plugin stopped")
            }
            if std::time::Instant::now() >= deadline {
                break Ok(());
            }
            let Some(data) = frame_read(&mut stream)? else {
                break Ok(());
            };
            let channels = parse_channels(&data)?;
            if !channels.is_empty() && callback(&channels)? {
                break Ok(());
            }
        })();
        let _ = frame_write(&mut stream, &request(&request_id, 6));
        result
    }
}

impl MicrophoneBackend for IoHubMicrophone {
    fn is_available(&self) -> Result<bool> {
        Ok(self.available())
    }
    fn devices(&self) -> Result<Vec<MicrophoneDevice>> {
        if !self.available() {
            bail!("Quasar microphone capture is unavailable")
        }
        Ok((0..4)
            .map(|i| MicrophoneDevice {
                id: format!("raw_mic_{i}"),
                name: format!("Raw microphone {}", i + 1),
            })
            .collect())
    }
    fn record(&self, device: Option<&str>, seconds: u8, cancelled: &AtomicBool) -> Result<Vec<u8>> {
        if !self.available() {
            bail!("Quasar microphone capture is unavailable")
        }
        if !(1..=30).contains(&seconds) {
            bail!("microphone capture duration must be 1..=30 seconds")
        }
        let selected = device.map(parse_id).transpose()?.unwrap_or(0);
        let mut pcm = Vec::new();
        let mut found = false;
        self.capture(seconds, cancelled, &mut |channels| {
            let Some(channel) = channels.get(selected) else {
                if !found {
                    bail!("unknown microphone device")
                }
                return Ok(false);
            };
            found = true;
            pcm.extend_from_slice(&resample(&channel.data, channel.rate)?);
            Ok(false)
        })?;
        if !found {
            bail!("Quasar IOHub did not report the selected microphone channel")
        }
        if pcm.is_empty() {
            bail!("microphone capture returned no audio")
        }
        Ok(wav(&pcm))
    }
}

struct Channel {
    data: Vec<u8>,
    rate: u32,
}

fn parse_id(value: &str) -> Result<usize> {
    let index = value
        .strip_prefix("raw_mic_")
        .context("invalid microphone device ID")?
        .parse::<usize>()?;
    if index >= 4 {
        bail!("unknown microphone device")
    }
    Ok(index)
}

fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
}
fn bytes(field: u32, value: &[u8], out: &mut Vec<u8>) {
    varint((u64::from(field) << 3) | 2, out);
    varint(value.len() as u64, out);
    out.extend_from_slice(value);
}
fn request(id: &str, command: u32) -> Vec<u8> {
    let mut c = Vec::new();
    bytes(command, &[], &mut c);
    let mut m = Vec::new();
    bytes(1, id.as_bytes(), &mut m);
    bytes(REQUEST_FIELD, &c, &mut m);
    m
}

fn frame_write(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    stream.write_all(&(i32::try_from(data.len())?).to_le_bytes())?;
    stream.write_all(data)?;
    stream.flush()?;
    Ok(())
}
fn frame_read(stream: &mut TcpStream) -> Result<Option<Vec<u8>>> {
    let mut h = [0; 4];
    match stream.read_exact(&mut h) {
        Ok(()) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e.into()),
    }
    let n = i32::from_le_bytes(h);
    if !(0..=10 * 1024 * 1024).contains(&n) {
        bail!("invalid Quasar IPC frame length")
    }
    let mut data = vec![0; n as usize];
    stream.read_exact(&mut data)?;
    Ok(Some(data))
}

fn read_varint(data: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let b = *data.get(*pos).context("truncated protobuf varint")?;
        *pos += 1;
        value |= u64::from(b & 127) << shift;
        if b & 128 == 0 {
            return Ok(value);
        }
    }
    bail!("invalid protobuf varint")
}
fn key(data: &[u8], pos: &mut usize) -> Result<(u32, u8)> {
    let k = read_varint(data, pos)?;
    Ok(((k >> 3) as u32, (k & 7) as u8))
}
fn read_bytes<'a>(data: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    let n = read_varint(data, pos)? as usize;
    let end = pos.checked_add(n).context("protobuf length overflow")?;
    let value = data.get(*pos..end).context("truncated protobuf bytes")?;
    *pos = end;
    Ok(value)
}
fn skip(data: &[u8], pos: &mut usize, wire: u8) -> Result<()> {
    match wire {
        0 => {
            read_varint(data, pos)?;
        }
        1 => *pos += 8,
        2 => {
            read_bytes(data, pos)?;
        }
        5 => *pos += 4,
        _ => bail!("unsupported protobuf wire type"),
    };
    if *pos > data.len() {
        bail!("truncated protobuf field")
    }
    Ok(())
}

fn parse_channels(data: &[u8]) -> Result<Vec<Channel>> {
    let mut pos = 0;
    let mut result = Vec::new();
    while pos < data.len() {
        let (field, wire) = key(data, &mut pos)?;
        if field == CHANNELS_FIELD && wire == 2 {
            result.extend(parse_channel_list(read_bytes(data, &mut pos)?)?);
        } else {
            skip(data, &mut pos, wire)?;
        }
    }
    Ok(result)
}
fn parse_channel_list(data: &[u8]) -> Result<Vec<Channel>> {
    let mut pos = 0;
    let mut result = Vec::new();
    while pos < data.len() {
        let (field, wire) = key(data, &mut pos)?;
        if field == 1 && wire == 2 {
            if let Some(c) = parse_channel(read_bytes(data, &mut pos)?)? {
                result.push(c);
            }
        } else {
            skip(data, &mut pos, wire)?;
        }
    }
    Ok(result)
}
fn parse_channel(data: &[u8]) -> Result<Option<Channel>> {
    let mut pos = 0;
    let mut kind = None;
    let mut payload = Vec::new();
    let mut rate = None;
    while pos < data.len() {
        let (field, wire) = key(data, &mut pos)?;
        match (field, wire) {
            (1, 0) => kind = Some(read_varint(data, &mut pos)?),
            (3, 2) => payload = read_bytes(data, &mut pos)?.to_vec(),
            (7, 0) => rate = Some(read_varint(data, &mut pos)? as u32),
            (_, w) => skip(data, &mut pos, w)?,
        }
    }
    if kind != Some(RAW_MIC) {
        return Ok(None);
    }
    Ok(Some(Channel {
        data: payload,
        rate: rate.context("microphone channel has no sample rate")?,
    }))
}

fn resample(data: &[u8], rate: u32) -> Result<Vec<u8>> {
    if data.len() % 2 != 0 {
        bail!("microphone PCM has an incomplete sample")
    }
    if rate == RATE {
        return Ok(data.to_vec());
    }
    if rate < RATE || rate % RATE != 0 {
        bail!("unsupported microphone sample rate {rate}")
    }
    let factor = (rate / RATE) as usize;
    let mut out = Vec::with_capacity(data.len() / factor);
    for chunk in data.chunks_exact(factor * 2) {
        let mut sum = 0i32;
        for s in chunk.chunks_exact(2) {
            sum += i32::from(i16::from_le_bytes([s[0], s[1]]));
        }
        out.extend_from_slice(&((sum / factor as i32) as i16).to_le_bytes());
    }
    Ok(out)
}
fn wav(pcm: &[u8]) -> Vec<u8> {
    let n = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + n).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}
