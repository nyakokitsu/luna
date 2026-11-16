use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::fd::{FromRawFd, RawFd},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    sync::{Mutex, OnceLock},
    thread,
    time::Duration,
};

const RFKILL_ROOT: &str = "/sys/class/rfkill";
const POWER_SCRIPT: &str = "/system/vendor/quasar/bsa_server_run.sh";
const BT_DEMO: &str = "/system/workdir/bin/bt_demo";
const BSA_CONFIG: &str = "/vendor/bsa/bt_config.xml";
const ALSA_CONFIG: &str = "/tmp/luna-bluetooth-asound.conf";

const BLUETOOTH_ASOUND_CONF: &str = r#"pcm.!default {
    type plug
    slave.pcm "hw:0,0"
}

ctl.!default {
    type hw
    card 0
}
"#;

static SCAN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static SCAN_RESULTS: OnceLock<Mutex<Vec<Value>>> = OnceLock::new();
static DEMO_SESSION: OnceLock<Mutex<Option<DemoSession>>> = OnceLock::new();
static PAIRABLE_STATE: OnceLock<Mutex<Option<bool>>> = OnceLock::new();
static QUASAR_CLI: OnceLock<Mutex<Option<QuasarCli>>> = OnceLock::new();

struct DemoSession {
    child: Child,
    stdin: fs::File,
    events: Receiver<String>,
}

struct QuasarCli {
    child: Child,
    stdin: ChildStdin,
}

const QUASAR_MAIND: &str = "/system/vendor/quasar/maind";

fn quasar_cli() -> Result<std::sync::MutexGuard<'static, Option<QuasarCli>>> {
    QUASAR_CLI
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow!("Quasar CLI session is unavailable"))
}

fn native_quasar_command(command: &str) -> Result<()> {
    if !Path::new(QUASAR_MAIND).is_file() {
        bail!("station Quasar maind is unavailable")
    }

    let mut cli = quasar_cli()?;
    let dead = cli
        .as_mut()
        .is_some_and(|value| value.child.try_wait().ok().flatten().is_some());
    if dead {
        *cli = None;
    }
    if cli.is_none() {
        let mut child = Command::new(QUASAR_MAIND)
            .arg("--cli")
            .env(
                "LD_LIBRARY_PATH",
                "/system/vendor/quasar/libs:/system/workdir/lib",
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start native Quasar CLI")?;
        let stdin = child.stdin.take().context("open native Quasar CLI stdin")?;
        *cli = Some(QuasarCli { child, stdin });
        thread::sleep(Duration::from_millis(400));
    }
    let session = cli.as_mut().expect("Quasar CLI initialized");
    session.stdin.write_all(b"q\n")?;
    session.stdin.write_all(command.as_bytes())?;
    session.stdin.write_all(b"\n")?;
    session.stdin.flush()?;
    // The command is asynchronous: maind receives it and routes the resulting
    // start_bluetooth/stop_bluetooth directive through its own capability.
    thread::sleep(Duration::from_millis(1200));
    Ok(())
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|path| path.join(name).is_file()))
}

fn prepare_bluetooth_audio() -> Result<()> {
    if fs::read_to_string(ALSA_CONFIG).ok().as_deref() != Some(BLUETOOTH_ASOUND_CONF) {
        fs::write(ALSA_CONFIG, BLUETOOTH_ASOUND_CONF)
            .context("write Bluetooth ALSA compatibility configuration")?;
    }
    Ok(())
}

fn command_output(args: &[&str]) -> Result<String> {
    if !command_exists("bluetoothctl") {
        bail!("bluetoothctl is unavailable")
    }
    let output = Command::new("bluetoothctl")
        .args(args)
        .output()
        .context("run bluetoothctl")?;
    if !output.status.success() {
        bail!(
            "bluetoothctl {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn process_running(name: &str) -> Result<bool> {
    for entry in fs::read_dir("/proc").context("read /proc for Bluetooth process status")? {
        let entry = entry?;
        if !entry
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii_digit())
        {
            continue;
        }
        let cmdline = match fs::read(entry.path().join("cmdline")) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("read Bluetooth process command line"),
        };
        if cmdline.split(|byte| *byte == 0).any(|part| {
            Path::new(std::str::from_utf8(part).unwrap_or(""))
                .file_name()
                .and_then(|name| name.to_str())
                == Some(name)
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn rfkill_powered() -> Result<Option<bool>> {
    let root = Path::new(RFKILL_ROOT);
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read Bluetooth rfkill directory"),
    };
    let mut states = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if fs::read_to_string(path.join("type"))
            .ok()
            .as_deref()
            .map(str::trim)
            != Some("bluetooth")
        {
            continue;
        }
        let state = fs::read_to_string(path.join("state"))?;
        states.push(match state.trim() {
            "0" => false,
            "1" => true,
            value => bail!("invalid Bluetooth rfkill state {value}"),
        });
    }
    Ok(states.into_iter().reduce(|left, right| left || right))
}

fn parse_property(output: &str, property: &str) -> Option<bool> {
    output.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim().eq_ignore_ascii_case(property))
            .then(|| value.trim().eq_ignore_ascii_case("yes"))
    })
}

fn devices() -> Result<Vec<Value>> {
    let output = command_output(&["devices"])?;
    Ok(output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ' ');
            (parts.next()? == "Device").then(|| {
                json!({
                    "address": parts.next().unwrap_or_default(),
                    "name": parts.next().unwrap_or_default().trim(),
                })
            })
        })
        .collect())
}

fn cached_devices() -> Vec<Value> {
    SCAN_RESULTS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .map(|devices| devices.clone())
        .unwrap_or_default()
}

fn cached_pairable() -> Option<bool> {
    PAIRABLE_STATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|state| *state)
}

fn config_flag(config: &str, tag: &str) -> Option<bool> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = config.find(&open)? + open.len();
    let end = start + config[start..].find(&close)?;
    match config[start..end].trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

fn configured_pairable() -> Option<bool> {
    let config = fs::read_to_string(BSA_CONFIG).ok()?;
    match (
        config_flag(&config, "visible"),
        config_flag(&config, "connectable"),
    ) {
        (Some(visible), Some(connectable)) => Some(visible && connectable),
        _ => None,
    }
}

fn set_bsa_visibility_config(on: bool) -> Result<()> {
    let mut config =
        fs::read_to_string(BSA_CONFIG).context("read station Bluetooth configuration")?;
    for tag in ["visible", "connectable"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let start = config
            .find(&open)
            .with_context(|| format!("Bluetooth configuration has no <{tag}> field"))?;
        let value_start = start + open.len();
        let end = config[value_start..]
            .find(&close)
            .map(|offset| value_start + offset)
            .with_context(|| format!("Bluetooth configuration has no </{tag}> field"))?;
        config.replace_range(value_start..end, if on { "1" } else { "0" });
    }
    fs::write(BSA_CONFIG, config).context("write station Bluetooth visibility configuration")?;
    Ok(())
}

fn parse_demo_devices(output: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(output);
    let mut latest = Vec::new();
    for line in text.lines() {
        let Some(start) = line.find('{') else {
            continue;
        };
        let Some(end) = line.rfind('}') else { continue };
        let Ok(message) = serde_json::from_str::<Value>(&line[start..=end]) else {
            continue;
        };
        let Some(list) = message.get("list").and_then(Value::as_array) else {
            continue;
        };
        latest = list
            .iter()
            .filter_map(|device| {
                let address = device.get("ad")?.as_str()?.to_ascii_uppercase();
                let name = device
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Some(json!({
                    "address": address,
                    "name": name,
                    "role": device.get("role").cloned().unwrap_or(Value::Null),
                }))
            })
            .collect();
    }
    latest
}

fn demo_scan() -> Result<Vec<Value>> {
    let _guard = SCAN_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow!("Bluetooth scan is already running"))?;

    let mut path = std::env::var("PATH").unwrap_or_default();
    if !path.is_empty() {
        path.push(':');
    }
    path.push_str("/system/workdir/bin");

    let mut child = Command::new(BT_DEMO)
        .env("PATH", path)
        .env(
            "LD_LIBRARY_PATH",
            "/system/vendor/quasar/libs:/system/workdir/lib",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start station Bluetooth discovery helper")?;
    let mut stdin = child.stdin.take().context("open Bluetooth helper stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("open Bluetooth helper stdout")?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let _ = std::io::BufReader::new(stdout).read_to_end(&mut output);
        output
    });

    // bt_demo is an interactive wrapper around the station's already-running
    // bsa_server. Let it finish initialization before selecting Scan.
    // bsa_server_run.sh may have just restarted the controller; bt_demo's
    // management handshake can take a few seconds before its menu is ready.
    thread::sleep(Duration::from_secs(3));
    stdin.write_all(b"4\n")?;
    stdin.flush()?;
    thread::sleep(Duration::from_secs(7));
    let _ = stdin.write_all(b"5\n99\n");
    let _ = stdin.flush();
    drop(stdin);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while child.try_wait()?.is_none() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    if child.try_wait()?.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    let output = reader
        .join()
        .map_err(|_| anyhow!("Bluetooth helper stopped unexpectedly"))?;
    let devices = parse_demo_devices(&output);
    if let Ok(mut cached) = SCAN_RESULTS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        *cached = devices.clone();
    }
    Ok(devices)
}

fn demo_session() -> Result<std::sync::MutexGuard<'static, Option<DemoSession>>> {
    DEMO_SESSION
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow!("Bluetooth helper session is unavailable"))
}

fn start_demo_session() -> Result<DemoSession> {
    prepare_bluetooth_audio()?;
    let mut path = std::env::var("PATH").unwrap_or_default();
    if !path.is_empty() {
        path.push(':');
    }
    path.push_str("/system/workdir/bin");
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result != 0 {
        bail!(
            "create Bluetooth helper pseudo-terminal: {}",
            std::io::Error::last_os_error()
        )
    }
    let duplicate_slave = || -> Result<fs::File> {
        let fd = unsafe { libc::dup(slave) };
        if fd < 0 {
            bail!(
                "duplicate Bluetooth helper pseudo-terminal: {}",
                std::io::Error::last_os_error()
            )
        }
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    };
    let master_file = unsafe { fs::File::from_raw_fd(master) };
    let reader = master_file.try_clone()?;
    let child = Command::new(BT_DEMO)
        .env("PATH", path)
        .env(
            "LD_LIBRARY_PATH",
            "/system/vendor/quasar/libs:/system/workdir/lib",
        )
        .env("ALSA_CONFIG_PATH", ALSA_CONFIG)
        .stdin(Stdio::from(duplicate_slave()?))
        .stdout(Stdio::from(duplicate_slave()?))
        .stderr(Stdio::from(duplicate_slave()?))
        .spawn()
        .context("start station Bluetooth control helper")?;
    unsafe { libc::close(slave) };
    let (sender, events) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            let _ = sender.send(line);
        }
    });
    Ok(DemoSession {
        child,
        stdin: master_file,
        events,
    })
}

fn wait_demo_text(session: &mut DemoSession, needle: &str, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        while let Ok(line) = session.events.try_recv() {
            if line.contains(needle) {
                return Ok(());
            }
        }
        if let Ok(Some(status)) = session.child.try_wait() {
            bail!("Bluetooth helper exited with {status}")
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!("timed out waiting for Bluetooth helper prompt: {needle}")
}

fn wait_demo_result(session: &mut DemoSession, command: &str, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        while let Ok(line) = session.events.try_recv() {
            let line = line.trim();
            if let Some(result) = line.strip_prefix(&format!("{command} ret ")) {
                if result.trim() == "0" {
                    // BSA may acknowledge dispatch first and report the
                    // actual A2DP failure a moment later.
                    let settle_deadline = std::time::Instant::now() + Duration::from_secs(3);
                    while std::time::Instant::now() < settle_deadline {
                        while let Ok(next) = session.events.try_recv() {
                            let next = next.trim();
                            if next.contains("failure opening AV connection") {
                                bail!("station Bluetooth connection failed")
                            }
                            if let Some(code) = next.strip_prefix(&format!("{command} ret ")) {
                                if code.trim() != "0" {
                                    bail!("station Bluetooth command failed: {next}")
                                }
                            }
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                    return Ok(());
                }
                bail!("station Bluetooth command failed: {line}")
            }
            if line.contains("failure opening AV connection") {
                bail!("station Bluetooth connection failed")
            }
        }
        if let Ok(Some(status)) = session.child.try_wait() {
            bail!("Bluetooth helper exited with {status}")
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!("timed out waiting for station Bluetooth command result")
}

fn demo_connect(address: &str) -> Result<()> {
    let mut session = demo_session()?;
    if session
        .as_mut()
        .is_some_and(|value| value.child.try_wait().ok().flatten().is_some())
    {
        *session = None;
    }
    if session.is_none() {
        *session = Some(start_demo_session()?);
    }
    let value = session
        .as_mut()
        .expect("Bluetooth helper session initialized");
    value.stdin.write_all(b"2\n")?;
    value.stdin.flush()?;
    thread::sleep(Duration::from_millis(500));
    value
        .stdin
        .write_all(format!("12\n{address}\n").as_bytes())?;
    value.stdin.flush()?;
    let result = wait_demo_result(value, "bt_dispatch_command 11", Duration::from_secs(12));
    if result.is_err() {
        drop(session);
        stop_demo_session();
    }
    result
}

fn demo_disconnect(address: &str) -> Result<()> {
    let mut session = demo_session()?;
    if session
        .as_mut()
        .is_some_and(|value| value.child.try_wait().ok().flatten().is_some())
    {
        *session = None;
    }
    if session.is_none() {
        *session = Some(start_demo_session()?);
    }
    let value = session
        .as_mut()
        .expect("Bluetooth helper session initialized");
    value
        .stdin
        .write_all(format!("11\n{}\n", cached_role(address)).as_bytes())?;
    value.stdin.flush()?;
    let result = wait_demo_result(value, "bt_dispatch_command 10", Duration::from_secs(4));
    drop(session);
    thread::sleep(Duration::from_millis(500));
    stop_demo_session();
    result
}

pub fn pairable(on: bool) -> Result<()> {
    if ctl_available() {
        command_output(&["pairable", if on { "on" } else { "off" }])?;
        return Ok(());
    }
    // On Quasar stations Bluetooth is owned by maind/mediad. Do not start
    // the standalone BSA demo: it creates a second A2DP path and bypasses the
    // native BluetoothCapability and BLUETOOTH_STREAM pipeline.
    native_quasar_command(if on {
        "включи bluetooth"
    } else {
        "выключи bluetooth"
    })?;
    if let Ok(mut cached) = PAIRABLE_STATE.get_or_init(|| Mutex::new(None)).lock() {
        *cached = Some(on);
    }
    return Ok(());
    /*
    if !Path::new(BT_DEMO).is_file() {
        bail!("Bluetooth pairing mode is unavailable: station helper is missing")
    }

    stop_demo_session();
    set_bsa_visibility_config(on)?;
    if Path::new(POWER_SCRIPT).is_file() {
        let status = Command::new(POWER_SCRIPT)
            .arg("1")
            .status()
            .context("restart station Bluetooth daemon")?;
        if !status.success() {
            bail!("station Bluetooth daemon restart failed")
        }
        // bsa_server_run.sh returns immediately, while the UART controller
        // and the BSA service become ready asynchronously.  bt_demo can
        // already print its menu during that interval but its first command
        // would otherwise be silently dropped.
        thread::sleep(Duration::from_secs(5));
    }

    let mut session = demo_session()?;
    if session.as_mut().is_some_and(|value| value.child.try_wait().ok().flatten().is_some()) {
        *session = None;
    }
    if session.is_none() {
        *session = Some(start_demo_session()?);
    }
    let value = session.as_mut().expect("Bluetooth helper session initialized");
    wait_demo_text(value, "Select action", Duration::from_secs(8))?;
    // Menu 1 starts A2DP Sink; menu 3 controls discoverable/connectable.
    value.stdin.write_all(b"1\n")?;
    value.stdin.flush()?;
    let service_result = wait_demo_result(value, "bt_dispatch_command 1", Duration::from_secs(15));
    if service_result.is_err() {
        drop(session);
        stop_demo_session();
        return service_result
    }
    value.stdin.write_all(b"3\n")?;
    value.stdin.flush()?;
    wait_demo_text(value, "Choose discoverable", Duration::from_secs(3))?;
    value.stdin.write_all(if on { b"1\n" } else { b"0\n" })?;
    value.stdin.flush()?;
    wait_demo_text(value, "Choose connectable", Duration::from_secs(3))?;
    value.stdin.write_all(if on { b"1\n" } else { b"0\n" })?;
    value.stdin.flush()?;
    let result = wait_demo_result(value, "bt_dispatch_command 2", Duration::from_secs(15));
    if result.is_ok() {
        if let Ok(mut cached) = PAIRABLE_STATE.get_or_init(|| Mutex::new(None)).lock() {
            *cached = Some(on);
        }
    }
    if result.is_err() {
        drop(session);
        stop_demo_session();
    }
    result
    */
}

fn stop_demo_session() {
    if let Ok(mut session) = demo_session() {
        if let Some(mut value) = session.take() {
            let _ = value.stdin.write_all(b"99\n");
            let _ = value.stdin.flush();
            let _ = value.child.kill();
            let _ = value.child.wait();
        }
    }
}

fn cached_role(address: &str) -> u8 {
    cached_devices()
        .into_iter()
        .find(|device| {
            device
                .get("address")
                .and_then(Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case(address))
        })
        .and_then(|device| {
            device.get("role").and_then(Value::as_str).map(|role| {
                if role.eq_ignore_ascii_case("Audio Source") {
                    1
                } else {
                    0
                }
            })
        })
        .unwrap_or(0)
}

pub fn available() -> bool {
    command_exists("bluetoothctl")
        || fs::read_dir(RFKILL_ROOT).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                fs::read_to_string(entry.path().join("type"))
                    .ok()
                    .is_some_and(|kind| kind.trim() == "bluetooth")
            })
        })
}

pub fn ctl_available() -> bool {
    command_exists("bluetoothctl")
}

pub fn scan_available() -> bool {
    ctl_available() || Path::new(BT_DEMO).is_file()
}

pub fn power_available() -> bool {
    command_exists("bluetoothctl") || Path::new(POWER_SCRIPT).is_file()
}

pub fn status() -> Result<Value> {
    let show = command_output(&["show"]).unwrap_or_default();
    let powered = parse_property(&show, "Powered").or(rfkill_powered()?);
    let devices = if ctl_available() {
        devices().unwrap_or_default()
    } else {
        cached_devices()
    };
    Ok(json!({
        "available": available(),
        "powered": powered,
        "discovering": parse_property(&show, "Discovering"),
        "pairable": parse_property(&show, "Pairable")
            .or(cached_pairable())
            .or(configured_pairable()),
        "server_running": process_running("bluetoothd")?
            || process_running("bsa_server")?,
        "power_control_available": power_available(),
        "power_control_reason": if power_available() { Value::Null } else {
            Value::String("bluetoothctl and the station BSA power script are unavailable".into())
        },
        "devices": devices,
    }))
}

pub fn power(on: bool) -> Result<()> {
    if !on {
        stop_demo_session();
        if let Ok(mut cached) = PAIRABLE_STATE.get_or_init(|| Mutex::new(None)).lock() {
            *cached = Some(false);
        }
    }
    let state = if on { "on" } else { "off" };
    if command_exists("bluetoothctl") {
        match command_output(&["power", state]) {
            Ok(_) => return Ok(()),
            Err(error) => {
                tracing::warn!(%error, "bluetoothctl power command failed; trying station fallback")
            }
        }
    }
    if Path::new(POWER_SCRIPT).is_file() {
        let status = Command::new(POWER_SCRIPT)
            .arg(if on { "1" } else { "0" })
            .status()
            .context("run Bluetooth power control script")?;
        if status.success() {
            return Ok(());
        }
    }
    bail!("Bluetooth power control failed: bluetoothctl and station fallback were unable to set power")
}

pub fn scan(on: bool) -> Result<()> {
    if !on {
        if ctl_available() {
            command_output(&["scan", "off"])?;
        }
        return Ok(());
    }
    if ctl_available() {
        command_output(&["scan", "on"])?;
        return Ok(());
    }
    if Path::new(BT_DEMO).is_file() {
        let _ = demo_scan()?;
        return Ok(());
    }
    bail!(
        "Bluetooth scan is unavailable: bluetoothctl and the station discovery helper are missing"
    )
}

pub fn device_action(action: &str, address: &str) -> Result<()> {
    if address.len() != 17
        || !address.bytes().enumerate().all(|(index, byte)| {
            if [2, 5, 8, 11, 14].contains(&index) {
                byte == b':'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(anyhow!("invalid Bluetooth MAC address"));
    }
    if !matches!(action, "pair" | "connect" | "disconnect" | "remove") {
        bail!("unsupported Bluetooth action {action}")
    }
    if ctl_available() {
        return command_output(&[action, address]).map(|_| ());
    }
    if !Path::new(BT_DEMO).is_file() {
        bail!("Bluetooth action is unavailable: bluetoothctl and the station helper are missing")
    }
    match action {
        // BSA performs bonding as part of the A2DP source connection flow.
        "pair" | "connect" => {
            // bt_demo's source-connect path only allocates an AV connection
            // after the Source Service has been started. Keep the helper alive:
            // its process owns the BSA AV session.
            demo_connect(address)
        }
        "disconnect" => demo_disconnect(address),
        "remove" => bail!("Bluetooth device removal is not exposed by the station helper"),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_demo_devices;

    #[test]
    fn parses_station_discovery_json() {
        let output = br#"
            random helper output
            { "num": 1, "list": [{ "name": "Speaker", "ad": "aa:bb:cc:dd:ee:ff", "role": "Audio Sink" }] }
        "#;
        assert_eq!(
            parse_demo_devices(output),
            vec![serde_json::json!({
                "address": "AA:BB:CC:DD:EE:FF",
                "name": "Speaker",
                "role": "Audio Sink"
            })]
        );
    }
}
