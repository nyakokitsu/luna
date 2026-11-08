//! Quasar's own (logical) station volume.
//!
//! `maind` keeps the level that the hardware buttons, the ring and Alice use and
//! mirrors it to ALSA. Writing ALSA alone — or the VQE socket, which this
//! firmware never starts — leaves that state untouched, so the next button
//! press jumps back to it. The native protocol below reads the state stream on
//! `stereo_pair` and sends one external VolumeSet directive on `iohub_clients`;
//! the station acknowledges through the same state stream.
//!
//! No credentials, loopback only, one fixed directive type.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

const CONFIG_PATH: &str = "/system/vendor/quasar/quasar.cfg";
const VERSION_PATH: &str = "/system/vendor/quasar/version_full";
const COMPATIBLE_PATH: &str = "/proc/device-tree/compatible";

const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const STATE_TIMEOUT: Duration = Duration::from_millis(600);
const ACK_TIMEOUT: Duration = Duration::from_millis(1000);
const MAX_FRAME: usize = 4 << 20;

/// The VolumeSet directive is only sent to firmware whose write path was
/// verified on this model; everything else falls back to ALSA.
static VERIFIED: OnceLock<bool> = OnceLock::new();
static PORTS: OnceLock<(u16, u16)> = OnceLock::new();

fn verified() -> bool {
    *VERIFIED.get_or_init(|| {
        let build = fs::read_to_string(VERSION_PATH).unwrap_or_default();
        let build = build.trim();
        let compatible = fs::read(COMPATIBLE_PATH).unwrap_or_default();
        verified_build(build, &compatible)
    })
}

// Keep firmware qualification independent of filesystem state so unqualified
// builds and look-alike DT names can be rejected before opening IPC sockets.
fn verified_build(build: &str, compatible: &[u8]) -> bool {
    // Mini 3: verified against logical stereo_pair state and VolumeSet ACKs
    // on the stock 2026-10-21 firmware, including a 5 -> 0 -> 5% round trip.
    if build == "0.369.1.50.4794652521.20261021.117" {
        return compatible
            .split(|byte| *byte == 0)
            .any(|entry| entry == b"yandex,bergamot");
    }
    if build == "0.369.1.46.4793643989.20261020.299" {
        return compatible
            .split(|byte| *byte == 0)
            .any(|entry| entry == b"yandex,station-mini2");
    }
    let compatible = String::from_utf8_lossy(compatible).replace('\0', " ");
    let has = |token: &str| compatible.contains(token);
    match build {
        "0.369.1.50.4794649087.20261021.295" => has("yandex,station-midi"),
        "0.369.1.50.4794659041.20261021.299" => has("yandex,station-mini2"),
        "0.369.1.50.4795647582.20261021.133" => has("yandex,plum-evt2"),
        "0.369.1.50.4794654278.20261021.133" => has("yandex,prune-evt2"),
        "0.369.1.46.4778262955.20261013.160" => has("yandex,cucumber"),
        "0.369.1.46.4778262442.20261013.297" => has("yandex,yandexmicro"),
        "0.369.1.50.4794663100.20261021.160" => {
            has("yandex,cucumber-dvt2") || has("yandex,cucumber")
        }
        "1.266.4.40.2633723173.20241021" => true,
        _ => false,
    }
}

fn read_ports() -> Result<(u16, u16)> {
    let text = fs::read_to_string(CONFIG_PATH).context("read Quasar config")?;
    let config: serde_json::Value =
        serde_json::from_str(&text).context("invalid Quasar config JSON")?;
    let port = |service: &str| -> Result<u16> {
        config
            .get(service)
            .and_then(|entry| entry.get("port"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .with_context(|| format!("{service}.port is missing from the Quasar config"))
    };
    Ok((port("stereo_pair")?, port("iohub_clients")?))
}

fn ports() -> Result<(u16, u16)> {
    if let Some(ports) = PORTS.get() {
        return Ok(*ports);
    }
    let ports = read_ports()?;
    let _ = PORTS.set(ports);
    Ok(ports)
}

fn connect(port: u16) -> Result<TcpStream> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        .with_context(|| format!("connect to Quasar IPC on 127.0.0.1:{port}"))
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        bail!("Quasar volume IPC timed out")
    }
    Ok(left)
}

fn read_frame(stream: &mut TcpStream, deadline: Instant) -> Result<Vec<u8>> {
    let mut header = [0u8; 4];
    stream.set_read_timeout(Some(remaining(deadline)?))?;
    stream
        .read_exact(&mut header)
        .context("read Quasar frame header")?;
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        bail!("invalid Quasar frame length")
    }
    let mut body = vec![0u8; length];
    stream.set_read_timeout(Some(remaining(deadline)?))?;
    stream.read_exact(&mut body).context("read Quasar frame")?;
    Ok(body)
}

fn read_varint(data: &[u8], offset: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data.get(*offset).context("truncated protobuf varint")?;
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    bail!("protobuf varint overflow")
}

struct Field<'a> {
    number: u64,
    wire: u64,
    bytes: &'a [u8],
    integer: u64,
}

fn next_field<'a>(data: &'a [u8], offset: &mut usize) -> Result<Field<'a>> {
    let key = read_varint(data, offset)?;
    let (number, wire) = (key >> 3, key & 7);
    if number == 0 {
        bail!("invalid protobuf field number")
    }
    let mut field = Field {
        number,
        wire,
        bytes: &[],
        integer: 0,
    };
    match wire {
        0 => field.integer = read_varint(data, offset)?,
        1 | 5 => {
            let size = if wire == 1 { 8 } else { 4 };
            let end = (*offset)
                .checked_add(size)
                .context("protobuf field overflow")?;
            field.bytes = data.get(*offset..end).context("truncated protobuf field")?;
            *offset = end;
        }
        2 => {
            let length = read_varint(data, offset)?;
            let length = usize::try_from(length).context("protobuf length overflow")?;
            let end = (*offset)
                .checked_add(length)
                .context("protobuf field overflow")?;
            field.bytes = data.get(*offset..end).context("truncated protobuf field")?;
            *offset = end;
        }
        _ => bail!("unsupported protobuf wire type"),
    }
    Ok(field)
}

fn bytes_field<'a>(data: &'a [u8], wanted: u64) -> Result<Option<&'a [u8]>> {
    let mut offset = 0usize;
    while offset < data.len() {
        let field = next_field(data, &mut offset)?;
        if field.number == wanted && field.wire == 2 {
            return Ok(Some(field.bytes));
        }
    }
    Ok(None)
}

/// Extracts the logical percentage from one state frame. Frames that carry no
/// volume state (they are common on the shared stream) yield `None`.
fn state_percent(frame: &[u8]) -> Result<Option<f32>> {
    let mut body: Option<&[u8]> = None;
    let mut kind = 0u64;
    let mut offset = 0usize;
    while offset < frame.len() {
        let field = next_field(frame, &mut offset)?;
        match (field.number, field.wire) {
            (198, 2) => {
                body = Some(field.bytes);
                kind = 198;
            }
            (252, 2) => body = Some(field.bytes),
            (253, 0) => kind = field.integer,
            _ => {}
        }
    }
    if kind != 198 {
        return Ok(None);
    }
    let Some(body) = body else { return Ok(None) };
    let Some(inner) = bytes_field(body, 1)? else {
        return Ok(None);
    };
    let Some(state) = bytes_field(inner, 11)? else {
        return Ok(None);
    };
    let mut muted = false;
    let mut percent: Option<f32> = None;
    let mut offset = 0usize;
    while offset < state.len() {
        let field = next_field(state, &mut offset)?;
        if field.number == 3 && field.wire == 0 {
            muted = field.integer != 0;
        } else if field.number == 6 && field.wire == 5 {
            let raw: [u8; 4] = field
                .bytes
                .try_into()
                .context("invalid native volume float")?;
            percent = Some(f32::from_le_bytes(raw));
        }
    }
    let Some(percent) = percent else {
        return Ok(None);
    };
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        bail!("invalid native volume percentage")
    }
    Ok(Some(if muted { 0.0 } else { percent }))
}

fn read_state(stream: &mut TcpStream, deadline: Instant) -> Result<f32> {
    loop {
        let frame = read_frame(stream, deadline)?;
        if let Some(percent) = state_percent(&frame)? {
            return Ok(percent);
        }
    }
}

fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// IOControl88.external_directives95.directive_list1 with one TDirective:
/// name `volume_set`, IgnoreAnswer, the typed PercentLevel payload and
/// DirectiveType 254 (VolumeSet). Source=Button makes the station show the
/// level on the ring without a sound; IsLedSilent is left unset.
fn set_frame(percent: u8) -> Vec<u8> {
    let mut typed = Vec::with_capacity(7);
    typed.push(13);
    typed.extend_from_slice(&(f32::from(percent)).to_le_bytes());
    typed.extend_from_slice(&[24, 1]);

    let mut directive = Vec::new();
    directive.extend_from_slice(&[10, 9]);
    directive.extend_from_slice(b"directive");
    directive.extend_from_slice(&[18, 10]);
    directive.extend_from_slice(b"volume_set");
    directive.extend_from_slice(&[32, 1, 122, typed.len() as u8]);
    directive.extend_from_slice(&typed);
    push_varint(&mut directive, 20 << 3);
    push_varint(&mut directive, 254);

    let mut packet = vec![0u8; 4];
    push_varint(&mut packet, 252 << 3 | 2);
    push_varint(&mut packet, (7 + directive.len()) as u64);
    push_varint(&mut packet, 95 << 3 | 2);
    push_varint(&mut packet, (4 + directive.len()) as u64);
    packet.extend_from_slice(&[10, (2 + directive.len()) as u8, 10, directive.len() as u8]);
    packet.extend_from_slice(&directive);
    push_varint(&mut packet, 253 << 3);
    push_varint(&mut packet, 88);
    let length = (packet.len() - 4) as u32;
    packet[..4].copy_from_slice(&length.to_le_bytes());
    packet
}

fn quantise(percent: f32) -> u8 {
    ((percent / 5.0).round().clamp(0.0, 20.0)) as u8 * 5
}

/// Reads the station's logical level, rounding to the 5% grid the hardware uses.
pub fn get() -> Result<u8> {
    if !verified() {
        bail!("station firmware is not verified for native volume")
    }
    let (state_port, _) = ports()?;
    let mut state = connect(state_port)?;
    let value = read_state(&mut state, Instant::now() + STATE_TIMEOUT)?;
    Ok(quantise(value))
}

/// Sets the station's logical level and waits for the station to acknowledge it.
pub fn set(percent: u8) -> Result<u8> {
    if !verified() {
        bail!("station firmware is not verified for native volume")
    }
    let (state_port, control_port) = ports()?;
    let wanted = quantise(f32::from(percent));
    let mut state = connect(state_port)?;
    let current = read_state(&mut state, Instant::now() + STATE_TIMEOUT)?;
    // Compare on the station's own grid: an in-between level (the firmware
    // keeps floats) already stands for this step and must not be rewritten.
    if quantise(current) == wanted {
        return Ok(wanted);
    }
    let mut control = connect(control_port)?;
    control.set_write_timeout(Some(remaining(Instant::now() + STATE_TIMEOUT)?))?;
    control
        .write_all(&set_frame(wanted))
        .context("write Quasar volume directive")?;
    let _ = control.flush();
    let deadline = Instant::now() + ACK_TIMEOUT;
    loop {
        let value = read_state(&mut state, deadline)?;
        if quantise(value) == wanted {
            return Ok(wanted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mini3_requires_exact_firmware_and_dt_entry() {
        let build = "0.369.1.50.4794652521.20261021.117";
        assert!(verified_build(
            build,
            b"yandex,bergamot-evt1\0yandex,bergamot\0amlogic, axg\0"
        ));
        assert!(verified_build(build, b"yandex,bergamot\0"));
        for board in [
            &b""[..],
            &b"yandex,station-mini2\0"[..],
            &b"yandex,bergamot-evt1\0"[..],
            &b"prefix-yandex,bergamot\0"[..],
            &b"yandex,bergamot-suffix\0"[..],
        ] {
            assert!(!verified_build(build, board));
        }
        for build in [
            "0.369.1.50.4794652521.20261021.116",
            "0.369.1.50.4794652521.20261021.117-extra",
            "",
        ] {
            assert!(!verified_build(build, b"yandex,bergamot\0"));
        }
    }

    #[test]
    fn mini2_046_requires_exact_firmware_and_dt_entry() {
        let build = "0.369.1.46.4793643989.20261020.299";
        assert!(verified_build(
            build,
            b"yandex,station-mini2\0amlogic,axg\0"
        ));
        assert!(verified_build(
            build,
            b"amlogic,axg\0yandex,station-mini2\0"
        ));
        for compatible in [
            &b""[..],
            &b"yandex,station-mini\0"[..],
            &b"yandex,station-midi\0"[..],
            &b"prefix-yandex,station-mini2\0"[..],
            &b"yandex,station-mini2-suffix\0"[..],
        ] {
            assert!(!verified_build(build, compatible));
        }
        for unqualified in [
            "0.369.1.46.4793643989.20261020.298",
            "0.369.1.46.4793643989.20261020.299-extra",
            "0.369.1.46",
            "",
        ] {
            assert!(!verified_build(unqualified, b"yandex,station-mini2\0"));
        }
    }

    #[test]
    fn existing_native_firmware_qualification_is_preserved() {
        for (build, board) in [
            ("0.369.1.50.4794649087.20261021.295", "yandex,station-midi"),
            ("0.369.1.50.4794659041.20261021.299", "yandex,station-mini2"),
            ("0.369.1.50.4795647582.20261021.133", "yandex,plum-evt2"),
            ("0.369.1.50.4794654278.20261021.133", "yandex,prune-evt2"),
            ("0.369.1.46.4778262955.20261013.160", "yandex,cucumber"),
            ("0.369.1.46.4778262442.20261013.297", "yandex,yandexmicro"),
            ("0.369.1.50.4794663100.20261021.160", "yandex,cucumber-dvt2"),
            ("0.369.1.50.4794663100.20261021.160", "yandex,cucumber"),
        ] {
            assert!(verified_build(build, board.as_bytes()));
            assert!(!verified_build(build, b"unqualified-board\0"));
        }
        assert!(verified_build("1.266.4.40.2633723173.20241021", b""));
    }

    /// Independently transcribed schema fixture from the reference helper:
    /// field252 IOControl88, VolumeSet254, PercentLevel 50.0f, Source=Button.
    #[test]
    fn set_frame_matches_the_verified_schema() {
        let golden = "33000000e20f2dfa052a0a280a260a0964697265637469766512\
                      0a766f6c756d655f73657420017a070d000048421801a001fe01e80f58";
        assert_eq!(set_frame(50), hex(golden));
    }

    #[test]
    fn state_frame_decodes_percent_and_mute() {
        for muted in [false, true] {
            let mut value = vec![53];
            value.extend_from_slice(&70.0f32.to_le_bytes());
            if muted {
                value.extend_from_slice(&[24, 1]);
            }
            let frame = field(198, &field(1, &field(11, &value)));
            assert_eq!(
                state_percent(&frame).unwrap(),
                Some(if muted { 0.0 } else { 70.0 })
            );
        }
    }

    #[test]
    fn quantise_uses_the_five_percent_grid() {
        assert_eq!(quantise(0.0), 0);
        assert_eq!(quantise(56.5), 55);
        assert_eq!(quantise(57.5), 60);
        assert_eq!(quantise(100.0), 100);
        assert_eq!(quantise(120.0), 100);
    }

    #[test]
    fn rejects_malformed_frames() {
        for bad in [vec![0u8], vec![255], vec![10, 99, 0]] {
            assert!(state_percent(&bad).is_err());
        }
        assert_eq!(state_percent(&[]).unwrap(), None);
    }

    fn field(number: u64, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        push_varint(&mut out, number << 3 | 2);
        push_varint(&mut out, body.len() as u64);
        out.extend_from_slice(body);
        out
    }

    fn hex(text: &str) -> Vec<u8> {
        let compact: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        (0..compact.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&compact[i..i + 2], 16).unwrap())
            .collect()
    }
}
