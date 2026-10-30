use crate::process::stop_process_group;
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use clap::{Parser, Subcommand};
use flate2::read::GzDecoder;
use mlua::{HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, VmState};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{Cursor, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::Mutex, task};
use tower_http::trace::TraceLayer;
use url::Url;
use uuid::Uuid;

use crate::rooms::Room;

#[path = "media.rs"]
mod media;
#[path = "providers.rs"]
mod providers;
#[path = "bluetooth.rs"]
mod bluetooth;
#[path = "yam_binary.rs"]
mod yam_binary;
#[path = "dependencies.rs"]
mod dependencies;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8099")]
    listen: String,
    #[arg(long, default_value = "/data/quazar/luna/data")]
    data_dir: PathBuf,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    yam_root: Option<PathBuf>,
    #[arg(long)]
    yam_run: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<RuntimeCommand>,
}

#[derive(Subcommand)]
enum RuntimeCommand {
    /// Read, set or clear the optional room label without starting the runtime.
    Room {
        value: Option<String>,
        #[arg(long, conflicts_with = "value")]
        clear: bool,
    },
}

/// Hardware LED backend supplied by a station crate.
pub trait LedBackend: Send + Sync {
    fn show(&self, frame: &[u8]) -> Result<()>;

    fn restore_ring_trigger(&self) -> Result<()> {
        Ok(())
    }

    fn restore_logo_trigger(&self) -> Result<()> {
        Ok(())
    }

    fn show_logo(&self, _frame: &[u8]) -> Result<()> {
        bail!("logo LED is not supported by this station")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MicrophoneDevice {
    pub id: String,
    pub name: String,
}

pub trait MicrophoneBackend: Send + Sync {
    fn is_available(&self) -> Result<bool>;
    fn devices(&self) -> Result<Vec<MicrophoneDevice>>;
    fn record(&self, device: Option<&str>, seconds: u8, cancelled: &AtomicBool) -> Result<Vec<u8>>;
}

#[derive(Debug, Clone)]
pub enum ClockCommand {
    Time { text: String, brightness: u8 },
    Segments(HashMap<String, u8>),
    Off,
}

pub trait ClockBackend: Send + Sync {
    fn segments(&self) -> Vec<String>;
    fn apply(&self, command: ClockCommand) -> Result<()>;
}

#[derive(Debug, Clone, Copy)]
pub enum ColorOrder {
    Rgb,
    Bgr,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct LedTopology {
    pub ring_count: usize,
    pub has_logo: bool,
    pub logo_count: usize,
}

#[derive(Clone, Copy)]
pub struct InputAction {
    pub name: &'static str,
    pub device: &'static str,
    pub kind: u16,
    pub code: u16,
}

#[derive(Clone, Copy)]
pub struct InputGesture {
    pub name: &'static str,
    pub actions: &'static [&'static str],
    pub max_gap_ms: u64,
}

#[derive(Clone, Copy)]
pub struct GpioInput {
    pub name: &'static str,
    pub path: &'static str,
    pub active_level: u8,
    pub code: u16,
    pub action: &'static str,
}

/// Hardware contract supplied by a station crate.
///
/// The gateway owns API, permissions and package execution. A station crate
/// owns this contract and the hardware implementation behind it.
#[derive(Clone, Copy)]
pub struct StationSpec {
    pub model: &'static str,
    pub led_profile: &'static str,
    pub led_count: usize,
    pub led_topology: LedTopology,
    pub color_order: ColorOrder,
    pub create_led_backend: fn() -> Result<Box<dyn LedBackend>>,
    pub create_microphone_backend: fn() -> Option<Box<dyn MicrophoneBackend>>,
    pub create_clock_backend: fn() -> Option<Box<dyn ClockBackend>>,
    pub capabilities: &'static [&'static str],
    pub input_actions: &'static [InputAction],
    pub input_gestures: &'static [InputGesture],
    pub gpio_inputs: &'static [GpioInput],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    led: LedSettings,
    #[serde(default)]
    web: WebSettings,
    #[serde(default)]
    allow_plugin_binaries: bool,
    #[serde(default)]
    updates: UpdateSettings,
    #[serde(default)]
    lua: LuaSettings,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LuaSettings {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_lua_share_dir")]
    share_dir: String,
    #[serde(default = "default_lua_runtime_seconds")]
    max_runtime_seconds: u64,
    #[serde(default)]
    allow_infinite_loops: bool,
    #[serde(default)]
    permissions: LuaPermissions,
    #[serde(default)]
    network_allow: Vec<String>,
    #[serde(default)]
    input_devices: Vec<String>,
    #[serde(default)]
    ipc_services: Vec<String>,
    #[serde(default)]
    command_phrases: Vec<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LuaPermissions {
    #[serde(default)]
    led: bool,
    #[serde(default)]
    volume: bool,
    #[serde(default)]
    audio: bool,
    #[serde(default)]
    microphone: bool,
    #[serde(default)]
    bluetooth: bool,
    #[serde(default)]
    display: bool,
    #[serde(default)]
    sensors: bool,
    #[serde(default)]
    clock: bool,
    #[serde(default)]
    input: bool,
    #[serde(default)]
    network: bool,
    #[serde(default)]
    ipc_read: bool,
    #[serde(default)]
    ipc_control: bool,
    #[serde(default)]
    commands: bool,
    #[serde(default)]
    system: bool,
    #[serde(default)]
    exec: bool,
    #[serde(default)]
    web: bool,
}
fn default_lua_share_dir() -> String {
    "share/lua".into()
}
fn default_lua_runtime_seconds() -> u64 {
    30
}
impl Default for LuaSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            share_dir: default_lua_share_dir(),
            max_runtime_seconds: default_lua_runtime_seconds(),
            allow_infinite_loops: false,
            permissions: LuaPermissions::default(),
            network_allow: Vec::new(),
            input_devices: Vec::new(),
            ipc_services: Vec::new(),
            command_phrases: Vec::new(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateSettings {
    #[serde(default = "default_updates_enabled")]
    enabled: bool,
    #[serde(default = "default_update_interval")]
    interval_minutes: u64,
}
fn default_updates_enabled() -> bool {
    false
}
fn default_update_interval() -> u64 {
    360
}
impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: default_update_interval(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WebSettings {
    #[serde(default = "default_true")]
    ui: bool,
    #[serde(default = "default_true")]
    api: bool,
}
fn default_true() -> bool {
    true
}
impl Default for WebSettings {
    fn default() -> Self {
        Self {
            ui: true,
            api: true,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LedSettings {
    profile: String,
    #[serde(default = "default_led_count")]
    count: usize,
    #[serde(default = "default_led_root")]
    sysfs_root: String,
    #[serde(default)]
    path_template: Option<String>,
}
impl Default for LedSettings {
    fn default() -> Self {
        Self {
            profile: "custom".into(),
            count: default_led_count(),
            sysfs_root: default_led_root(),
            path_template: None,
        }
    }
}
fn default_led_count() -> usize {
    6
}
fn default_led_root() -> String {
    "/sys/class/leds".into()
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            // Unknown hardware gets the `custom` template; `main` swaps in a detected profile.
            led: LedSettings::default(),
            web: WebSettings::default(),
            allow_plugin_binaries: false,
            updates: UpdateSettings::default(),
            lua: LuaSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    /// Runtime aliases, not a substitute for YAM package-level depends.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    media_provider: Option<providers::Declaration>,
    id: String,
    name: String,
    version: String,
    entry: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    settings: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    network: NetworkPolicy,
    #[serde(default)]
    input: InputPolicy,
    #[serde(default)]
    ipc: IpcPolicy,
    #[serde(default)]
    commands: CommandPolicy,
    #[serde(default)]
    compatibility: Compatibility,
    #[serde(default)]
    launch: Launch,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct NetworkPolicy {
    /// Exact host names or a DNS suffix prefixed with `*.`.
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    allow_insecure_http: bool,
}
/// Input devices a package may read through `alice.input.next_event`, by kernel device name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct InputPolicy {
    #[serde(default)]
    devices: Vec<String>,
}
/// Quasar services a package may access through the local IPC bridge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IpcPolicy {
    #[serde(default)]
    services: Vec<String>,
}
/// Voice commands a package may receive from aliced.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CommandPolicy {
    #[serde(default)]
    phrases: Vec<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Launch {
    #[serde(default)]
    kind: LaunchKind,
    #[serde(default)]
    mode: LaunchMode,
    #[serde(default)]
    path: String,
    #[serde(default)]
    args: Vec<String>,
    /// Runs `path` through this interpreter; only `sh` is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interpreter: Option<String>,
    /// Package-relative directories prepended to `LD_LIBRARY_PATH`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    library_paths: Vec<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum LaunchKind {
    #[default]
    Lua,
    Binary,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum LaunchMode {
    #[default]
    Oneshot,
    Service,
}
impl Launch {
    fn path<'a>(&'a self, manifest: &'a Manifest) -> &'a str {
        if self.path.is_empty() {
            &manifest.entry
        } else {
            &self.path
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Compatibility {
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    features: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceProfile {
    model: String,
    architecture: String,
    features: Vec<String>,
    source: String,
    #[serde(default)]
    led: LedTopology,
}
#[derive(Debug, Serialize)]
struct DeviceMetadata {
    #[serde(flatten)]
    profile: DeviceProfile,
    room: Option<String>,
}
#[derive(Debug, Clone)]
struct Sensor {
    id: String,
    kind: String,
    path: PathBuf,
    unit: String,
}
#[derive(Debug, Serialize)]
struct SensorInfo {
    id: String,
    kind: String,
    unit: String,
    value: Option<f64>,
}
#[derive(Debug, Serialize)]
struct ClockInfo {
    available: bool,
    text_available: bool,
    segments: Vec<String>,
}
#[derive(Debug, Deserialize)]
struct ClockRequest {
    #[serde(default)]
    time: Option<String>,
    #[serde(default)]
    brightness: Option<u8>,
    #[serde(default)]
    segments: Option<HashMap<String, u8>>,
}
#[derive(Debug, Deserialize)]
struct InputQuery {
    device: Option<String>,
    timeout_ms: Option<u64>,
    max_events: Option<usize>,
    idle_ms: Option<u64>,
    #[serde(default)]
    cancel: bool,
}
#[derive(Debug, Deserialize)]
struct AliceCommandQuery {
    timeout_ms: Option<u64>,
    #[serde(default)]
    cancel: bool,
}
#[derive(Debug, Serialize)]
struct InputDeviceInfo {
    name: String,
    path: PathBuf,
}
#[derive(Debug, Serialize)]
struct InputEventInfo {
    device: String,
    kind: u16,
    kind_name: &'static str,
    code: u16,
    value: i32,
    value_name: Option<&'static str>,
    action: Option<&'static str>,
    time_ms: i64,
}
#[derive(Debug, Serialize)]
struct InputGestureInfo {
    gesture: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MicrophoneRecordRequest {
    #[serde(default)]
    device: Option<String>,
    seconds: u8,
}

async fn get_microphone_devices(
    State(a): State<App>,
) -> Result<Json<Vec<MicrophoneDevice>>, ApiError> {
    let backend = a
        .microphone
        .clone()
        .context("microphone capture is not available")?;
    Ok(Json(backend.devices()?))
}

async fn record_microphone(
    State(a): State<App>,
    Json(request): Json<MicrophoneRecordRequest>,
) -> Result<Response, ApiError> {
    if !(1..=30).contains(&request.seconds) {
        return Err(ApiError(anyhow!(
            "microphone capture duration must be 1..=30 seconds"
        )));
    }
    let backend = a
        .microphone
        .clone()
        .context("microphone capture is not available")?;
    let wav = task::spawn_blocking(move || {
        backend.record(
            request.device.as_deref(),
            request.seconds,
            &AtomicBool::new(false),
        )
    })
    .await
    .context("microphone capture task failed")??;
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "audio/wav")
        .header("content-disposition", "attachment; filename=microphone.wav")
        .body(Body::from(wav))
        .context("build microphone response")
        .map_err(ApiError)
}

async fn get_alice_command(
    Query(query): Query<AliceCommandQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let timeout_ms = query.timeout_ms.unwrap_or(60_000).min(60_000);
    let event = task::spawn_blocking(move || wait_alice_speech(timeout_ms, true))
        .await
        .context("Alice command reader task failed")??
        .context("timeout waiting for an Alice command")?;
    if query.cancel {
        cancel_alice_dialog()?;
    }
    Ok(Json(event))
}

async fn get_alice_speech(
    Query(query): Query<InputQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let timeout_ms = query.timeout_ms.unwrap_or(60_000).min(60_000);
    let cancel = query.cancel;
    // Expose interim STT updates normally. In cancel mode the purpose is to
    // intercept the voice input before the cloud request starts, so cancel on
    // the first non-empty partial transcript rather than waiting for final.
    let event = task::spawn_blocking(move || wait_alice_speech(timeout_ms, false))
        .await
        .context("Alice speech reader task failed")??
        .context("timeout waiting for speech")?;
    if cancel {
        task::spawn_blocking(cancel_alice_dialog)
            .await
            .context("Alice speech cancel task failed")??;
    }
    Ok(Json(event))
}

/// Starts the station recognizer in recognition-only mode and returns the
/// final transcript. Unlike the regular Alice speech endpoint this does not
/// start a cloud dialog or wait for an Alice response.
async fn recognize_alice_speech(
    Query(query): Query<InputQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let timeout_ms = query.timeout_ms.unwrap_or(60_000).min(60_000);
    task::spawn_blocking(start_alice_recognition)
        .await
        .context("Alice recognition start task failed")??;
    let event = task::spawn_blocking(move || wait_alice_speech(timeout_ms, true))
        .await
        .context("Alice recognition reader task failed")??
        .context("timeout waiting for recognition")?;
    Ok(Json(event))
}

async fn cancel_alice_speech() -> Result<StatusCode, ApiError> {
    task::spawn_blocking(cancel_alice_dialog)
        .await
        .context("Alice speech cancel task failed")??;
    Ok(StatusCode::OK)
}

async fn get_gpio_input(
    State(a): State<App>,
    AxumPath(input_name): AxumPath<String>,
    Query(query): Query<InputQuery>,
) -> Result<Json<Option<InputEventInfo>>, ApiError> {
    let input = a
        .gpio_inputs
        .iter()
        .find(|input| input.name == input_name)
        .with_context(|| format!("gpio input is not supported: {input_name}"))?;
    let timeout_ms = query.timeout_ms.unwrap_or(60_000).min(60_000);
    let input = *input;
    let event = task::spawn_blocking(move || -> Result<Option<InputEventInfo>> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut previous = fs::read_to_string(input.path)?
            .trim()
            .parse::<u8>()
            .context("invalid GPIO value")?;
        loop {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            let current = fs::read_to_string(input.path)?
                .trim()
                .parse::<u8>()
                .context("invalid GPIO value")?;
            if current == input.active_level && previous != input.active_level {
                return Ok(Some(InputEventInfo {
                    device: format!("gpio-{}", input.name),
                    kind: 1,
                    kind_name: "EV_KEY",
                    code: input.code,
                    value: 1,
                    value_name: Some("press"),
                    action: Some(input.action),
                    time_ms: chrono_like_now_ms(),
                }));
            }
            previous = current;
            std::thread::sleep(Duration::from_millis(10));
        }
    })
    .await
    .context("gpio input reader task failed")??;
    Ok(Json(event))
}

fn wait_gpio_transition(input: GpioInput, timeout_ms: u64) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.min(60_000));
    let mut previous = fs::read_to_string(input.path)?
        .trim()
        .parse::<u8>()
        .context("invalid GPIO value")?;
    loop {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        let current = fs::read_to_string(input.path)?
            .trim()
            .parse::<u8>()
            .context("invalid GPIO value")?;
        if current == input.active_level && previous != input.active_level {
            return Ok(true);
        }
        previous = current;
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn chrono_like_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}
fn input_action_for(actions: &[InputAction], event: &InputEvent) -> Option<&'static str> {
    actions
        .iter()
        .find(|action| {
            action.device == event.device && action.kind == event.kind && action.code == event.code
        })
        .map(|action| action.name)
}
fn input_event_info(actions: &[InputAction], event: InputEvent) -> InputEventInfo {
    InputEventInfo {
        kind_name: input_kind_name(event.kind),
        value_name: input_key_value_name(event.kind, event.value),
        action: input_action_for(actions, &event),
        device: event.device,
        kind: event.kind,
        code: event.code,
        value: event.value,
        time_ms: event.time_ms,
    }
}
#[derive(Debug, Clone, Serialize)]
struct PluginInfo {
    manifest: Manifest,
    /// Absolute package directory on the station, used by the package manager.
    directory: PathBuf,
    enabled: bool,
    service_enabled: bool,
    running: bool,
    compatible: bool,
    incompatibility: Option<String>,
    source: PluginSource,
    package: Option<String>,
}
#[derive(Clone)]
struct App {
    root: PathBuf,
    settings: Arc<Mutex<Settings>>,
    device: Arc<Mutex<DeviceProfile>>,
    led: Arc<Led>,
    microphone: Option<Arc<dyn MicrophoneBackend>>,
    clock: Option<Arc<dyn ClockBackend>>,
    plugins: Arc<Mutex<PluginManager>>,
    runtime_tokens: Arc<Mutex<HashMap<String, String>>>,
    speech_readers:
        Arc<Mutex<HashMap<String, (String, Arc<std::sync::Mutex<media::SpeechReader>>)>>>,
    speech_claims: Arc<Mutex<HashMap<String, (String, String, String, Instant, bool)>>>,
    providers: Arc<Mutex<providers::Broker>>,
    custom_web: Arc<Mutex<HashMap<String, CustomWebPage>>>,
    runtime_api_url: String,
    input_actions: &'static [InputAction],
    input_gestures: &'static [InputGesture],
    gpio_inputs: &'static [GpioInput],
}
#[derive(Clone)]
struct Led {
    settings: Arc<Mutex<LedSettings>>,
    backend: Option<Arc<dyn LedBackend>>,
    color_order: ColorOrder,
    topology: LedTopology,
}
#[derive(Deserialize)]
struct Frame {
    colors: Vec<String>,
    #[serde(default = "frame_delay")]
    delay_ms: u64,
}
#[derive(Deserialize)]
struct Invocation {
    #[serde(default)]
    args: Vec<String>,
}
#[derive(Debug, Deserialize)]
struct BluetoothPowerRequest {
    on: bool,
}
#[derive(Debug, Deserialize)]
struct BluetoothScanRequest {
    on: bool,
}
#[derive(Debug, Deserialize)]
struct BluetoothDeviceRequest {
    address: String,
}
#[derive(Debug, Deserialize)]
struct LuaRunRequest {
    file: String,
}
#[derive(Debug, Serialize)]
struct LuaFileInfo {
    file: String,
    bytes: u64,
}
#[derive(Debug, Serialize)]
struct LuaRunInfo {
    id: String,
    file: String,
    status: &'static str,
}
#[derive(Debug, Clone)]
struct CustomWebPage {
    root: PathBuf,
    file: PathBuf,
}
#[derive(Debug, Clone, Serialize)]
struct CustomWebInfo {
    id: String,
    url: String,
}
fn frame_delay() -> u64 {
    100
}

impl Led {
    async fn draw(&self, colors: Vec<String>) -> Result<()> {
        let mut drawn = false;
        if self.ring_count().await > 0 {
            self.draw_ring(colors.clone()).await?;
            drawn = true;
        }
        if self.topology.has_logo {
            self.draw_logo(colors).await?;
            drawn = true;
        }
        if !drawn {
            bail!("no LED zone is available")
        }
        Ok(())
    }

    async fn draw_ring(&self, colors: Vec<String>) -> Result<()> {
        let count = self.ring_count().await;
        self.draw_zone(colors, count, LedZone::Ring).await
    }

    async fn draw_logo(&self, colors: Vec<String>) -> Result<()> {
        self.draw_zone(colors, self.topology.logo_count, LedZone::Logo)
            .await
    }

    async fn draw_zone(&self, colors: Vec<String>, count: usize, zone: LedZone) -> Result<()> {
        let c = self.settings.lock().await.clone();
        if count == 0 {
            bail!("{} LED is not available", zone.name())
        }
        let colors = expand_colors(colors, count, zone.name())?;
        if let Some(backend) = &self.backend {
            let mut frame = Vec::with_capacity(colors.len() * 3);
            for hex in colors {
                let (r, g, b) = parse_color(&hex)?;
                match self.color_order {
                    ColorOrder::Rgb => frame.extend([r, g, b]),
                    ColorOrder::Bgr => frame.extend([b, g, r]),
                }
            }
            let backend = backend.clone();
            return task::spawn_blocking(move || match zone {
                LedZone::Ring => backend.show(&frame),
                LedZone::Logo => backend.show_logo(&frame),
            })
            .await
            .context("LED backend task failed")?;
        }
        if !matches!(zone, LedZone::Ring) {
            bail!("{} LED requires a station backend", zone.name())
        }
        for (i, hex) in colors.iter().enumerate() {
            let (r, g, b) = parse_color(hex)?;
            self.write(&c, "red", i, r).await?;
            self.write(&c, "green", i, g).await?;
            self.write(&c, "blue", i, b).await?;
        }
        Ok(())
    }
    async fn off(&self) -> Result<()> {
        let c = self.settings.lock().await.clone();
        if self.backend.is_some() {
            if self.ring_count().await > 0 {
                self.off_zone(LedZone::Ring).await?;
            }
            if self.topology.has_logo {
                self.off_zone(LedZone::Logo).await?;
            }
            return Ok(());
        }
        self.draw(vec!["#000000".into(); c.count]).await
    }

    async fn off_ring(&self) -> Result<()> {
        self.off_zone(LedZone::Ring).await
    }

    async fn off_logo(&self) -> Result<()> {
        self.off_zone(LedZone::Logo).await
    }

    async fn off_zone(&self, zone: LedZone) -> Result<()> {
        let count = match zone {
            LedZone::Ring => self.ring_count().await,
            LedZone::Logo => self.topology.logo_count,
        };
        if count == 0 {
            bail!("{} LED is not available", zone.name())
        }
        let backend = self
            .backend
            .clone()
            .context("LED off requires a station backend")?;
        let frame = vec![0; count * 3];
        task::spawn_blocking(move || {
            match zone {
                LedZone::Ring => backend.show(&frame)?,
                LedZone::Logo => backend.show_logo(&frame)?,
            }
            match zone {
                LedZone::Ring => backend.restore_ring_trigger(),
                LedZone::Logo => backend.restore_logo_trigger(),
            }
        })
        .await
        .context("LED backend task failed")?
    }

    async fn ring_count(&self) -> usize {
        if self.topology.ring_count > 0 {
            self.topology.ring_count
        } else {
            self.settings.lock().await.count
        }
    }
    async fn write(&self, c: &LedSettings, channel: &str, i: usize, value: u8) -> Result<()> {
        let path = led_brightness_path(c, channel, i)?;
        fs::write(&path, value.to_string()).with_context(|| format!("write {}", path.display()))
    }
    // Writing brightness detaches the `ledfb-ring` trigger used by platformd.
    async fn play_led(&self, path: PathBuf, cancelled: &AtomicBool) -> Result<()> {
        let source = fs::read_to_string(&path)
            .with_context(|| format!("read pattern {}", path.display()))?;
        let (looped, frames) = parse_pattern(&source)?;
        if frames.is_empty() {
            bail!("pattern has no frames")
        };
        loop {
            for f in &frames {
                self.draw(f.colors.clone()).await?;
                let mut left = f.delay_ms;
                while left > 0 {
                    if cancelled.load(Ordering::Relaxed) {
                        bail!("plugin stopped")
                    }
                    let slice = left.min(20);
                    tokio::time::sleep(Duration::from_millis(slice)).await;
                    left -= slice;
                }
            }
            if !looped {
                break;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum LedZone {
    Ring,
    Logo,
}

impl LedZone {
    fn name(self) -> &'static str {
        match self {
            Self::Ring => "ring",
            Self::Logo => "logo",
        }
    }
}
fn led_brightness_path(c: &LedSettings, channel: &str, index: usize) -> Result<PathBuf> {
    if c.profile != "custom" {
        bail!("hardware LED profiles are provided by the station crate")
    }
    Ok(PathBuf::from(
        c.path_template
            .as_ref()
            .context("custom profile needs path_template")?
            .replace("{root}", &c.sysfs_root)
            .replace("{channel}", channel)
            .replace("{index}", &(index + 1).to_string()),
    ))
}
fn parse_color(s: &str) -> Result<(u8, u8, u8)> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid RGB color {s}")
    };
    Ok((
        u8::from_str_radix(&h[0..2], 16)?,
        u8::from_str_radix(&h[2..4], 16)?,
        u8::from_str_radix(&h[4..6], 16)?,
    ))
}

/// Expands a compact color frame evenly across a physical LED zone.
/// For example, 2 colors on 24 LEDs become 12 LEDs of each color.
fn expand_colors(colors: Vec<String>, count: usize, zone: &str) -> Result<Vec<String>> {
    if colors.is_empty() {
        bail!("at least one {zone} color is required")
    }
    if colors.len() > count {
        bail!(
            "expected at most {} {} colors, got {}",
            count,
            zone,
            colors.len()
        )
    }
    if colors.len() == count {
        return Ok(colors);
    }
    Ok((0..count)
        .map(|index| colors[index * colors.len() / count].clone())
        .collect())
}
fn parse_pattern(s: &str) -> Result<(bool, Vec<Frame>)> {
    let mut looped = false;
    let mut frames = vec![];
    for line in s
        .lines()
        .map(str::trim)
        .filter(|x| !x.is_empty() && !x.starts_with('#'))
    {
        if line.eq_ignore_ascii_case("loop") {
            looped = true;
            continue;
        }
        if line.eq_ignore_ascii_case("gammacorrection") {
            continue;
        };
        let p: Vec<_> = line.split_whitespace().collect();
        if p.len() < 2 {
            continue;
        };
        let delay = p.last().unwrap().parse().context("bad frame delay")?;
        frames.push(Frame {
            colors: p[..p.len() - 1].iter().map(|x| format!("#{x}")).collect(),
            delay_ms: delay,
        });
    }
    Ok((looped, frames))
}

struct RunningPackage {
    cancelled: Arc<AtomicBool>,
    finished: tokio::sync::watch::Receiver<bool>,
}

impl RunningPackage {
    async fn stop(&self, timeout: Duration) -> Result<()> {
        self.cancelled.store(true, Ordering::Relaxed);
        let mut finished = self.finished.clone();
        tokio::time::timeout(timeout, async {
            while !*finished.borrow_and_update() {
                finished.changed().await
                    .context("package task ended without cleanup confirmation")?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("package is still stopping; cleanup has not completed")??;
        Ok(())
    }
}

struct PluginManager {
    root: PathBuf,
    yam_root: Option<PathBuf>,
    yam_run: Option<PathBuf>,
    enabled: HashSet<String>,
    service_enabled: HashSet<String>,
    running: HashMap<String, Arc<RunningPackage>>,
    lifecycle: Arc<Mutex<()>>,
    shutting_down: bool,
}
fn yam_root_from_executable(executable: &Path) -> Option<PathBuf> {
    let bin = executable.parent()?;
    if bin.file_name()? != "bin" || bin.parent()?.parent()?.file_name()? != "luna" {
        return None;
    }
    let yam = bin.parent()?.parent()?.parent()?;
    if yam.file_name()? != "yam" || yam.parent()?.file_name()? != "pkgs" {
        return None;
    }
    yam.parent()?.parent().map(Path::to_path_buf)
}
fn resolve_yam_paths(args: &Args) -> (Option<PathBuf>, Option<PathBuf>) {
    let root = args
        .yam_root
        .clone()
        .or_else(|| std::env::var_os("YAM_ROOT").map(PathBuf::from))
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| yam_root_from_executable(&path))
        });
    let run = args
        .yam_run
        .clone()
        .or_else(|| std::env::var_os("YAM_RUN").map(PathBuf::from));
    (root, run)
}
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum PluginSource {
    Local,
    Yam,
}
#[derive(Debug, Clone)]
struct PluginLocation {
    root: PathBuf,
    source: PluginSource,
    package: Option<String>,
}
fn is_local_package_dir(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name != "plugin-state" && !name.starts_with("user_lua_")
}
impl PluginManager {
    fn new(root: PathBuf, yam_root: Option<PathBuf>, yam_run: Option<PathBuf>) -> Result<Self> {
        fs::create_dir_all(&root)?;
        let enabled = match fs::read_to_string(root.join("enabled.json")) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                tracing::warn!(%error, "enabled.json is corrupt; all packages start disabled");
                HashSet::new()
            }),
            Err(_) => HashSet::new(),
        };
        let service_path = root.join("services.json");
        let service_enabled = match fs::read_to_string(&service_path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                tracing::warn!(%error, "services.json is corrupt; no packages auto-start");
                HashSet::new()
            }),
            Err(_) => {
                // Migrate old mode-based services exactly once when no explicit
                // service state exists yet.
                let legacy = fs::read_dir(&root)?
                    .filter_map(|entry| entry.ok())
                    .filter_map(|entry| {
                        let manifest = fs::read_to_string(entry.path().join("plugin.toml")).ok()?;
                        let manifest = toml::from_str::<Manifest>(&manifest).ok()?;
                        (enabled.contains(&manifest.id)
                            && manifest.launch.mode == LaunchMode::Service)
                            .then_some(manifest.id)
                    })
                    .collect::<HashSet<_>>();
                if !legacy.is_empty() {
                    write_atomic(&service_path, &serde_json::to_vec_pretty(&legacy)?)?;
                }
                legacy
            }
        };
        Ok(Self {
            root,
            yam_root,
            yam_run,
            enabled,
            service_enabled,
            running: HashMap::new(),
            lifecycle: Arc::new(Mutex::new(())),
            shutting_down: false,
        })
    }
    fn save(&self) -> Result<()> {
        write_atomic(
            &self.root.join("enabled.json"),
            &serde_json::to_vec_pretty(&self.enabled)?,
        )?;
        write_atomic(
            &self.root.join("services.json"),
            &serde_json::to_vec_pretty(&self.service_enabled)?,
        )
    }
    fn yam_plugins_root(&self) -> Option<PathBuf> {
        self.yam_root
            .as_ref()
            .map(|root| root.join("share/luna/plugins"))
    }
    fn state_root(&self, id: &str) -> PathBuf {
        self.root.join("plugin-state").join(id)
    }
    fn yam_location(&self, id: &str) -> Option<PluginLocation> {
        let root = self.yam_plugins_root()?;
        let path = root.join(id);
        (path.is_dir() && validate_yam_id(id).is_ok()).then_some(PluginLocation {
            root: path,
            source: PluginSource::Yam,
            package: Some(id.replace('.', "/")),
        })
    }
    fn location(&self, id: &str) -> Result<PluginLocation> {
        validate_id_or_yam(id)?;
        if let Some(location) = self.yam_location(id) {
            return Ok(location);
        }
        let root = self.root.join(id);
        if !root.is_dir() {
            bail!("plugin not found")
        }
        Ok(PluginLocation {
            root,
            source: PluginSource::Local,
            package: None,
        })
    }
    fn with_yam_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let Some(run) = &self.yam_run else {
            return operation();
        };
        let lock_path = run.join("yam.lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .open(&lock_path)
            .with_context(|| format!("open YAM lock {}", lock_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let mut locked = false;
            for _ in 0..30 {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
                    locked = true;
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            if !locked {
                bail!("YAM lock is busy")
            }
            let result = operation();
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return result;
        }
        #[cfg(not(unix))]
        operation()
    }
    fn list(&self, device: &DeviceProfile) -> Result<Vec<PluginInfo>> {
        self.with_yam_lock(|| {
            let mut out = vec![];
            let mut locations = Vec::new();
            for e in fs::read_dir(&self.root)? {
                let e = e?;
                if e.file_type()?.is_dir() && is_local_package_dir(&e.file_name()) {
                    locations.push(PluginLocation { root: e.path(), source: PluginSource::Local, package: None });
                }
            }
            if let Some(root) = self.yam_plugins_root() {
                if root.is_dir() {
                    for e in fs::read_dir(root)? {
                        let e = e?;
                        let id = e.file_name().to_string_lossy().to_string();
                        if !e.file_type()?.is_dir() || validate_yam_id(&id).is_err() {
                            tracing::warn!(path=%e.path().display(), "ignoring invalid YAM plugin directory");
                            continue;
                        }
                        locations.push(PluginLocation { root: e.path(), source: PluginSource::Yam, package: Some(id.replace('.', "/")) });
                    }
                }
            }
            for location in locations {
                let p = location.root.join("plugin.toml");
                let Ok(x) = fs::read_to_string(&p) else {
                    tracing::warn!(path=%p.display(), "ignoring plugin without plugin.toml");
                    continue;
                };
                let Ok(manifest) = toml::from_str::<Manifest>(&x) else {
                    tracing::warn!(path=%p.display(), "ignoring plugin with invalid plugin.toml");
                    continue;
                };
                if location.source == PluginSource::Yam && manifest.id != location.root.file_name().unwrap_or_default().to_string_lossy() {
                    tracing::warn!(id=%manifest.id, path=%location.root.display(), "ignoring YAM plugin with mismatched id");
                    continue;
                }
                let reason = compatibility_error(&manifest, device);
                let reason = if location.source == PluginSource::Yam && manifest.launch.kind == LaunchKind::Binary {
                    if self.yam_run.is_none() {
                        Some("YAM binary plugins require --yam-run or YAM_RUN".into())
                    } else {
                        yam_binary::pinned_root(self.yam_root.as_ref().unwrap(), &location.root, &manifest)
                            .err().map(|error| error.to_string()).or(reason)
                    }
                } else { reason };
                out.push(PluginInfo {
                    directory: location.root,
                    enabled: self.enabled.contains(&manifest.id),
                    service_enabled: self.service_enabled.contains(&manifest.id),
                    running: self.running.contains_key(&manifest.id),
                    compatible: reason.is_none(),
                    incompatibility: reason,
                    source: location.source,
                    package: location.package,
                    manifest,
                });
            }
            Ok(out)
        })
    }
    fn rescan(&mut self, device: &DeviceProfile) -> Result<Vec<PluginInfo>> {
        let visible = self.list(device)?;
        let visible_ids: HashSet<_> = visible
            .iter()
            .map(|plugin| plugin.manifest.id.as_str())
            .collect();
        let removed: Vec<_> = self
            .enabled
            .iter()
            .filter(|id| id.contains('.') && !visible_ids.contains(id.as_str()))
            .cloned()
            .collect();
        let mut changed = false;
        for id in removed {
            self.enabled.remove(&id);
            self.service_enabled.remove(&id);
            if let Some(run) = self.running.get(&id) {
                run.cancelled.store(true, Ordering::Relaxed);
            }
            changed = true;
            tracing::info!(plugin=%id, "YAM plugin disappeared; stopped and removed from enabled state");
        }
        if changed {
            self.save()?;
        }
        Ok(visible)
    }
    fn manifest(&self, id: &str) -> Result<Manifest> {
        let location = self.location(id)?;
        Ok(toml::from_str(&fs::read_to_string(
            location.root.join("plugin.toml"),
        )?)?)
    }
    fn location_and_manifest(&self, id: &str) -> Result<(PluginLocation, Manifest)> {
        let location = self.location(id)?;
        let manifest = toml::from_str(&fs::read_to_string(location.root.join("plugin.toml"))?)?;
        Ok((location, manifest))
    }
    fn install(&mut self, bytes: &[u8]) -> Result<Manifest> {
        let stage = self.root.join(format!(".install-{}", Uuid::new_v4()));
        let budget = unpack_budget(&self.root)?;
        fs::create_dir(&stage)?;
        let result = self.install_staged(bytes, &stage, budget);
        if result.is_err() {
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }
    fn install_staged(&self, bytes: &[u8], stage: &Path, budget: u64) -> Result<Manifest> {
        if bytes.starts_with(b"PK\x03\x04") {
            extract_zip_yap(bytes, stage, budget)?
        } else if bytes.starts_with(&[0x1f, 0x8b]) {
            extract_tar_gz_yap(bytes, stage, budget)?
        } else {
            bail!(".yap must be a ZIP or gzip-compressed TAR archive")
        }
        let manifest_text = fs::read_to_string(stage.join("plugin.toml"))
            .context(".yap must contain root plugin.toml")?;
        let m: Manifest = toml::from_str(&manifest_text)?;
        if m.id.contains('.') && self.yam_location(&m.id).is_some() {
            bail!(
                "плагин установлен через YAM, удаляйте его через `yam remove {}`",
                m.id.replace('.', "/")
            )
        }
        validate_id(&m.id)?;
        safe_relative(&m.entry)?;
        safe_relative(m.launch.path(&m))?;
        for directory in &m.launch.library_paths {
            safe_relative(directory)?;
        }
        if m.launch.interpreter.as_deref().is_some_and(|x| x != "sh") {
            bail!("launch.interpreter supports only \"sh\"")
        }
        if m.launch.kind == LaunchKind::Binary && !m.permissions.iter().any(|p| p == "exec") {
            bail!("binary packages require the exec permission")
        }
        validate_input_policy(&m)?;
        validate_ipc_policy(&m)?;
        validate_command_policy(&m)?;
        if let Some(provider) = &m.media_provider {
            provider.validate()?;
            if !m.permissions.iter().any(|p| p == "media.provider") {
                bail!("media_provider requires media.provider permission");
            }
        }
        if m.launch.kind == LaunchKind::Binary {
            let executable = stage.join(m.launch.path(&m));
            if !executable.is_file() {
                bail!("binary launch path does not name a file")
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for entry in walkdir::WalkDir::new(stage) {
                    let entry = entry?;
                    if entry.file_type().is_file() {
                        let mut permissions = fs::metadata(entry.path())?.permissions();
                        permissions.set_mode(permissions.mode() | 0o111);
                        fs::set_permissions(entry.path(), permissions)?;
                    }
                }
            }
        }
        let target = self.root.join(&m.id);
        if target.exists() {
            fs::remove_dir_all(&target)?
        };
        fs::rename(stage, target)?;
        Ok(m)
    }
    fn remove(&mut self, id: &str) -> Result<()> {
        let location = self.location(id)?;
        if location.source == PluginSource::Yam {
            bail!(
                "плагин установлен через YAM, удаляйте его через `yam remove {}`",
                location.package.unwrap_or_else(|| id.replace('.', "/"))
            )
        }
        let p = location.root;
        if !p.exists() {
            bail!("plugin not found")
        };
        if self.running.contains_key(id) {
            bail!("package is still running")
        }
        fs::remove_dir_all(p)?;
        self.enabled.remove(id);
        self.service_enabled.remove(id);
        self.save()
    }
}
fn archive_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("unsafe archive path")
    }
    Ok(path.to_path_buf())
}
fn set_archive_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))?;
    }
    Ok(())
}
const MAX_UNPACKED_BYTES: u64 = 32 * 1024 * 1024;
const FREE_SPACE_RESERVE: u64 = 8 * 1024 * 1024;
fn unpack_budget(directory: &Path) -> Result<u64> {
    Ok(MAX_UNPACKED_BYTES.min(available_bytes(directory)?.saturating_sub(FREE_SPACE_RESERVE)))
}
#[cfg(unix)]
fn available_bytes(directory: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(directory.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("statvfs {}", directory.display()));
    }
    let stat = unsafe { stat.assume_init() };
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}
#[cfg(not(unix))]
fn available_bytes(_: &Path) -> Result<u64> {
    Ok(u64::MAX)
}
fn copy_limited(source: impl Read, output: &mut fs::File, remaining: &mut u64) -> Result<()> {
    let written = std::io::copy(&mut source.take(*remaining + 1), output)?;
    if written > *remaining {
        bail!("package does not fit: unpacked size is limited to 32 MiB and 8 MiB of free space is kept")
    }
    *remaining -= written;
    Ok(())
}
fn extract_zip_yap(bytes: &[u8], stage: &Path, mut remaining: u64) -> Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let path = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("unsafe archive path"))?
            .to_owned();
        let destination = stage.join(path);
        if entry.is_dir() {
            fs::create_dir_all(destination)?;
        } else {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mode = entry.unix_mode();
            let mut output = fs::File::create(&destination)?;
            copy_limited(&mut entry, &mut output, &mut remaining)?;
            if let Some(mode) = mode {
                set_archive_mode(&destination, mode)?;
            }
        }
    }
    Ok(())
}
fn extract_tar_gz_yap(bytes: &[u8], stage: &Path, mut remaining: u64) -> Result<()> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    for item in archive.entries()? {
        let mut entry = item?;
        let path = archive_path(&entry.path()?)?;
        let destination = stage.join(path);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            fs::create_dir_all(destination)?;
        } else if entry_type.is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mode = entry.header().mode()?;
            let mut output = fs::File::create(&destination)?;
            copy_limited(&mut entry, &mut output, &mut remaining)?;
            set_archive_mode(&destination, mode)?;
        } else {
            bail!("unsupported TAR archive entry")
        }
    }
    Ok(())
}
fn compatibility_error(m: &Manifest, d: &DeviceProfile) -> Option<String> {
    if !m.compatibility.models.is_empty() && !m.compatibility.models.iter().any(|x| x == &d.model) {
        return Some(format!(
            "requires model [{}], detected {}",
            m.compatibility.models.join(", "),
            d.model
        ));
    };
    let present: HashSet<_> = d.features.iter().collect();
    let missing: Vec<_> = m
        .compatibility
        .features
        .iter()
        .filter(|x| !present.contains(x))
        .cloned()
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(format!("missing feature(s): {}", missing.join(", ")))
    }
}
fn canonical_model(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}
fn detect_device(root: &Path, forced: Option<String>) -> DeviceProfile {
    if let Ok(raw) = fs::read_to_string(root.join("device.toml")) {
        if let Ok(mut p) = toml::from_str::<DeviceProfile>(&raw) {
            p.model = canonical_model(&p.model);
            return p;
        }
    }
    let arch = std::env::consts::ARCH.to_string();
    let model = forced.unwrap_or_else(|| "unknown".into());
    let model = canonical_model(&model);
    // LED ring features depend on the configured profile and are added in `main`.
    let mut features = vec![
        "runtime.lua.v1".into(),
        "plugin.yap.v1".into(),
        "network".into(),
        "system".into(),
    ];
    if find_amixer().is_some() {
        features.push("audio.volume.alsa".into())
    }
    if crate::audio::available() {
        features.push("audio.play.mediad".into())
    }
    if bluetooth::available() {
        features.extend(["bluetooth.bluez".into(), "bluetooth.status".into()]);
        if bluetooth::scan_available() {
            features.push("bluetooth.scan".into());
            features.extend([
                "bluetooth.pairable".into(),
                "bluetooth.pair".into(),
                "bluetooth.connect".into(),
                "bluetooth.disconnect".into(),
            ]);
        }
        if bluetooth::ctl_available() {
            features.extend([
                "bluetooth.remove".into(),
            ])
        }
    }
    if bluetooth::power_available() {
        features.push("bluetooth.power".into())
    }
    if Path::new("/sys/class/backlight").exists() {
        features.push("display.backlight".into())
    }
    if Path::new("/sys/class/input").exists() {
        features.push("input.evdev".into())
    }
    let sensors = discover_sensors();
    if sensors.iter().any(|sensor| sensor.kind == "ambient_light") {
        features.push("sensor.ambient_light.iio".into())
    }
    if sensors.iter().any(|sensor| sensor.kind == "temperature") {
        features.push("sensor.temperature.sysfs".into())
    }
    if sensors.iter().any(|sensor| sensor.kind == "battery") {
        features.push("battery.sysfs".into())
    }
    if clock_available() {
        features.push("display.clock.segment.v1".into())
    }
    if arch.starts_with("arm") {
        features.push("arch.armhf".into())
    };
    if arch == "aarch64" {
        features.push("arch.aarch64".into())
    }
    DeviceProfile {
        model,
        architecture: arch,
        features,
        source: "auto (override: data-dir/device.toml or --model)".into(),
        led: LedTopology::default(),
    }
}
/// Replaces `path` via a synced sibling file so a power cut never leaves it truncated.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("tmp");
    let mut file =
        fs::File::create(&temporary).with_context(|| format!("write {}", temporary.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
}
fn read_trimmed(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}
fn sorted_directories(path: &Path) -> Vec<PathBuf> {
    let mut entries = fs::read_dir(path)
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}
fn discover_sensors() -> Vec<Sensor> {
    let mut sensors = Vec::new();
    for device in sorted_directories(Path::new("/sys/bus/iio/devices")) {
        let value = device.join("in_intensity_input");
        if value.is_file() {
            let name = device.file_name().unwrap_or_default().to_string_lossy();
            sensors.push(Sensor {
                id: format!("ambient-light:{name}"),
                kind: "ambient_light".into(),
                path: value,
                unit: "lux".into(),
            });
        }
    }
    for zone in sorted_directories(Path::new("/sys/class/thermal")) {
        let value = zone.join("temp");
        if value.is_file() {
            let name = zone.file_name().unwrap_or_default().to_string_lossy();
            sensors.push(Sensor {
                id: format!("temperature:{name}"),
                kind: "temperature".into(),
                path: value,
                unit: "celsius".into(),
            });
        }
    }
    for device in sorted_directories(Path::new("/sys/class/hwmon")) {
        let name = read_trimmed(&device.join("name")).unwrap_or_else(|_| {
            device
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });
        if let Ok(entries) = fs::read_dir(&device) {
            for entry in entries.flatten() {
                let file_name = entry.file_name().to_string_lossy().to_string();
                if file_name.starts_with("temp") && file_name.ends_with("_input") {
                    sensors.push(Sensor {
                        id: format!("temperature:{name}:{file_name}"),
                        kind: "temperature".into(),
                        path: entry.path(),
                        unit: "celsius".into(),
                    });
                }
            }
        }
    }
    for supply in sorted_directories(Path::new("/sys/class/power_supply")) {
        let value = supply.join("capacity");
        if value.is_file() {
            let name = supply.file_name().unwrap_or_default().to_string_lossy();
            sensors.push(Sensor {
                id: format!("battery:{name}"),
                kind: "battery".into(),
                path: value,
                unit: "percent".into(),
            });
        }
    }
    sensors
}
fn sensor_value(sensor: &Sensor) -> Result<f64> {
    let mut value: f64 = read_trimmed(&sensor.path)?.parse()?;
    if sensor.kind == "temperature" && value.abs() > 1_000.0 {
        value /= 1_000.0;
    }
    Ok(value)
}
fn configured_light_sensor(name: &str) -> Result<PathBuf> {
    let sensor = discover_sensors()
        .into_iter()
        .find(|sensor| sensor.id == format!("ambient-light:{name}"))
        .context("ambient light sensor not found")?;
    Ok(sensor
        .path
        .parent()
        .context("invalid ambient light sensor path")?
        .to_path_buf())
}
enum ClockLayout {
    Mini2(String),
    Cucumber(String),
    /// Yandex Station Lite 2 (`plum`). Same `{prefix}clock{segment}` shape as
    /// Mini2, but positions are 0-based and the separator is three separate
    /// segments instead of PLUS/MINUS markers.
    Plum(String),
}
const CUCUMBER_EXTRA_SEGMENTS: [&str; 14] = [
    "H1", "H2", "L1", "L2", "M1", "M2", "P1", "P2", "P4", "P5", "Q1", "Q2", "Q4", "Q5",
];
impl ClockLayout {
    fn segments(&self) -> Vec<String> {
        // plum: four positions 0..3, eight parts A..H, and a three-segment
        // separator (DH/DM/DL) instead of PLUS1/PLUS2/MINUS. DEGREE is the
        // standalone marker next to the digits.
        if let ClockLayout::Plum(_) = self {
            let mut segments = Vec::new();
            for position in 0..=3 {
                for part in ["A", "B", "C", "D", "E", "F", "G", "H"] {
                    segments.push(format!("{part}{position}"));
                }
            }
            segments.extend(["DEGREE".into(), "DH".into(), "DM".into(), "DL".into()]);
            return segments;
        }
        let mut segments = Vec::new();
        for position in 1..=4 {
            for part in ["A", "B", "C", "D", "E", "F", "G"] {
                segments.push(format!("{part}{position}"));
            }
        }
        segments.extend(["PLUS1".into(), "PLUS2".into()]);
        match self {
            ClockLayout::Mini2(_) => segments.extend(["MINUS".into(), "DEGREE".into()]),
            ClockLayout::Cucumber(_) => {
                segments.push("DEGREE".into());
                segments.extend(CUCUMBER_EXTRA_SEGMENTS.iter().map(|x| x.to_string()));
            }
            ClockLayout::Plum(_) => unreachable!(),
        }
        segments
    }
    fn path(&self, segment: &str) -> PathBuf {
        match self {
            ClockLayout::Mini2(prefix) | ClockLayout::Plum(prefix) => {
                Path::new("/sys/class/leds").join(format!("{prefix}clock{segment}/brightness"))
            }
            ClockLayout::Cucumber(prefix) => {
                let name = match segment {
                    "PLUS1" => "p3".into(),
                    "PLUS2" => "q3".into(),
                    "DEGREE" => "deg".into(),
                    other => other.to_lowercase(),
                };
                Path::new("/sys/class/leds").join(format!("{prefix}white:clock-{name}/brightness"))
            }
        }
    }
    fn complete(&self) -> bool {
        self.segments()
            .iter()
            .all(|segment| self.path(segment).is_file())
    }
    // `sync:clock-N` = 1 freezes the Midi display until it is reset to 0.
    fn unlatch(&self) -> Result<()> {
        let ClockLayout::Cucumber(prefix) = self else {
            return Ok(());
        };
        let latch = format!("{prefix}sync:clock-");
        for entry in fs::read_dir("/sys/class/leds")?.flatten() {
            let brightness = entry.path().join("brightness");
            if entry.file_name().to_string_lossy().starts_with(&latch)
                && read_trimmed(&brightness)? != "0"
            {
                fs::write(brightness, "0")?;
            }
        }
        Ok(())
    }
}
// The LED set is fixed by the kernel driver, so the layout is probed once.
fn clock_layout() -> Option<&'static ClockLayout> {
    static LAYOUT: std::sync::OnceLock<Option<ClockLayout>> = std::sync::OnceLock::new();
    LAYOUT.get_or_init(probe_clock_layout).as_ref()
}
fn probe_clock_layout() -> Option<ClockLayout> {
    let names = fs::read_dir("/sys/class/leds")
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    // plum is detected first: its names also end with "clockA1", so the Mini2
    // probe below would otherwise claim it and then fail `complete()`.
    let plum = names
        .iter()
        .filter_map(|name| name.strip_suffix("clockA0"))
        .map(|prefix| ClockLayout::Plum(prefix.into()));
    let upper = names
        .iter()
        .filter_map(|name| name.strip_suffix("clockA1"))
        .map(|prefix| ClockLayout::Mini2(prefix.into()));
    let cucumber = names
        .iter()
        .filter_map(|name| name.strip_suffix("white:clock-a1"))
        .map(|prefix| ClockLayout::Cucumber(prefix.into()));
    plum.chain(upper)
        .chain(cucumber)
        .find(ClockLayout::complete)
}
fn clock_segments() -> Vec<String> {
    clock_layout()
        .map(ClockLayout::segments)
        .unwrap_or_default()
}
fn clock_available() -> bool {
    clock_layout().is_some()
}
// platformd redraws the clock once a minute, so release restores its last frame instead of blanking.
struct ClockOwnership {
    system: Vec<(PathBuf, String)>,
    ours: Vec<(PathBuf, String)>,
}
static CLOCK_OWNERSHIP: std::sync::Mutex<Option<ClockOwnership>> = std::sync::Mutex::new(None);
fn clock_read(layout: &ClockLayout) -> Result<Vec<(PathBuf, String)>> {
    layout
        .segments()
        .iter()
        .map(|segment| {
            let path = layout.path(segment);
            let value = read_trimmed(&path)?;
            Ok((path, value))
        })
        .collect()
}
fn clock_write(segments: &HashMap<String, u8>) -> Result<()> {
    validate_clock_segments(segments)?;
    let _gate = CLOCK_TEXT_GATE.lock().unwrap_or_else(|e| e.into_inner());
    CLOCK_TEXT_EPOCH.fetch_add(1, Ordering::SeqCst);
    clock_write_frame(segments)
}
fn clock_write_frame(segments: &HashMap<String, u8>) -> Result<()> {
    let layout = clock_layout().context("segment clock is not available")?;
    let mut ownership = CLOCK_OWNERSHIP.lock().unwrap_or_else(|e| e.into_inner());
    let current = clock_read(&layout)?;
    if ownership.as_ref().is_none_or(|o| o.ours != current) {
        *ownership = Some(ClockOwnership {
            system: current,
            ours: Vec::new(),
        });
    }
    layout.unlatch()?;
    for segment in layout.segments() {
        let value = segments.get(&segment).copied().unwrap_or(0);
        fs::write(layout.path(&segment), value.to_string())?;
    }
    if let Some(o) = ownership.as_mut() {
        o.ours = clock_read(&layout)?;
    }
    Ok(())
}
fn clock_release() -> Result<()> {
    let _gate = CLOCK_TEXT_GATE.lock().unwrap_or_else(|e| e.into_inner());
    CLOCK_TEXT_EPOCH.fetch_add(1, Ordering::SeqCst);
    clock_release_frame()
}
fn clock_release_frame() -> Result<()> {
    let layout = clock_layout().context("segment clock is not available")?;
    let mut ownership = CLOCK_OWNERSHIP.lock().unwrap_or_else(|e| e.into_inner());
    let Some(o) = ownership.take() else {
        return Ok(());
    };
    if clock_read(&layout)? != o.ours {
        return Ok(());
    }
    layout.unlatch()?;
    for (path, value) in o.system {
        fs::write(path, value)?;
    }
    Ok(())
}
fn validate_clock_segments(segments: &HashMap<String, u8>) -> Result<()> {
    let valid = clock_segments().into_iter().collect::<HashSet<_>>();
    if segments.keys().any(|segment| !valid.contains(segment)) {
        bail!("unknown clock segment")
    }
    Ok(())
}
fn network_url_allowed(policy: &NetworkPolicy, value: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid URL")?;
    match url.scheme() {
        "https" => {}
        "http" if policy.allow_insecure_http => {}
        "http" => bail!("HTTP requires network.allow_insecure_http = true"),
        _ => bail!("only HTTP(S) URLs are allowed"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URLs with credentials are not allowed")
    }
    let host = url
        .host_str()
        .context("URL host is required")?
        .to_ascii_lowercase();
    if host == "localhost" {
        bail!("localhost is not allowed")
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let local = match ip {
            std::net::IpAddr::V4(ip) => {
                ip.is_private()
                    || ip.is_loopback()
                    || ip.is_link_local()
                    || ip.is_broadcast()
                    || ip.is_unspecified()
                    || ip.is_multicast()
            }
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback()
                    || ip.is_unicast_link_local()
                    || ip.is_unspecified()
                    || ip.is_multicast()
            }
        };
        if local {
            bail!("local-network addresses are not allowed")
        }
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
        bail!("URL host is not allowed by the package manifest")
    }
    Ok(url)
}
fn http_request(
    policy: &NetworkPolicy,
    method: &str,
    url: &str,
    body: Option<String>,
) -> Result<(u16, String)> {
    let url = network_url_allowed(policy, url)?;
    let method = reqwest::Method::from_bytes(method.as_bytes()).context("invalid HTTP method")?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .request(method, url)
        .body(body.unwrap_or_default())
        .send()?;
    let status = response.status().as_u16();
    let mut response_body = String::new();
    response
        .take(1_048_576)
        .read_to_string(&mut response_body)?;
    Ok((status, response_body))
}
const QUASAR_CONFIG: &str = "/system/vendor/quasar/quasar.cfg";
const IPC_MAX_FRAME_SIZE: usize = 10 * 1024 * 1024;
const IPC_RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Extract only `<service>.port` from quasar.cfg; the rest of the file holds secrets.
fn quasar_service_port(config: &str, service: &str) -> Result<Option<u16>> {
    let value: serde_json::Value =
        serde_json::from_str(config).context("invalid quasar config JSON")?;
    Ok(value
        .get(service)
        .and_then(|entry| entry.get("port"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|port| u16::try_from(port).ok()))
}
const EV_SYN: u16 = 0;
const EV_FF: u16 = 0x15;
/// Longest single `poll` wait, so a stopped plugin is noticed within about this long.
const INPUT_POLL_SLICE_MS: u64 = 100;

fn validate_input_policy(m: &Manifest) -> Result<()> {
    if m.input.devices.is_empty() {
        return Ok(());
    }
    if !m.permissions.iter().any(|p| p == "input") {
        bail!("[input].devices requires the input permission")
    }
    if m.input.devices.len() > 16 {
        bail!("[input].devices lists more than 16 devices")
    }
    for name in &m.input.devices {
        if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            bail!("[input].devices contains an invalid device name")
        }
    }
    Ok(())
}
fn validate_ipc_policy(m: &Manifest) -> Result<()> {
    let has_raw_ipc = m
        .permissions
        .iter()
        .any(|permission| matches!(permission.as_str(), "ipc.read" | "ipc.control"));
    if m.ipc.services.is_empty() {
        if has_raw_ipc {
            bail!("ipc.read and ipc.control require a non-empty [ipc].services list")
        }
        return Ok(());
    }
    if !has_raw_ipc {
        bail!("[ipc].services requires ipc.read or ipc.control permission")
    }
    if m.ipc.services.len() > 16 {
        bail!("[ipc].services lists more than 16 services")
    }
    for service in &m.ipc.services {
        if service.is_empty()
            || service.len() > 64
            || !service
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            bail!("[ipc].services contains an invalid service name")
        }
    }
    Ok(())
}
fn normalize_command(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_lowercase().to_string()
            } else {
                " ".into()
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn validate_command_policy(m: &Manifest) -> Result<()> {
    let has_commands = m
        .permissions
        .iter()
        .any(|permission| permission == "commands");
    if m.commands.phrases.is_empty() {
        if has_commands {
            bail!("commands permission requires a non-empty [commands].phrases list")
        }
        return Ok(());
    }
    if !has_commands {
        bail!("[commands].phrases requires the commands permission")
    }
    if m.commands.phrases.len() > 8192 {
        bail!("[commands].phrases lists more than 8192 phrases")
    }
    for phrase in &m.commands.phrases {
        if phrase.len() > 128 || normalize_command(phrase).is_empty() {
            bail!("[commands].phrases contains an invalid phrase")
        }
    }
    Ok(())
}
fn command_matches(trigger: &str, transcript: &str) -> bool {
    let trigger = normalize_command(trigger);
    let transcript = normalize_command(transcript);
    transcript == trigger
        || transcript
            .strip_prefix("алиса ")
            .is_some_and(|phrase| phrase == trigger)
}
/// The device names a package declared; packages without a list get no input devices.
fn input_scope(m: &Manifest) -> Result<&[String]> {
    if m.input.devices.is_empty() {
        bail!("alice.input.next_event requires a non-empty [input].devices manifest list")
    }
    Ok(&m.input.devices)
}
/// Maps declared device names to `/dev/input/eventN` using `/proc/bus/input/devices` text.
/// Only exact name matches are returned; every declared name must exist.
fn resolve_input_devices(proc_devices: &str, wanted: &[String]) -> Result<Vec<(String, PathBuf)>> {
    if wanted.is_empty() {
        bail!("no input devices declared")
    }
    let available = list_input_devices(proc_devices);
    let mut resolved = vec![];
    for name in wanted {
        let before = resolved.len();
        for (candidate, path) in &available {
            if candidate == name {
                resolved.push((name.clone(), path.clone()));
            }
        }
        if resolved.len() == before {
            bail!("input device {name:?} not found")
        }
    }
    Ok(resolved)
}
fn list_input_devices(proc_devices: &str) -> Vec<(String, PathBuf)> {
    let mut available = vec![];
    for block in proc_devices.split("\n\n") {
        let mut name = None;
        let mut event = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("N: Name=") {
                name = Some(value.trim().trim_matches('"').to_string());
            } else if let Some(handlers) = line.strip_prefix("H: Handlers=") {
                event = handlers.split_whitespace().find(|handler| {
                    handler
                        .strip_prefix("event")
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                });
            }
        }
        if let (Some(name), Some(event)) = (name, event) {
            available.push((name, PathBuf::from("/dev/input").join(event)));
        }
    }
    available
}
#[derive(Debug, Clone, PartialEq)]
struct InputEvent {
    device: String,
    kind: u16,
    code: u16,
    value: i32,
    time_ms: i64,
}
/// Size of the kernel `struct input_event`: two `long` timestamp words plus type, code, value.
fn input_event_size(word: usize) -> usize {
    word * 2 + 8
}
fn read_input_word(bytes: &[u8], word: usize) -> i64 {
    match word {
        4 => i32::from_ne_bytes(bytes[..4].try_into().unwrap()) as i64,
        _ => i64::from_ne_bytes(bytes[..8].try_into().unwrap()),
    }
}
/// Decodes one native-endian `struct input_event` whose timestamp words are `word` bytes wide.
fn decode_input_event(device: &str, bytes: &[u8], word: usize) -> Result<InputEvent> {
    if !matches!(word, 4 | 8) || bytes.len() != input_event_size(word) {
        bail!("malformed input_event record")
    }
    let seconds = read_input_word(bytes, word);
    let micros = read_input_word(&bytes[word..], word);
    let tail = &bytes[word * 2..];
    Ok(InputEvent {
        device: device.to_string(),
        kind: u16::from_ne_bytes([tail[0], tail[1]]),
        code: u16::from_ne_bytes([tail[2], tail[3]]),
        value: i32::from_ne_bytes([tail[4], tail[5], tail[6], tail[7]]),
        time_ms: seconds * 1000 + micros / 1000,
    })
}
fn input_kind_name(kind: u16) -> &'static str {
    match kind {
        0 => "EV_SYN",
        1 => "EV_KEY",
        2 => "EV_REL",
        3 => "EV_ABS",
        4 => "EV_MSC",
        5 => "EV_SW",
        6 => "EV_LED",
        7 => "EV_SND",
        20 => "EV_REP",
        EV_FF => "EV_FF",
        _ => "unknown",
    }
}
fn input_key_value_name(kind: u16, value: i32) -> Option<&'static str> {
    if kind != 1 {
        return None;
    }
    Some(match value {
        0 => "release",
        1 => "press",
        2 => "repeat",
        _ => "unknown",
    })
}
struct InputSource {
    name: String,
    file: fs::File,
}
/// Read-only, non-grabbing evdev reader over the devices a package declared.
struct InputReader {
    sources: Vec<InputSource>,
    pending: std::collections::VecDeque<InputEvent>,
}
impl InputReader {
    fn open(devices: &[(String, PathBuf)]) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut sources = vec![];
        for (name, path) in devices {
            let file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .with_context(|| format!("open input device {name:?} at {}", path.display()))?;
            sources.push(InputSource {
                name: name.clone(),
                file,
            });
        }
        Ok(Self {
            sources,
            pending: Default::default(),
        })
    }
    /// Waits up to `timeout_ms` for the next non-`EV_SYN` event, checking `cancelled` between
    /// `INPUT_POLL_SLICE_MS` waits.
    fn next(&mut self, timeout_ms: u64, cancelled: &AtomicBool) -> Result<Option<InputEvent>> {
        use std::os::unix::io::AsRawFd;
        let word = std::mem::size_of::<libc::c_long>();
        let size = input_event_size(word);
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut buffer = vec![0u8; size * 32];
        loop {
            if cancelled.load(Ordering::Relaxed) {
                bail!("plugin stopped")
            }
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = (remaining.as_millis() as u64).min(INPUT_POLL_SLICE_MS);
            let mut fds: Vec<libc::pollfd> = self
                .sources
                .iter()
                .map(|source| libc::pollfd {
                    fd: source.file.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            let ready =
                unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait as i32) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error).context("poll input devices");
                }
            } else if ready > 0 {
                for (source, fd) in self.sources.iter_mut().zip(&fds) {
                    if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                        bail!("input device {:?} is no longer available", source.name)
                    }
                    if fd.revents & libc::POLLIN == 0 {
                        continue;
                    }
                    let length = match source.file.read(&mut buffer) {
                        Ok(length) => length,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("read input device {:?}", source.name))
                        }
                    };
                    for record in buffer[..length - length % size].chunks_exact(size) {
                        let event = decode_input_event(&source.name, record, word)?;
                        if event.kind != EV_SYN {
                            self.pending.push_back(event);
                        }
                    }
                }
            }
            if self.pending.is_empty() && std::time::Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }
}
fn ipc_endpoint(service: &str) -> Result<SocketAddr> {
    if service.is_empty()
        || !service
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        bail!("invalid Quasar IPC service name")
    }
    let variable = format!("LUNA_{}_ADDR", service.to_ascii_uppercase());
    let address = match std::env::var(&variable) {
        Ok(address) => address,
        Err(_) => {
            let config = std::fs::read_to_string(QUASAR_CONFIG)
                .with_context(|| format!("read {QUASAR_CONFIG} or set {variable}"))?;
            let port = quasar_service_port(&config, service)?
                .with_context(|| format!("{service}.port is not configured"))?;
            format!("127.0.0.1:{port}")
        }
    };
    let endpoint = address
        .parse::<SocketAddr>()
        .context("invalid IPC address")?;
    if !endpoint.ip().is_ipv4() || !endpoint.ip().is_loopback() {
        bail!("Quasar IPC endpoint must be an IPv4 loopback address")
    }
    Ok(endpoint)
}
fn protobuf_varint(data: &[u8], offset: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data.get(*offset).context("truncated protobuf field")?;
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("invalid protobuf varint")
}
fn protobuf_fields(data: &[u8]) -> Result<Vec<(u32, u8, Vec<u8>)>> {
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let key = protobuf_varint(data, &mut offset)?;
        let number = (key >> 3) as u32;
        let wire = (key & 7) as u8;
        let value = match wire {
            0 => protobuf_varint(data, &mut offset)?.to_le_bytes().to_vec(),
            1 => {
                let end = offset.checked_add(8).context("protobuf length overflow")?;
                let value = data
                    .get(offset..end)
                    .context("truncated protobuf field")?
                    .to_vec();
                offset = end;
                value
            }
            2 => {
                let size = usize::try_from(protobuf_varint(data, &mut offset)?)?;
                let end = offset
                    .checked_add(size)
                    .context("protobuf length overflow")?;
                let value = data
                    .get(offset..end)
                    .context("truncated protobuf field")?
                    .to_vec();
                offset = end;
                value
            }
            5 => {
                let end = offset.checked_add(4).context("protobuf length overflow")?;
                let value = data
                    .get(offset..end)
                    .context("truncated protobuf field")?
                    .to_vec();
                offset = end;
                value
            }
            _ => bail!("unsupported protobuf wire type"),
        };
        fields.push((number, wire, value));
    }
    Ok(fields)
}
fn nested_field(data: &[u8], field: u32) -> Result<Option<Vec<u8>>> {
    Ok(protobuf_fields(data)?
        .into_iter()
        .find_map(|(number, wire, value)| (number == field && wire == 2).then_some(value)))
}
fn text_field(data: &[u8], field: u32) -> Result<Option<String>> {
    nested_field(data, field)?
        .map(|value| String::from_utf8(value).context("invalid UTF-8 in IPC message"))
        .transpose()
}
fn varint_field(data: &[u8], field: u32) -> Result<Option<u64>> {
    Ok(protobuf_fields(data)?
        .into_iter()
        .find_map(|(number, wire, value)| {
            (number == field && wire == 0)
                .then(|| value.try_into().ok().map(u64::from_le_bytes))
                .flatten()
        }))
}
/// Unwrap the Midi envelope (field 252 = body, field 253 = QuasarMessage field
/// number) into a plain QuasarMessage; other frames are returned unchanged.
fn normalize_ipc_message(message: &[u8]) -> Result<Vec<u8>> {
    let fields = protobuf_fields(message)?;
    let kind = fields
        .iter()
        .find_map(|(number, wire, value)| (*number == 253 && *wire == 0).then_some(value));
    let body = fields
        .iter()
        .find_map(|(number, wire, value)| (*number == 252 && *wire == 2).then_some(value));
    let (Some(kind), Some(body)) = (kind, body) else {
        return Ok(message.to_vec());
    };
    let kind = u64::from_le_bytes(
        kind.as_slice()
            .try_into()
            .context("invalid envelope type")?,
    );
    let mut out = Vec::with_capacity(body.len() + 12);
    push_varint(&mut out, kind << 3 | 2);
    push_varint(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    Ok(out)
}
fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}
fn push_bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    push_varint(out, field << 3 | 2);
    push_varint(out, value.len() as u64);
    out.extend_from_slice(value);
}
fn alice_state_phase(state: &[u8]) -> &'static str {
    match varint_field(state, 1).ok().flatten() {
        Some(0) => "idle",
        Some(1) => "listening",
        Some(2) => "speaking",
        Some(3) => "busy",
        Some(5) => "shazam",
        Some(6) => "unknown",
        Some(_) => "unknown",
        None => "recognition",
    }
}
/// Replays the station's own voice-stop sequence through the supported local
/// remoting envelope. This is deliberately exposed only as a fixed command,
/// rather than letting a voice-command package build arbitrary aliced control
/// frames.
fn cancel_alice_dialog() -> Result<()> {
    // Stop the already active audio first. STOP_CONVERSATION/CANCEL_DIALOG
    // close the request pipeline, but do not reliably interrupt TTS that has
    // already entered the speaking state on Mini 1.
    send_alice_methods(&[14, 3, 1])
}

/// Ask aliced to run its recognizer without opening the normal Alice dialog.
/// `silent` suppresses the station's feedback, while `events` asks aliced to
/// publish recognition state through the existing AliceState IPC stream.
fn start_alice_recognition() -> Result<()> {
    send_alice_method_options(19, true, true, Some("luna"))
}

fn send_alice_methods(method_ids: &[u64]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let endpoint = ipc_endpoint("aliced")?;
    let mut stream = connect_ipc(endpoint, deadline)?.context("Quasar aliced is unavailable")?;

    // Try the audio stop immediately. On Mini 1 the first few seconds of a
    // TTS stream can be active while aliced is still emitting its handshake;
    // this best-effort write lets the station's remoting loop observe the
    // interruption without waiting for the handshake timeout.
    write_alice_method(&mut stream, 14)?;

    // Accept aliced's connection handshake before sending control methods.
    // Sending three methods over one initialized connection avoids the race
    // where the first method on a fresh socket is consumed by the handshake.
    let handshake_deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < handshake_deadline {
        if read_ipc_frame(&mut stream, handshake_deadline)?.is_none() {
            break;
        }
    }
    for &method_id in method_ids {
        write_alice_method(&mut stream, method_id)?;
    }
    Ok(())
}

fn send_alice_method_options(
    method_id: u64,
    silent: bool,
    events: bool,
    source_id: Option<&str>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let endpoint = ipc_endpoint("aliced")?;
    let mut stream = connect_ipc(endpoint, deadline)?.context("Quasar aliced is unavailable")?;
    let handshake_deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < handshake_deadline {
        if read_ipc_frame(&mut stream, handshake_deadline)?.is_none() {
            break;
        }
    }
    write_alice_method_with_options(&mut stream, method_id, silent, events, source_id)
}

fn write_alice_method(stream: &mut TcpStream, method_id: u64) -> Result<()> {
    write_alice_method_with_options(stream, method_id, false, false, None)
}

fn write_alice_method_with_options(
    stream: &mut TcpStream,
    method_id: u64,
    silent: bool,
    events: bool,
    source_id: Option<&str>,
) -> Result<()> {
    let mut method = Vec::new();
    push_varint(&mut method, 1 << 3);
    push_varint(&mut method, method_id);
    if events {
        push_varint(&mut method, 3 << 3);
        push_varint(&mut method, 1);
    }
    if let Some(source_id) = source_id {
        push_bytes(&mut method, 4, source_id.as_bytes());
    }
    if silent {
        push_varint(&mut method, 7 << 3);
        push_varint(&mut method, 1);
    }
    let mut remoting = Vec::new();
    push_bytes(&mut remoting, 7, b"AliceCapability");
    push_bytes(&mut remoting, 8, &method);
    let mut message = Vec::new();
    // Mini 1 accepts remoting commands directly as QuasarMessage field 210.
    // The 252/253 envelope is used by some event frames, but is ignored for
    // outgoing AliceCapability commands on this firmware.
    push_bytes(&mut message, 210, &remoting);
    write_ipc_frame(stream, &message)?;
    Ok(())
}
fn decode_ipc_event(service: &str, message: &[u8]) -> Result<Option<serde_json::Value>> {
    let message = normalize_ipc_message(message)?;
    let message = message.as_slice();
    match service {
        "aliced" => {
            // Local command spotter events use a separate remoting capability
            // instead of the regular voice-dialog state message. Mini 1 sends
            // CommandSpotterCapability with the recognized phrase in field 15.
            if let Some(remoting) = nested_field(message, 210)? {
                if text_field(&remoting, 7)?.as_deref() == Some("CommandSpotterCapability") {
                    if let Some(command) = nested_field(&remoting, 15)? {
                        if let Some(transcript) = text_field(&command, 3)? {
                            return Ok(Some(serde_json::json!({
                                "type": "speech",
                                "source": "command",
                                "phase": "recognition",
                                "request_id": "",
                                "transcript": transcript,
                                "final": true
                            })));
                        }
                    }
                }
            }
            let Some(state) = nested_field(message, 3)? else {
                return Ok(None);
            };
            let is_command = match nested_field(&state, 10)? {
                Some(source) => text_field(&source, 6)?.as_deref() == Some("command"),
                None => false,
            };
            if is_command {
                if let Some(request) = nested_field(&state, 15)? {
                    if varint_field(&request, 2)?.unwrap_or(0) != 0 {
                        if let Some(transcript) = text_field(&request, 1)? {
                            return Ok(Some(
                                serde_json::json!({
                                    "type": "speech",
                                    "source": "command",
                                    "phase": "recognition",
                                    "request_id": "",
                                    "transcript": transcript,
                                    "final": true
                                }),
                            ));
                        }
                    }
                }
            }
            let is_final = nested_field(&state, 15)?
                .map(|request| {
                    varint_field(&request, 2)
                        .ok()
                        .flatten()
                        .unwrap_or(0)
                        != 0
                })
                .unwrap_or(false);
            let phase = alice_state_phase(&state);
            let request_id = text_field(&state, 7)?.unwrap_or_default();
            Ok(text_field(&state, 2)?.map(|transcript| {
                serde_json::json!({
                    "type": "speech",
                    "source": "voice",
                    "phase": phase,
                    "request_id": request_id,
                    "transcript": transcript,
                    "final": is_final
                })
            }))
        }
        "alarmd" => {
            let Some(event) = nested_field(message, 215)? else {
                return Ok(None);
            };
            let Some(alarm) = nested_field(&event, 1)? else {
                return Ok(None);
            };
            Ok(Some(serde_json::json!({
                "type": "alarm_fired",
                "id": text_field(&alarm, 1)?,
                "alarm_type": varint_field(&alarm, 2)?,
                "start_timestamp_ms": varint_field(&alarm, 3)?,
                "duration_seconds": varint_field(&alarm, 4)?,
                "remind_message": text_field(&alarm, 5)?,
                "currently_playing": varint_field(&alarm, 6)?.map(|value| value != 0),
            })))
        }
        _ => bail!("unsupported IPC service"),
    }
}
fn ipc_frame_length(header: [u8; 4]) -> Result<usize> {
    let length = i32::from_le_bytes(header);
    if length < 0 {
        bail!("IPC frame has a negative length")
    }
    let length = length as usize;
    if length > IPC_MAX_FRAME_SIZE {
        bail!("IPC frame exceeds 10 MiB")
    }
    Ok(length)
}

fn validate_quasar_message(payload: &[u8]) -> Result<()> {
    if payload.len() > IPC_MAX_FRAME_SIZE {
        bail!("IPC frame exceeds 10 MiB")
    }
    protobuf_fields(payload).map(|_| ())
}

fn write_ipc_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    validate_quasar_message(payload)?;
    let length = i32::try_from(payload.len()).context("IPC frame is too large")?;
    stream.write_all(&length.to_le_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

fn read_ipc_frame(stream: &mut TcpStream, deadline: Instant) -> Result<Option<Vec<u8>>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(None);
    }
    stream.set_read_timeout(Some(remaining))?;
    let mut header = [0u8; 4];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    }
    let mut payload = vec![0; ipc_frame_length(header)?];
    stream.read_exact(&mut payload)?;
    validate_quasar_message(&payload)?;
    Ok(Some(payload))
}

fn connect_ipc(endpoint: SocketAddr, deadline: Instant) -> Result<Option<TcpStream>> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        match TcpStream::connect_timeout(&endpoint, remaining.min(IPC_RECONNECT_DELAY)) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                return Ok(Some(stream));
            }
            Err(_) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(None);
                }
                std::thread::sleep(remaining.min(IPC_RECONNECT_DELAY));
            }
        }
    }
}

fn ipc_send(service: &str, payload: &[u8]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let endpoint = ipc_endpoint(service)?;
    let mut stream =
        connect_ipc(endpoint, deadline)?.context("Quasar IPC service is unavailable")?;
    write_ipc_frame(&mut stream, payload)
}

fn file_player_remoting(method: u64, path: Option<&Path>) -> Vec<u8> {
    let mut method_message = Vec::new();
    push_varint(&mut method_message, 1 << 3);
    push_varint(&mut method_message, method);
    if let Some(path) = path {
        push_bytes(&mut method_message, 2, path.to_string_lossy().as_bytes());
        // FilePlayerCapabilityMethod.PlayParams: play once.
        let mut play_params = Vec::new();
        push_varint(&mut play_params, 2 << 3);
        push_varint(&mut play_params, 1);
        push_bytes(&mut method_message, 4, &play_params);
    }
    // The station's own FilePlayer client asks for completion events. Without
    // this bit older Mini firmware may accept the directive but immediately
    // tear down the player before audio reaches the hardware.
    push_varint(&mut method_message, 5 << 3);
    push_varint(&mut method_message, 1);
    let mut remoting = Vec::new();
    push_bytes(&mut remoting, 7, b"FilePlayerCapability");
    // Remoting.file_player_capability_method is field 12. Field 8 is the
    // Alice capability method and silently drops this message in mediad.
    push_bytes(&mut remoting, 12, &method_message);
    let mut message = Vec::new();
    push_bytes(&mut message, 210, &remoting); // QuasarMessage.remoting
    message
}

pub(crate) fn play_audio_file(path: &Path) -> Result<()> {
    crate::audio::validate_file(path)?;
    // FilePlayerCapability is owned by aliced. aliced forwards the actual
    // decoded audio to mediad; mediad is not the capability endpoint.
    let endpoint = ipc_endpoint("aliced")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = connect_ipc(endpoint, deadline)?.context("Quasar aliced is unavailable")?;
    // aliced uses the remoting handshake before accepting FilePlayer commands.
    // The handshake response is optional on older firmware, so do not require
    // it before sending the actual request.
    write_ipc_frame(&mut stream, &file_player_remoting(2, Some(path)))?;
    let handshake_deadline = Instant::now() + Duration::from_millis(250);
    let _ = read_ipc_frame(&mut stream, handshake_deadline)?;
    // FilePlayerCapabilityMethod.PLAY_SOUND_FILE is enum value 0.
    write_ipc_frame(&mut stream, &file_player_remoting(0, Some(path)))?;
    // aliced forwards the accepted directive asynchronously to mediad. Keep
    // the client channel alive for the same short handoff window used by the
    // station's own clients before dropping the socket.
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}

const API_TEST_AUDIO: &[u8] = include_bytes!("../assets/audio-test.wav");

fn write_api_test_audio(path: &Path) -> Result<()> {
    fs::write(path, API_TEST_AUDIO)
        .with_context(|| format!("write audio test file {}", path.display()))?;
    Ok(())
}

async fn audio_test(State(a): State<App>) -> Result<Json<serde_json::Value>, ApiError> {
    if !crate::audio::available() {
        return Err(ApiError(anyhow!("audio playback is not available")));
    }
    let path = a.root.join(".luna-api-test-tone.wav");
    let play_path = path.clone();
    task::spawn_blocking(move || {
        write_api_test_audio(&play_path)?;
        play_audio_file(&play_path)
    })
    .await
    .context("audio test task failed")??;
    Ok(Json(serde_json::json!({
        "played": true,
        "duration_ms": 4000,
        "format": "pcm_s16le",
        "source": "bundled test excerpt"
    })))
}

fn ipc_request(service: &str, payload: &[u8], timeout_ms: u64) -> Result<Option<Vec<u8>>> {
    if timeout_ms > 60_000 {
        bail!("IPC timeout is limited to 60 seconds")
    }
    validate_quasar_message(payload)?;
    let request_id =
        nested_field(payload, 1)?.context("IPC request must include field 1 request_id")?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let endpoint = ipc_endpoint(service)?;
    let mut stream =
        connect_ipc(endpoint, deadline)?.context("Quasar IPC service is unavailable")?;
    write_ipc_frame(&mut stream, payload)?;
    while let Some(response) = read_ipc_frame(&mut stream, deadline)? {
        if nested_field(&response, 1)?.as_deref() == Some(request_id.as_slice()) {
            return Ok(Some(response));
        }
    }
    Ok(None)
}

fn ipc_receive(service: &str, timeout_ms: u64) -> Result<Option<Vec<u8>>> {
    if timeout_ms > 60_000 {
        bail!("IPC timeout is limited to 60 seconds")
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let endpoint = ipc_endpoint(service)?;
    let Some(mut stream) = connect_ipc(endpoint, deadline)? else {
        return Ok(None);
    };
    read_ipc_frame(&mut stream, deadline)
}

fn ipc_service_allowed(services: &[String], service: &str) -> Result<()> {
    if !services.iter().any(|allowed| allowed == service) {
        bail!("Quasar IPC service is not allowed by [ipc].services")
    }
    Ok(())
}

fn ipc_next(service: &str, timeout_ms: u64) -> Result<Option<serde_json::Value>> {
    if timeout_ms > 60_000 {
        bail!("IPC timeout is limited to 60 seconds")
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let endpoint = ipc_endpoint(service)?;
    let Some(mut stream) = connect_ipc(endpoint, deadline)? else {
        return Ok(None);
    };

    // aliced sends capability/state handshake frames immediately after
    // accepting a client. The event stream is tied to this connection on
    // Mini firmware; reopening it every 100 ms only replays the handshake,
    // floods maind with resets, and can lose speech/command notifications.
    while let Some(payload) = read_ipc_frame(&mut stream, deadline)? {
        protobuf_fields(&payload)?;
        if let Some(event) = decode_ipc_event(service, &payload)? {
            return Ok(Some(event));
        }
    }
    Ok(None)
}

/// Wait for the next speech event from aliced. BetterYAM's command listener
/// accepts both `voice` and `command` events and matches the transcript itself.
fn wait_alice_speech(timeout_ms: u64, final_only: bool) -> Result<Option<serde_json::Value>> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        let event = ipc_next("aliced", remaining.as_millis().min(60_000) as u64)?;
        let Some(event) = event else {
            return Ok(None);
        };
        if !final_only || event.get("final").and_then(serde_json::Value::as_bool) == Some(true) {
            return Ok(Some(event));
        }
    }
}

fn clock_time(value: &str, brightness: u8) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || ![bytes[0], bytes[1], bytes[3], bytes[4]]
            .iter()
            .all(u8::is_ascii_digit)
    {
        bail!("clock time must have HH:MM format")
    }
    let digits: HashMap<u8, &[&str]> = HashMap::from([
        (b'0', &["A", "B", "C", "D", "E", "F"] as &[&str]),
        (b'1', &["B", "C"]),
        (b'2', &["A", "B", "G", "E", "D"]),
        (b'3', &["A", "B", "C", "D", "G"]),
        (b'4', &["F", "G", "B", "C"]),
        (b'5', &["A", "F", "G", "C", "D"]),
        (b'6', &["A", "F", "G", "E", "C", "D"]),
        (b'7', &["A", "B", "C"]),
        (b'8', &["A", "B", "C", "D", "E", "F", "G"]),
        (b'9', &["A", "B", "C", "D", "F", "G"]),
    ]);
    let mut segments = HashMap::new();
    let layout = clock_layout().context("segment clock is not available")?;
    // plum numbers its positions from 0 and has no PLUS/MINUS markers: its
    // colon is the upper and lower halves of the separator.
    let base = if matches!(layout, ClockLayout::Plum(_)) {
        0
    } else {
        1
    };
    for (index, digit) in [bytes[0], bytes[1], bytes[3], bytes[4]].iter().enumerate() {
        for part in digits.get(digit).context("invalid clock digit")?.iter() {
            segments.insert(format!("{part}{}", index + base), brightness);
        }
    }
    match layout {
        ClockLayout::Plum(_) => {
            segments.insert("DH".into(), brightness);
            segments.insert("DL".into(), brightness);
        }
        _ => {
            segments.insert("PLUS1".into(), brightness);
            segments.insert("PLUS2".into(), brightness);
        }
    }
    clock_write(&segments)
}
fn validate_id(s: &str) -> Result<()> {
    if s.is_empty()
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid plugin id")
    };
    Ok(())
}
fn validate_yam_id(s: &str) -> Result<()> {
    let mut parts = s.split('.');
    let Some(namespace) = parts.next() else {
        bail!("invalid YAM plugin id")
    };
    let Some(name) = parts.next() else {
        bail!("invalid YAM plugin id")
    };
    if parts.next().is_some()
        || namespace.is_empty()
        || name.is_empty()
        || namespace.len() > 64
        || name.len() > 64
        || !namespace
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("invalid YAM plugin id")
    }
    Ok(())
}
fn validate_id_or_yam(s: &str) -> Result<()> {
    if s.contains('.') {
        validate_yam_id(s)
    } else {
        validate_id(s)
    }
}
fn safe_relative(s: &str) -> Result<()> {
    let p = Path::new(s);
    if p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("unsafe path")
    };
    Ok(())
}
const SYSTEM_AMIXER: &str = "/system/workdir/bin/amixer";
const STATION_ALSA_LIBS: &str = "/bkupgrade/data/bundle/ap2/libs:/system/workdir/lib";

fn find_amixer() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| {
            std::env::split_paths(&paths)
                .map(|path| path.join("amixer"))
                .collect::<Vec<_>>()
        })
        .chain(std::iter::once(PathBuf::from(SYSTEM_AMIXER)))
        .find(|path| path.is_file())
}

fn amixer_output(args: &[String]) -> Result<String> {
    let path = find_amixer().context("amixer is not available on this station")?;
    let mut command = Command::new(&path);
    if path == Path::new(SYSTEM_AMIXER) {
        command
            .env("LD_LIBRARY_PATH", STATION_ALSA_LIBS)
            .env("ALSA_CONFIG_PATH", "/etc/asound.conf");
    }
    let output = command
        .args(args)
        .output()
        .with_context(|| format!("run {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "{} exited with {}: {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
fn configured_rgb_ring(settings: &LedSettings) -> bool {
    ["red", "green", "blue"]
        .iter()
        .all(|channel| led_brightness_path(settings, channel, 0).is_ok_and(|p| p.exists()))
}
/// Returns whether the named speaker API is usable on this device. This is a
/// capability probe, not a permission check: an unavailable API returns false,
/// while an API omitted from a package's manifest remains inaccessible.
fn lua_api_available(device: &DeviceProfile, name: &str) -> Result<bool> {
    let has_feature = |feature: &str| device.features.iter().any(|value| value == feature);
    let quasar_has = |service: &str| {
        fs::read_to_string(QUASAR_CONFIG)
            .ok()
            .and_then(|config| quasar_service_port(&config, service).ok().flatten())
            .is_some()
    };
    Ok(match name {
        "led" => device
            .features
            .iter()
            .any(|feature| feature.starts_with("led.")),
        "volume" => has_feature("audio.volume.alsa"),
        "audio" => has_feature("audio.play.mediad") || has_feature("audio.play.aplay"),
        "microphone" => has_feature("microphone.capture"),
        "bluetooth" | "bluetooth.status" => has_feature("bluetooth.bluez"),
        "bluetooth.power" => has_feature("bluetooth.power"),
        "bluetooth.scan" => has_feature("bluetooth.scan"),
        "bluetooth.pair" => has_feature("bluetooth.pair"),
        "bluetooth.connect" => has_feature("bluetooth.connect"),
        "bluetooth.disconnect" => has_feature("bluetooth.disconnect"),
        "bluetooth.remove" => has_feature("bluetooth.remove"),
        "display" => has_feature("display.backlight"),
        "sensors" => device
            .features
            .iter()
            .any(|feature| feature.starts_with("sensor.")),
        "sensors.control" => has_feature("sensor.ambient_light.iio"),
        "clock" => has_feature("display.clock.segment.v1"),
        "input" => has_feature("input.evdev"),
        "network" | "system" => true,
        "ipc.alarm" => quasar_has("alarmd"),
        "ipc.speech" | "commands" => quasar_has("aliced"),
        "ipc.read" | "ipc.control" => Path::new(QUASAR_CONFIG).is_file(),
        "exec" => bail!("exec has no is_available probe"),
        _ => bail!("unknown speaker API {name:?}"),
    })
}
fn attach_lua_availability(
    lua: &Lua,
    table: &mlua::Table,
    device: &DeviceProfile,
    name: &'static str,
) -> mlua::Result<()> {
    let available = lua_api_available(device, name).map_err(mlua::Error::external)?;
    table.set(
        "is_available",
        lua.create_function(move |_, ()| Ok(available))?,
    )
}
fn lua_callable_with_availability(
    lua: &Lua,
    function: mlua::Function,
    device: &DeviceProfile,
    capability: &'static str,
) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.set("__call", function)?;
    table.set_metatable(Some(metatable));
    attach_lua_availability(lua, &table, device, capability)?;
    Ok(table)
}

async fn run_plugin(app: App, id: String, event: String, cancelled: Arc<AtomicBool>) -> Result<()> {
    let device = app.device.lock().await.clone();
    let led_count = app.led.settings.lock().await.count;
    let (manifest, root, allowed) = {
        let p = app.plugins.lock().await;
        if !p.enabled.contains(&id) {
            bail!("plugin is disabled")
        };
        let (location, m) = p.location_and_manifest(&id)?;
        if let Some(reason) = compatibility_error(&m, &device) {
            bail!("plugin is incompatible: {reason}")
        };
        (
            m,
            location.root,
            app.settings.lock().await.allow_plugin_binaries,
        )
    };
    let led = app.led.clone();
    let microphone = app.microphone.clone();
    let gpio_inputs = app.gpio_inputs;
    let clock_backend = app.clock.clone();
    let custom_web = app.custom_web.clone();
    custom_web.lock().await.remove(&id);
    let room = Room::load(&app.root)?.room;
    let lua_settings = app.settings.lock().await.lua.clone();
    let enforce_user_lua_limits = id.starts_with("user_lua_");
    let lua_cancelled = cancelled.clone();
    let plugin_id = id.clone();
    let web_registry = custom_web.clone();
    let lua_result = task::spawn_blocking(move || -> Result<()> {
        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
            LuaOptions::default(),
        )?;
        if enforce_user_lua_limits {
            let deadline = Instant::now()
                + Duration::from_secs(lua_settings.max_runtime_seconds.clamp(1, 24 * 60 * 60));
            lua.set_hook(
                HookTriggers::new().every_nth_instruction(10_000),
                move |_lua, _debug| {
                    if lua_cancelled.load(Ordering::Relaxed) {
                        return Err(mlua::Error::RuntimeError("Lua script stopped".into()));
                    }
                    if !lua_settings.allow_infinite_loops && Instant::now() >= deadline {
                        return Err(mlua::Error::RuntimeError(
                            "Lua runtime limit exceeded".into(),
                        ));
                    }
                    Ok(VmState::Continue)
                },
            );
        }
        let globals = lua.globals();
        let api = lua.create_table()?;
        let permissions: HashSet<_> = manifest.permissions.iter().map(String::as_str).collect();
        if permissions.contains("web") {
            let register = lua.create_table()?;
            let web_id = plugin_id.clone();
            let web_root = root.clone();
            let web_registry = web_registry.clone();
            register.set(
                "customweb",
                lua.create_function(move |_, path: String| {
                    safe_relative(&path).map_err(mlua::Error::external)?;
                    let file = web_root.join(&path);
                    if !file.is_file() {
                        return Err(mlua::Error::external("custom web HTML file not found"));
                    }
                    let metadata = fs::metadata(&file).map_err(mlua::Error::external)?;
                    if metadata.len() > 4 * 1024 * 1024 {
                        return Err(mlua::Error::external(
                            "custom web HTML file exceeds 4 MiB",
                        ));
                    }
                    if !matches!(
                        file.extension().and_then(|extension| extension.to_str()),
                        Some("html") | Some("htm")
                    ) {
                        return Err(mlua::Error::external(
                            "custom web file must have .html or .htm extension",
                        ));
                    }
                    let page = CustomWebPage {
                        root: web_root.clone(),
                        file,
                    };
                    tokio::runtime::Handle::current()
                        .block_on(web_registry.lock())
                        .insert(web_id.clone(), page);
                    Ok(format!("/web/{web_id}"))
                })?,
            )?;
            globals.set("register", register)?;
        }
        let available_device = device.clone();
        api.set(
            "is_available",
            lua.create_function(move |_, name: String| {
                lua_api_available(&available_device, &name).map_err(mlua::Error::external)
            })?,
        )?;
        let model = device.model.clone();
        api.set(
            "model",
            lua.create_function(move |_, ()| Ok(model.clone()))?,
        )?;
        let cancelled_sleep = cancelled.clone();
        api.set(
            "sleep",
            lua.create_function(move |_, ms: u64| {
                if ms > 60_000 {
                    return Err(mlua::Error::external("sleep is limited to 60 seconds"));
                };
                let mut elapsed = 0;
                while elapsed < ms {
                    if cancelled_sleep.load(Ordering::Relaxed) {
                        return Err(mlua::Error::external("plugin stopped"));
                    }
                    let slice = (ms - elapsed).min(20);
                    std::thread::sleep(Duration::from_millis(slice));
                    elapsed += slice;
                }
                Ok(())
            })?,
        )?;
        if permissions.contains("led") {
            let led2 = led.clone();
            let cancelled_set = cancelled.clone();
            api.set(
                "set",
                lua.create_function(move |_, colors: mlua::Table| {
                    if cancelled_set.load(Ordering::Relaxed) {
                        return Err(mlua::Error::external("plugin stopped"));
                    }
                    let v = colors
                        .sequence_values::<String>()
                        .collect::<mlua::Result<Vec<_>>>()?;
                    tokio::runtime::Handle::current()
                        .block_on(led2.draw(v))
                        .map_err(mlua::Error::external)
                })?,
            )?;
            let led3 = led.clone();
            api.set(
                "off",
                lua.create_function(move |_, ()| {
                    tokio::runtime::Handle::current()
                        .block_on(led3.off())
                        .map_err(mlua::Error::external)
                })?,
            )?;
            let pattern_root = root.clone();
            let cancelled_play = cancelled.clone();
            let led_play = led.clone();
            api.set(
                "play",
                lua.create_function(move |_, name: String| {
                    safe_relative(&name).map_err(mlua::Error::external)?;
                    tokio::runtime::Handle::current()
                        .block_on(led_play.play_led(pattern_root.join(name), &cancelled_play))
                        .map_err(mlua::Error::external)
                })?,
            )?;
            let zones = lua.create_table()?;
            let logo_led = led.clone();
            let logo_zone = lua_callable_with_availability(
                &lua,
                lua.create_function(move |_, colors: mlua::Table| {
                    let colors = colors
                        .sequence_values::<String>()
                        .collect::<mlua::Result<Vec<_>>>()?;
                    tokio::runtime::Handle::current()
                        .block_on(logo_led.draw_logo(colors))
                        .map_err(mlua::Error::external)
                })?,
                &device,
                "led.logo",
            )?;
            zones.set("logo", logo_zone.clone())?;
            let led_api = lua.create_table()?;
            led_api.set("set", zones.clone())?;
            let off_zones = lua.create_table()?;
            let logo_led = led.clone();
            let logo_off_zone = lua_callable_with_availability(
                &lua,
                lua.create_function(move |_, ()| {
                    tokio::runtime::Handle::current()
                        .block_on(logo_led.off_logo())
                        .map_err(mlua::Error::external)
                })?,
                &device,
                "led.logo",
            )?;
            off_zones.set("logo", logo_off_zone.clone())?;
            let ring_set = led.clone();
            let ring_zone_led = ring_set.clone();
            let ring_zone = lua_callable_with_availability(
                &lua,
                lua.create_function(move |_, colors: mlua::Table| {
                    let colors = colors
                        .sequence_values::<String>()
                        .collect::<mlua::Result<Vec<_>>>()?;
                    tokio::runtime::Handle::current()
                        .block_on(ring_zone_led.draw_ring(colors))
                        .map_err(mlua::Error::external)
                })?,
                &device,
                "led.ring.rgb",
            )?;
            let set_meta = lua.create_table()?;
            set_meta.set(
                "__call",
                lua.create_function(move |_, colors: mlua::Table| {
                    let colors = colors
                        .sequence_values::<String>()
                        .collect::<mlua::Result<Vec<_>>>()?;
                    tokio::runtime::Handle::current()
                        .block_on(ring_set.draw(colors))
                        .map_err(mlua::Error::external)
                })?,
            )?;
            let set_api = lua.create_table()?;
            set_api.set_metatable(Some(set_meta));
            set_api.set("ring", ring_zone)?;
            set_api.set("logo", logo_zone)?;
            attach_lua_availability(&lua, &set_api, &device, "led")?;
            api.set("set", set_api.clone())?;

            let ring_off = led.clone();
            let off_meta = lua.create_table()?;
            off_meta.set(
                "__call",
                lua.create_function(move |_, ()| {
                    tokio::runtime::Handle::current()
                        .block_on(ring_off.off())
                        .map_err(mlua::Error::external)
                })?,
            )?;
            let off_api = lua.create_table()?;
            off_api.set_metatable(Some(off_meta));
            let ring_off = led.clone();
            let ring_off_zone = lua_callable_with_availability(
                &lua,
                lua.create_function(move |_, ()| {
                    tokio::runtime::Handle::current()
                        .block_on(ring_off.off_ring())
                        .map_err(mlua::Error::external)
                })?,
                &device,
                "led.ring.rgb",
            )?;
            off_api.set("ring", ring_off_zone)?;
            off_api.set("logo", logo_off_zone)?;
            attach_lua_availability(&lua, &off_api, &device, "led")?;
            api.set("off", off_api.clone())?;
            led_api.set("off", off_zones)?;
            led_api.set("set", set_api)?;
            led_api.set("off", off_api)?;
            api.set("led", led_api)?;
        }
        if permissions.contains("volume") {
            let volume = lua.create_table()?;
            volume.set(
                "set",
                lua.create_function(|_, percent: u8| {
                    if percent > 100 {
                        return Err(mlua::Error::external("volume must be 0..100"));
                    };
                    set_volume_backend(percent, false).map_err(mlua::Error::external)
                })?,
            )?;
            volume.set(
                "get",
                lua.create_function(|_, ()| {
                    amixer_output(&["get".into(), "Master".into()]).map_err(mlua::Error::external)
                })?,
            )?;
            api.set("volume", volume)?;
        }
        if permissions.contains("audio") {
            let audio = lua.create_table()?;
            let audio_root = root.clone();
            audio.set(
                "play",
                lua.create_function(move |_, file: String| {
                    safe_relative(&file).map_err(mlua::Error::external)?;
                    let path = audio_root.join(file);
                    if !path.is_file() {
                        return Err(mlua::Error::external("audio file not found"));
                    };
                    crate::gateway::play_audio_file(&path).map_err(mlua::Error::external)
                })?,
            )?;
            api.set("audio", audio)?;
        }
        if permissions.contains("microphone") {
            let backend = microphone
                .clone()
                .context("microphone capture is not available")?;
            let list_backend = backend.clone();
            let microphone_api = lua.create_table()?;
            microphone_api.set(
                "list",
                lua.create_function(move |lua, ()| {
                    let devices = list_backend.devices().map_err(mlua::Error::external)?;
                    let result = lua.create_table()?;
                    for (index, device) in devices.into_iter().enumerate() {
                        result.set(index + 1, lua.to_value(&device)?)?;
                    }
                    Ok(result)
                })?,
            )?;
            let record_backend = backend.clone();
            let cancelled_record = cancelled.clone();
            microphone_api.set(
                "record",
                lua.create_function(move |lua, (seconds, device): (u8, Option<String>)| {
                    if !(1..=30).contains(&seconds) {
                        return Err(mlua::Error::external(
                            "microphone capture duration must be 1..=30 seconds",
                        ));
                    }
                    let wav = record_backend
                        .record(device.as_deref(), seconds, &cancelled_record)
                        .map_err(mlua::Error::external)?;
                    Ok(lua.create_string(wav)?)
                })?,
            )?;
            let mute_input = gpio_inputs
                .iter()
                .find(|input| input.name == "mute")
                .copied();
            let unmute_input = gpio_inputs
                .iter()
                .find(|input| input.name == "unmute")
                .copied();
            microphone_api.set(
                "waitformute",
                lua.create_function(move |_, timeout_ms: Option<u64>| {
                    let input = mute_input
                        .context("microphone mute input is not available")
                        .map_err(mlua::Error::external)?;
                    wait_gpio_transition(input, timeout_ms.unwrap_or(60_000))
                        .map_err(mlua::Error::external)
                })?,
            )?;
            microphone_api.set(
                "waitforunmute",
                lua.create_function(move |_, timeout_ms: Option<u64>| {
                    let input = unmute_input
                        .context("microphone unmute input is not available")
                        .map_err(mlua::Error::external)?;
                    wait_gpio_transition(input, timeout_ms.unwrap_or(60_000))
                        .map_err(mlua::Error::external)
                })?,
            )?;
            api.set("microphone", microphone_api)?;
        }
        if permissions.contains("bluetooth") {
            let bt = lua.create_table()?;
            bt.set(
                "status",
                lua.create_function(|lua, ()| {
                    lua.to_value(&bluetooth::status().map_err(mlua::Error::external)?)
                })?,
            )?;
            bt.set(
                "power",
                lua.create_function(|_, on: bool| {
                    bluetooth::power(on).map_err(mlua::Error::external)
                })?,
            )?;
            bt.set(
                "scan",
                lua.create_function(|_, on: bool| {
                    bluetooth::scan(on).map_err(mlua::Error::external)
                })?,
            )?;
            for action in ["pair", "connect", "disconnect", "remove"] {
                let a = action.to_string();
                bt.set(
                    action,
                    lua.create_function(move |_, address: String| {
                        bluetooth::device_action(&a, &address).map_err(mlua::Error::external)
                    })?,
                )?;
            }
            api.set("bluetooth", bt)?;
        }
        if permissions.contains("display") {
            let display = lua.create_table()?;
            display.set(
                "backlights",
                lua.create_function(|lua, ()| {
                    let t = lua.create_table()?;
                    if let Ok(entries) = fs::read_dir("/sys/class/backlight") {
                        for (i, e) in entries.flatten().enumerate() {
                            t.set(i + 1, e.file_name().to_string_lossy().to_string())?
                        }
                    }
                    Ok(t)
                })?,
            )?;
            display.set(
                "set_backlight",
                lua.create_function(|_, (name, value): (String, u32)| {
                    validate_id(&name).map_err(mlua::Error::external)?;
                    let base = Path::new("/sys/class/backlight").join(&name);
                    let max: u32 = fs::read_to_string(base.join("max_brightness"))
                        .map_err(mlua::Error::external)?
                        .trim()
                        .parse()
                        .map_err(mlua::Error::external)?;
                    if value > max {
                        return Err(mlua::Error::external("brightness exceeds max_brightness"));
                    };
                    fs::write(base.join("brightness"), value.to_string())
                        .map_err(mlua::Error::external)
                })?,
            )?;
            api.set("display", display)?;
        }
        if permissions.contains("sensors") {
            let sensors = lua.create_table()?;
            sensors.set(
                "list",
                lua.create_function(|lua, ()| {
                    let result = lua.create_table()?;
                    for (index, sensor) in discover_sensors().iter().enumerate() {
                        let item = lua.create_table()?;
                        item.set("id", sensor.id.as_str())?;
                        item.set("kind", sensor.kind.as_str())?;
                        item.set("unit", sensor.unit.as_str())?;
                        result.set(index + 1, item)?;
                    }
                    Ok(result)
                })?,
            )?;
            sensors.set(
                "read",
                lua.create_function(|lua, id: String| {
                    let sensor = discover_sensors()
                        .into_iter()
                        .find(|sensor| sensor.id == id)
                        .context("sensor not found")
                        .map_err(mlua::Error::external)?;
                    let result = lua.create_table()?;
                    result.set("id", sensor.id.as_str())?;
                    result.set("kind", sensor.kind.as_str())?;
                    result.set("unit", sensor.unit.as_str())?;
                    result.set(
                        "value",
                        sensor_value(&sensor).map_err(mlua::Error::external)?,
                    )?;
                    Ok(result)
                })?,
            )?;
            if permissions.contains("sensors.control") {
                sensors.set(
                    "configure_ambient_light",
                    lua.create_function(|_, (name, gain, integration_ms): (String, u8, u16)| {
                        if ![1, 3, 6, 9, 18].contains(&gain) {
                            return Err(mlua::Error::external("gain must be 1, 3, 6, 9 or 18"));
                        }
                        if ![25, 50, 100, 200, 400].contains(&integration_ms) {
                            return Err(mlua::Error::external(
                                "integration time must be 25, 50, 100, 200 or 400 ms",
                            ));
                        }
                        let path = configured_light_sensor(&name).map_err(mlua::Error::external)?;
                        fs::write(path.join("in_intensity_hardwaregain"), gain.to_string())
                            .map_err(mlua::Error::external)?;
                        fs::write(
                            path.join("in_intensity_integration_time"),
                            format!("0.{integration_ms:03}"),
                        )
                        .map_err(mlua::Error::external)
                    })?,
                )?;
            }
            api.set("sensors", sensors)?;
        }
        if permissions.contains("clock") {
            let clock = lua.create_table()?;
            let station_clock = clock_backend.clone();
            clock.set(
                "available",
                lua.create_function(move |_, ()| Ok(station_clock.is_some() || clock_available()))?,
            )?;
            let station_clock = clock_backend.clone();
            clock.set(
                "segments",
                lua.create_function(move |_, levels: mlua::Table| {
                    let mut frame = HashMap::new();
                    for pair in levels.pairs::<String, u8>() {
                        let (segment, value) = pair?;
                        frame.insert(segment, value);
                    }
                    if let Some(clock) = &station_clock {
                        clock
                            .apply(ClockCommand::Segments(frame))
                            .map_err(mlua::Error::external)?;
                        return Ok(());
                    }
                    validate_clock_segments(&frame).map_err(mlua::Error::external)?;
                    clock_write(&frame).map_err(mlua::Error::external)
                })?,
            )?;
            let station_clock = clock_backend.clone();
            clock.set(
                "time",
                lua.create_function(move |_, (time, brightness): (String, Option<u8>)| {
                    if let Some(clock) = &station_clock {
                        return clock
                            .apply(ClockCommand::Time {
                                text: time,
                                brightness: brightness.unwrap_or(170),
                            })
                            .map_err(mlua::Error::external);
                    }
                    clock_time(&time, brightness.unwrap_or(170)).map_err(mlua::Error::external)
                })?,
            )?;
            let station_clock = clock_backend.clone();
            clock.set(
                "off",
                lua.create_function(move |_, ()| {
                    if let Some(clock) = &station_clock {
                        return clock
                            .apply(ClockCommand::Off)
                            .map_err(mlua::Error::external);
                    }
                    clock_release().map_err(mlua::Error::external)
                })?,
            )?;
            api.set("clock", clock)?;
        }
        if permissions.contains("input") {
            let input = lua.create_table()?;
            input.set(
                "devices",
                lua.create_function(|_, ()| {
                    fs::read_to_string("/proc/bus/input/devices").map_err(mlua::Error::external)
                })?,
            )?;
            let input_manifest = manifest.clone();
            let cancelled_input = cancelled.clone();
            let mut reader: Option<InputReader> = None;
            input.set(
                "next_event",
                lua.create_function_mut(move |lua, timeout_ms: u64| {
                    if timeout_ms > 60_000 {
                        return Err(mlua::Error::external(
                            "input timeout is limited to 60 seconds",
                        ));
                    }
                    if reader.is_none() {
                        let opened = input_scope(&input_manifest)
                            .and_then(|names| {
                                let listing = fs::read_to_string("/proc/bus/input/devices")
                                    .context("read /proc/bus/input/devices")?;
                                resolve_input_devices(&listing, names)
                            })
                            .and_then(|devices| InputReader::open(&devices))
                            .map_err(mlua::Error::external)?;
                        reader = Some(opened);
                    }
                    let event = reader
                        .as_mut()
                        .expect("reader opened above")
                        .next(timeout_ms, &cancelled_input)
                        .map_err(mlua::Error::external)?;
                    match event {
                        Some(event) => {
                            let table = lua.create_table()?;
                            table.set("device", event.device)?;
                            table.set("type", event.kind)?;
                            table.set("code", event.code)?;
                            table.set("value", event.value)?;
                            table.set("time_ms", event.time_ms)?;
                            Ok(mlua::Value::Table(table))
                        }
                        None => Ok(mlua::Value::Nil),
                    }
                })?,
            )?;
            api.set("input", input)?;
        }
        if permissions.contains("network") {
            if manifest.network.allow.is_empty() {
                bail!("network permission requires a non-empty [network].allow manifest list")
            }
            let http = lua.create_table()?;
            let get_policy = manifest.network.clone();
            http.set(
                "get",
                lua.create_function(move |lua, url: String| {
                    let (status, body) = http_request(&get_policy, "GET", &url, None)
                        .map_err(mlua::Error::external)?;
                    let result = lua.create_table()?;
                    result.set("status", status)?;
                    result.set("body", body)?;
                    Ok(result)
                })?,
            )?;
            let post_policy = manifest.network.clone();
            http.set(
                "post",
                lua.create_function(move |lua, (url, body): (String, String)| {
                    if body.len() > 1_048_576 {
                        return Err(mlua::Error::external("request body exceeds 1 MiB"));
                    }
                    let (status, body) = http_request(&post_policy, "POST", &url, Some(body))
                        .map_err(mlua::Error::external)?;
                    let result = lua.create_table()?;
                    result.set("status", status)?;
                    result.set("body", body)?;
                    Ok(result)
                })?,
            )?;
            api.set("http", http)?;
        }
        if permissions.contains("ipc.alarm")
            || permissions.contains("ipc.speech")
            || permissions.contains("ipc.read")
            || permissions.contains("ipc.control")
        {
            let ipc = lua.create_table()?;
            if permissions.contains("ipc.alarm") {
                ipc.set(
                    "next_alarm",
                    lua.create_function(|lua, timeout_ms: u64| {
                        match ipc_next("alarmd", timeout_ms).map_err(mlua::Error::external)? {
                            Some(event) => lua.to_value(&event),
                            None => Ok(mlua::Value::Nil),
                        }
                    })?,
                )?;
            }
            if permissions.contains("ipc.speech") {
                ipc.set(
                    "next_speech",
                    lua.create_function(|lua, timeout_ms: u64| {
                        match ipc_next("aliced", timeout_ms).map_err(mlua::Error::external)? {
                            Some(event) => lua.to_value(&event),
                            None => Ok(mlua::Value::Nil),
                        }
                    })?,
                )?;
            }
            if permissions.contains("ipc.read") || permissions.contains("ipc.control") {
                if manifest.ipc.services.is_empty() {
                    bail!("ipc.read and ipc.control require a non-empty [ipc].services list")
                }
            }
            if permissions.contains("ipc.read") {
                let read_services = manifest.ipc.services.clone();
                ipc.set(
                    "next",
                    lua.create_function(move |lua, (service, timeout_ms): (String, u64)| {
                        ipc_service_allowed(&read_services, &service)
                            .and_then(|_| ipc_receive(&service, timeout_ms))
                            .map_err(mlua::Error::external)?
                            .map(|payload| lua.create_string(&payload))
                            .transpose()?
                            .map(mlua::Value::String)
                            .map_or(Ok(mlua::Value::Nil), Ok)
                    })?,
                )?;
            }
            if permissions.contains("ipc.control") {
                let control_services = manifest.ipc.services.clone();
                ipc.set(
                    "send",
                    lua.create_function(move |_, (service, payload): (String, mlua::String)| {
                        ipc_service_allowed(&control_services, &service)
                            .and_then(|_| ipc_send(&service, payload.as_bytes().as_ref()))
                            .map_err(mlua::Error::external)
                    })?,
                )?;
            }
            if permissions.contains("ipc.read") && permissions.contains("ipc.control") {
                let request_services = manifest.ipc.services.clone();
                ipc.set(
                    "request",
                    lua.create_function(
                        move |lua, (service, payload, timeout_ms): (String, mlua::String, u64)| {
                            ipc_service_allowed(&request_services, &service)
                                .and_then(|_| {
                                    ipc_request(&service, payload.as_bytes().as_ref(), timeout_ms)
                                })
                                .map_err(mlua::Error::external)?
                                .map(|response| lua.create_string(&response))
                                .transpose()?
                                .map(mlua::Value::String)
                                .map_or(Ok(mlua::Value::Nil), Ok)
                        },
                    )?,
                )?;
            }
            api.set("ipc", ipc)?;
        }
        if permissions.contains("commands") {
            let command = lua.create_table()?;
            command.set(
                "cancel",
                lua.create_function(|_, ()| cancel_alice_dialog().map_err(mlua::Error::external))?,
            )?;
            let allowed_phrases = manifest.commands.phrases.clone();
            let command_cancelled = cancelled.clone();
            command.set(
                "listen",
                lua.create_function(
                    move |lua, (phrase, handler): (String, mlua::Function)| -> mlua::Result<()> {
                        if !allowed_phrases
                            .iter()
                            .any(|allowed| normalize_command(allowed) == normalize_command(&phrase))
                        {
                            return Err(mlua::Error::external(
                                "voice command is not declared in [commands].phrases",
                            ));
                        }
                        loop {
                            if command_cancelled.load(Ordering::Relaxed) {
                                return Err(mlua::Error::external("plugin stopped"));
                            }
                            let Some(event) =
                                ipc_next("aliced", 1_000).map_err(mlua::Error::external)?
                            else {
                                continue;
                            };
                            let Some(transcript) = event["transcript"].as_str() else {
                                continue;
                            };
                            if !command_matches(&phrase, transcript) {
                                continue;
                            }
                            let command_event = lua.create_table()?;
                            command_event.set("transcript", transcript)?;
                            command_event.set("command", normalize_command(&phrase))?;
                            command_event
                                .set("source", event["source"].as_str().unwrap_or("voice"))?;
                            handler.call::<()>(command_event)?;
                        }
                    },
                )?,
            )?;
            api.set("command", command)?;
        }
        if permissions.contains("system") {
            let system = lua.create_table()?;
            system.set(
                "hostname",
                lua.create_function(|_, ()| {
                    fs::read_to_string("/etc/hostname")
                        .map(|x| x.trim().to_string())
                        .map_err(mlua::Error::external)
                })?,
            )?;
            system.set(
                "uptime",
                lua.create_function(|_, ()| {
                    fs::read_to_string("/proc/uptime").map_err(mlua::Error::external)
                })?,
            )?;
            system.set(
                "meminfo",
                lua.create_function(|_, ()| {
                    fs::read_to_string("/proc/meminfo").map_err(mlua::Error::external)
                })?,
            )?;
            api.set("system", system)?;
        }
        if permissions.contains("exec") {
            let plugin_root = root.clone();
            api.set(
                "exec",
                lua.create_function(move |_, (program, args): (String, Vec<String>)| {
                    if !allowed {
                        return Err(mlua::Error::external(
                            "binary execution disabled by administrator",
                        ));
                    };
                    safe_relative(&program).map_err(mlua::Error::external)?;
                    let output = Command::new(plugin_root.join(program))
                        .args(args)
                        .current_dir(&plugin_root)
                        .output()
                        .map_err(mlua::Error::external)?;
                    Ok(String::from_utf8_lossy(&output.stdout).to_string())
                })?,
            )?;
        }
        for (name, capability) in [
            ("led", "led"),
            ("volume", "volume"),
            ("audio", "audio"),
            ("microphone", "microphone"),
            ("bluetooth", "bluetooth"),
            ("display", "display"),
            ("sensors", "sensors"),
            ("clock", "clock"),
            ("input", "input"),
            ("http", "network"),
            ("ipc", "ipc.read"),
            ("command", "commands"),
            ("system", "system"),
        ] {
            if let Some(table) = api.get::<Option<mlua::Table>>(name)? {
                attach_lua_availability(&lua, &table, &device, capability)?;
            }
        }
        globals.set("alice", api)?;
        globals.set("event", event)?;
        let lua_device = lua.create_table()?;
        lua_device.set("model", device.model)?;
        lua_device.set("architecture", device.architecture)?;
        lua_device.set("led_count", led_count)?;
        lua_device.set("has_ring", device.led.ring_count > 0)?;
        lua_device.set("ring_count", device.led.ring_count)?;
        lua_device.set("has_logo", device.led.has_logo)?;
        lua_device.set("logo_count", device.led.logo_count)?;
        lua_device.set(
            "has_clock",
            device
                .features
                .iter()
                .any(|feature| feature == "display.clock.segment.v1"),
        )?;
        lua_device.set(
            "has_mute_button",
            gpio_inputs.iter().any(|input| input.name == "mute"),
        )?;
        lua_device.set(
            "has_microphone",
            device
                .features
                .iter()
                .any(|feature| feature == "microphone.capture"),
        )?;
        lua_device.set("room", room)?;
        lua_device.set("HasRing", device.led.ring_count > 0)?;
        lua_device.set("RingCount", device.led.ring_count)?;
        lua_device.set("HasLogo", device.led.has_logo)?;
        lua_device.set("LogoCount", device.led.logo_count)?;
        lua_device.set(
            "HasClock",
            device
                .features
                .iter()
                .any(|feature| feature == "display.clock.segment.v1"),
        )?;
        lua_device.set(
            "HasMuteButton",
            gpio_inputs.iter().any(|input| input.name == "mute"),
        )?;
        lua_device.set(
            "HasMicrophone",
            device
                .features
                .iter()
                .any(|feature| feature == "microphone.capture"),
        )?;
        let lua_features = lua.create_table()?;
        for (i, feature) in device.features.iter().enumerate() {
            lua_features.set(i + 1, feature.as_str())?;
        }
        lua_device.set("features", lua_features)?;
        globals.set("device", lua_device)?;
        let launch_path = manifest.launch.path(&manifest).to_string();
        let source = fs::read_to_string(root.join(&launch_path))?;
        lua.load(&source).set_name(&launch_path).exec()?;
        Ok(())
    })
    .await
    .context("Lua runtime task failed")?;
    custom_web.lock().await.remove(&id);
    lua_result?;
    Ok(())
}

async fn run_binary(
    app: App,
    id: String,
    invocation_args: Vec<String>,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let device = app.device.lock().await.clone();
    let (location, local_manifest, state_root, yam_root, yam_run) = {
        let plugins = app.plugins.lock().await;
        if !plugins.enabled.contains(&id) {
            bail!("package is disabled")
        }
        let location = plugins.location(&id)?;
        let local_manifest = if location.source == PluginSource::Local {
            Some(plugins.manifest(&id)?)
        } else {
            None
        };
        (location, local_manifest, plugins.state_root(&id), plugins.yam_root.clone(), plugins.yam_run.clone())
    };
    let requested_id = id.clone();
    let prepared = if let Some(manifest) = local_manifest {
        yam_binary::Prepared { manifest, root: location.root, lease: None }
    } else {
        task::spawn_blocking(move || {
            yam_binary::prepare(&requested_id, location, yam_root.as_deref(), yam_run.as_deref())
        }).await??
    };
    let yam_binary::Prepared { manifest, root, lease } = prepared;
    if cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    if !app.plugins.lock().await.enabled.contains(&id) {
        bail!("package is disabled")
    }
    if !app.settings.lock().await.allow_plugin_binaries {
        bail!("binary execution disabled by administrator")
    }
    if let Some(reason) = compatibility_error(&manifest, &device) {
        bail!("package is incompatible: {reason}")
    }
    if !manifest.permissions.iter().any(|permission| permission == "exec") {
        bail!("binary packages require the exec permission")
    }
    let token = Uuid::new_v4().to_string();
    let invocation_token = token.clone();
    app.runtime_tokens
        .lock()
        .await
        .insert(id.clone(), token.clone());
    let api_url = format!("{}/api/runtime/{id}", app.runtime_api_url);
    let task_id = id.clone();
    let result = task::spawn_blocking(move || -> Result<()> {
        // Keep the selected package version alive through process-group cleanup.
        let _yam_lease = lease;
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let executable = manifest.launch.path(&manifest).to_string();
        safe_relative(&executable)?;
        fs::create_dir_all(&state_root)?;
        let log = Arc::new(std::sync::Mutex::new(RotatingLog::open(
            state_root.join("runtime.log"),
        )?));
        let (interpreter, program) = if _yam_lease.is_some() {
            (manifest.launch.interpreter.as_ref().map(|_| "sh"), root.join(&executable))
        } else {
            launch_program(&manifest, &root, &executable)
        };
        let library_path = if _yam_lease.is_some() {
            yam_binary::library_path(&manifest, &root)
        } else {
            binary_library_path(&manifest, &root)
        };
        let mut command = match interpreter {
            Some(interpreter) => {
                let mut command = Command::new(interpreter);
                command.arg(program);
                command
            }
            None => Command::new(program),
        };
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .args(&manifest.launch.args)
            .args(invocation_args)
            .current_dir(&root)
            .env("LD_LIBRARY_PATH", library_path)
            .env("LUNA_API_URL", api_url)
            .env("LUNA_API_TOKEN", token)
            .env("LUNA_PLUGIN_ID", task_id)
            .env("LUNA_PLUGIN_STATE", &state_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pumps = [
            pump_log(child.stdout.take(), log.clone()),
            pump_log(child.stderr.take(), log),
        ];
        loop {
            if cancelled.load(Ordering::Relaxed) {
                stop_process_group(&mut child);
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                stop_process_group(&mut child);
                for _ in 0..20 {
                    if pumps.iter().all(std::thread::JoinHandle::is_finished) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                if status.success() {
                    return Ok(());
                }
                bail!("binary exited with {status}")
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })
    .await?;
    let revoke = {
        let mut tokens = app.runtime_tokens.lock().await;
        if tokens.get(&id) == Some(&invocation_token) {
            tokens.remove(&id);
            true
        } else {
            false
        }
    };
    if revoke {
        app.speech_readers.lock().await.remove(&id);
        app.speech_claims.lock().await.remove(&id);
    }
    result
}

// Packages converted before `launch.interpreter` / `launch.library_paths`
// existed carry an apkg `<id>/<id>-run` script and `<prefix>/lib` next to
// `<prefix>/bin`; both are only guessed when the manifest is silent.
fn launch_program(
    manifest: &Manifest,
    root: &Path,
    executable: &str,
) -> (Option<&'static str>, PathBuf) {
    if manifest.launch.interpreter.as_deref() == Some("sh") {
        return (Some("sh"), root.join(executable));
    }
    let legacy_runner = root.join(&manifest.id).join(format!("{}-run", manifest.id));
    if manifest.launch.interpreter.is_none() && legacy_runner.is_file() {
        return (Some("sh"), legacy_runner);
    }
    (None, root.join(executable))
}
struct RotatingLog {
    path: PathBuf,
    file: fs::File,
    size: u64,
}
impl RotatingLog {
    const LIMIT: u64 = 1_048_576;
    fn open(path: PathBuf) -> std::io::Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let size = file.metadata()?.len();
        Ok(Self { path, file, size })
    }
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        if self.size > Self::LIMIT {
            fs::rename(&self.path, self.path.with_extension("log.1"))?;
            *self = Self::open(self.path.clone())?;
        }
        self.file.write_all(data)?;
        self.size += data.len() as u64;
        Ok(())
    }
}
fn pump_log(
    source: Option<impl Read + Send + 'static>,
    log: Arc<std::sync::Mutex<RotatingLog>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(mut source) = source else {
            return;
        };
        let mut buffer = [0; 8192];
        while let Ok(read @ 1..) = source.read(&mut buffer) {
            let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
            if log.write(&buffer[..read]).is_err() {
                return;
            }
        }
    })
}
fn binary_library_path(manifest: &Manifest, root: &Path) -> String {
    let bundled: Vec<PathBuf> = if manifest.launch.library_paths.is_empty() {
        Path::new(manifest.launch.path(manifest))
            .parent()
            .and_then(Path::parent)
            .map(|directory| root.join(directory).join("lib"))
            .filter(|directory| directory.is_dir())
            .into_iter()
            .collect()
    } else {
        manifest
            .launch
            .library_paths
            .iter()
            .map(|directory| root.join(directory))
            .collect()
    };
    let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    bundled
        .iter()
        .map(|directory| directory.display().to_string())
        .chain((!existing.is_empty()).then_some(existing))
        .collect::<Vec<_>>()
        .join(":")
}

async fn run_package(
    app: App,
    id: String,
    invocation_args: Vec<String>,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let kind = app.plugins.lock().await.manifest(&id)?.launch.kind;
    match kind {
        LaunchKind::Lua => run_plugin(app, id, "manual".into(), cancelled).await,
        LaunchKind::Binary => run_binary(app, id, invocation_args, cancelled).await,
    }
}

async fn stop_running_package(app: &App, id: &str) -> Result<()> {
    let running = app.plugins.lock().await.running.get(id).cloned();
    if let Some(running) = running {
        running.stop(Duration::from_secs(20)).await?;
    }
    Ok(())
}

async fn start_package(app: App, id: String, invocation_args: Vec<String>) -> Result<()> {
    let lifecycle = app.plugins.lock().await.lifecycle.clone();
    let _operation = lifecycle.lock().await;
    start_package_locked(app, id, invocation_args).await
}

// Lifecycle callers hold the operation lock, never the plugins lock while waiting.
async fn start_package_locked(app: App, id: String, invocation_args: Vec<String>) -> Result<()> {
    {
        let plugins = app.plugins.lock().await;
        if plugins.shutting_down {
            bail!("server is shutting down")
        }
        if !plugins.enabled.contains(&id) {
            bail!("package is disabled")
        }
        let manifest = plugins.manifest(&id)?;
        if manifest.launch.kind == LaunchKind::Binary
            && !app.settings.lock().await.allow_plugin_binaries
        {
            bail!("binary execution disabled by administrator")
        }
        if !invocation_args.is_empty() && manifest.launch.kind != LaunchKind::Binary {
            bail!("only binary packages accept invocation arguments")
        }
    }
    stop_running_package(&app, &id).await?;
    let (finished, receiver) = tokio::sync::watch::channel(false);
    let running = Arc::new(RunningPackage {
        cancelled: Arc::new(AtomicBool::new(false)),
        finished: receiver,
    });
    {
        let mut plugins = app.plugins.lock().await;
        if plugins.shutting_down {
            bail!("server is shutting down");
        }
        plugins.running.insert(id.clone(), running.clone());
    }
    tokio::spawn(async move {
        if let Err(error) = run_package(
            app.clone(),
            id.clone(),
            invocation_args,
            running.cancelled.clone(),
        )
        .await
        {
            if !running.cancelled.load(Ordering::Relaxed) {
                tracing::error!(package = %id, %error, "package execution failed")
            }
        }
        let mut plugins = app.plugins.lock().await;
        if plugins.running.get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, &running))
        {
            plugins.running.remove(&id);
        }
        let _ = finished.send(true);
    });
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(UI)
}
async fn custom_web(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<Html<String>, ApiError> {
    let page = a
        .custom_web
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| anyhow!("custom web page is not registered"))?;
    let root = fs::canonicalize(&page.root)?;
    let file = fs::canonicalize(&page.file)?;
    if !file.starts_with(&root) {
        return Err(ApiError(anyhow!("custom web path escapes plugin directory")));
    }
    Ok(Html(fs::read_to_string(file)?))
}
async fn list_custom_web(State(a): State<App>) -> Json<Vec<CustomWebInfo>> {
    let pages = a.custom_web.lock().await;
    let mut result: Vec<_> = pages
        .keys()
        .map(|id| CustomWebInfo {
            id: id.clone(),
            url: format!("/web/{id}"),
        })
        .collect();
    result.sort_by(|left, right| left.id.cmp(&right.id));
    Json(result)
}
fn lua_permissions(settings: &LuaSettings) -> Vec<String> {
    let p = &settings.permissions;
    let mut permissions = Vec::new();
    for (enabled, name) in [
        (p.led, "led"),
        (p.volume, "volume"),
        (p.audio, "audio"),
        (p.microphone, "microphone"),
        (p.bluetooth, "bluetooth"),
        (p.display, "display"),
        (p.sensors, "sensors"),
        (p.clock, "clock"),
        (p.input, "input"),
        (p.network, "network"),
        (p.ipc_read, "ipc.read"),
        (p.ipc_control, "ipc.control"),
        (p.commands, "commands"),
        (p.system, "system"),
        (p.exec, "exec"),
        (p.web, "web"),
    ] {
        if enabled {
            permissions.push(name.into());
        }
    }
    permissions
}
async fn list_lua_files(State(a): State<App>) -> Result<Json<Vec<LuaFileInfo>>, ApiError> {
    let settings = a.settings.lock().await.lua.clone();
    safe_relative(&settings.share_dir)?;
    let root = a.root.join(&settings.share_dir);
    let mut files = Vec::new();
    if root.is_dir() {
        for entry in walkdir::WalkDir::new(&root) {
            let entry = entry?;
            if !entry.file_type().is_file()
                || entry.path().extension().and_then(|x| x.to_str()) != Some("lua")
            {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&root)?
                .to_string_lossy()
                .to_string();
            files.push(LuaFileInfo {
                file: relative,
                bytes: entry.metadata()?.len(),
            });
        }
    }
    files.sort_by(|left, right| left.file.cmp(&right.file));
    Ok(Json(files))
}
async fn run_lua_file(
    State(a): State<App>,
    Json(request): Json<LuaRunRequest>,
) -> Result<(StatusCode, Json<LuaRunInfo>), ApiError> {
    let settings = a.settings.lock().await.lua.clone();
    if !settings.enabled {
        return Err(ApiError(anyhow!("user Lua execution is disabled")));
    }
    safe_relative(&settings.share_dir)?;
    safe_relative(&request.file)?;
    if Path::new(&request.file)
        .extension()
        .and_then(|x| x.to_str())
        != Some("lua")
    {
        return Err(ApiError(anyhow!("Lua file must have .lua extension")));
    }
    let share_root = a.root.join(&settings.share_dir);
    let source_path = share_root.join(&request.file);
    if !source_path.is_file() {
        return Err(ApiError(anyhow!("Lua file not found: {}", request.file)));
    }
    let id = format!("user_lua_{}", Uuid::new_v4().simple());
    let plugin_root = a.root.join("plugins").join(&id);
    fs::create_dir_all(&plugin_root)?;
    fs::copy(&source_path, plugin_root.join("main.lua"))?;
    for entry in walkdir::WalkDir::new(&share_root) {
        let entry = entry?;
        if !entry.file_type().is_file() || entry.path() == source_path {
            continue;
        }
        let relative = entry.path().strip_prefix(&share_root)?;
        let destination = plugin_root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), destination)?;
    }
    let manifest = Manifest {
        dependencies: BTreeMap::new(),
        media_provider: None,
        id: id.clone(),
        name: format!("User Lua: {}", request.file),
        version: "0.0.0".into(),
        entry: "main.lua".into(),
        description: "User Lua script from Luna share directory".into(),
        permissions: lua_permissions(&settings),
        settings: BTreeMap::new(),
        network: NetworkPolicy {
            allow: settings.network_allow.clone(),
            allow_insecure_http: false,
        },
        input: InputPolicy {
            devices: settings.input_devices.clone(),
        },
        ipc: IpcPolicy {
            services: settings.ipc_services.clone(),
        },
        commands: CommandPolicy {
            phrases: settings.command_phrases.clone(),
        },
        compatibility: Compatibility::default(),
        launch: Launch::default(),
    };
    fs::write(
        plugin_root.join("plugin.toml"),
        toml::to_string_pretty(&manifest)?,
    )?;
    {
        let mut plugins = a.plugins.lock().await;
        plugins.enabled.insert(id.clone());
        plugins.save()?;
    }
    if let Err(error) = start_package(a.clone(), id.clone(), Vec::new()).await {
        let _ = fs::remove_dir_all(&plugin_root);
        return Err(error.into());
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(LuaRunInfo {
            id,
            file: request.file,
            status: "started",
        }),
    ))
}
async fn stop_lua_file(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    validate_id(&id)?;
    if !id.starts_with("user_lua_") {
        return Err(ApiError(anyhow!("not a user Lua execution")));
    }
    let lifecycle = a.plugins.lock().await.lifecycle.clone();
    let _operation = lifecycle.lock().await;
    stop_running_package(&a, &id).await?;
    let mut plugins = a.plugins.lock().await;
    plugins.enabled.remove(&id);
    plugins.service_enabled.remove(&id);
    plugins.save()?;
    drop(plugins);
    let _ = fs::remove_dir_all(a.root.join("plugins").join(&id));
    Ok(StatusCode::OK)
}
async fn list(State(a): State<App>) -> Result<Json<Vec<PluginInfo>>, ApiError> {
    let d = a.device.lock().await.clone();
    Ok(Json(a.plugins.lock().await.list(&d)?))
}
#[derive(Debug, Serialize)]
struct PluginSources {
    local_root: PathBuf,
    yam_root: Option<PathBuf>,
    yam_run: Option<PathBuf>,
    yam_enabled: bool,
}
async fn plugin_sources(State(a): State<App>) -> Json<PluginSources> {
    let plugins = a.plugins.lock().await;
    Json(PluginSources {
        local_root: plugins.root.clone(),
        yam_root: plugins.yam_plugins_root(),
        yam_run: plugins.yam_run.clone(),
        yam_enabled: plugins.yam_root.is_some(),
    })
}
async fn rescan_plugins(State(a): State<App>) -> Result<Json<Vec<PluginInfo>>, ApiError> {
    let device = a.device.lock().await.clone();
    Ok(Json(a.plugins.lock().await.rescan(&device)?))
}
async fn install(State(a): State<App>, mut form: Multipart) -> Result<Json<Manifest>, ApiError> {
    while let Some(f) = form.next_field().await? {
        if f.name() == Some("package") {
            let b = f.bytes().await?;
            let m = a.plugins.lock().await.install(&b)?;
            return Ok(Json(m));
        }
    }
    Err(ApiError(anyhow!("missing package field")))
}

#[derive(Deserialize)]
struct RegistryInstall {
    id: String,
    version: Option<String>,
    registry: String,
}

async fn install_registry(
    State(a): State<App>,
    Json(request): Json<RegistryInstall>,
) -> Result<Json<Manifest>, ApiError> {
    let device = a.device.lock().await.clone();
    let package = task::spawn_blocking(move || {
        download_registry_package(
            &request.registry,
            &request.id,
            request.version.as_deref(),
            &device,
        )
    })
    .await
    .context("registry download task failed")??;
    let manifest = a.plugins.lock().await.install(&package)?;
    Ok(Json(manifest))
}

fn download_registry_package(
    registry: &str,
    id: &str,
    version: Option<&str>,
    device: &DeviceProfile,
) -> Result<Vec<u8>> {
    let index_url = if registry.trim_end_matches('/').ends_with("index.json") {
        registry.to_string()
    } else {
        format!("{}/index.json", registry.trim_end_matches('/'))
    };
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let index: serde_json::Value = client.get(&index_url).send()?.error_for_status()?.json()?;
    let packages = index["packages"]
        .as_array()
        .context("invalid registry package index")?;
    let entry = packages
        .iter()
        .filter(|entry| entry["id"] == id)
        .filter(|entry| version.is_none_or(|version| entry["version"] == version))
        .filter(|entry| registry_compatible(entry, device))
        .max_by(|left, right| {
            registry_version(
                left["version"].as_str().unwrap_or(""),
                right["version"].as_str().unwrap_or(""),
            )
            .then_with(|| {
                registry_target_rank(left, device).cmp(&registry_target_rank(right, device))
            })
        })
        .with_context(|| {
            format!(
                "no package build for {} matches device {}",
                id, device.model
            )
        })?;
    let package_url = entry["url"]
        .as_str()
        .context("registry entry has no package URL")?;
    let package_url = Url::parse(package_url)
        .or_else(|_| Url::parse(&index_url)?.join(package_url))?
        .to_string();
    let package = client
        .get(package_url)
        .send()?
        .error_for_status()?
        .bytes()?
        .to_vec();
    let expected = entry["sha256"]
        .as_str()
        .context("registry entry has no SHA-256")?;
    if format!("{:x}", Sha256::digest(&package)) != expected {
        bail!("package checksum mismatch")
    }
    Ok(package)
}
fn registry_values<'a>(entry: &'a serde_json::Value, key: &str) -> Vec<&'a str> {
    entry[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect()
}
fn registry_compatible(entry: &serde_json::Value, device: &DeviceProfile) -> bool {
    let models = registry_values(entry, "models");
    let features = registry_values(entry, "features");
    (models.is_empty() || models.contains(&device.model.as_str()))
        && features
            .iter()
            .all(|feature| device.features.iter().any(|available| available == feature))
}
fn registry_target_rank(entry: &serde_json::Value, device: &DeviceProfile) -> u8 {
    u8::from(registry_values(entry, "models").contains(&device.model.as_str()))
}

fn registry_version(left: &str, right: &str) -> std::cmp::Ordering {
    let numbers = |value: &str| {
        value
            .trim_start_matches('v')
            .split(|character: char| character == '.' || character == '-')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let left_numbers = numbers(left);
    let right_numbers = numbers(right);
    for index in 0..left_numbers.len().max(right_numbers.len()) {
        match left_numbers
            .get(index)
            .copied()
            .unwrap_or(0)
            .cmp(&right_numbers.get(index).copied().unwrap_or(0))
        {
            std::cmp::Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    left.cmp(right)
}
async fn toggle(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let lifecycle = a.plugins.lock().await.lifecycle.clone();
    let _operation = lifecycle.lock().await;
    let disabling = {
        let mut plugins = a.plugins.lock().await;
        plugins.manifest(&id)?;
        let enabling = plugins.enabled.insert(id.clone());
        if !enabling {
            plugins.enabled.remove(&id);
            plugins.service_enabled.remove(&id);
        }
        plugins.save()?;
        !enabling
    };
    if disabling {
        stop_running_package(&a, &id).await?;
    }
    Ok(StatusCode::OK)
}
async fn remove(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let lifecycle = a.plugins.lock().await.lifecycle.clone();
    let _operation = lifecycle.lock().await;
    stop_running_package(&a, &id).await?;
    a.plugins.lock().await.remove(&id)?;
    Ok(StatusCode::OK)
}

async fn invoke(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    invocation: Option<Json<Invocation>>,
) -> Result<StatusCode, ApiError> {
    start_package(
        a,
        id,
        invocation.map(|value| value.0.args).unwrap_or_default(),
    )
    .await?;
    Ok(StatusCode::ACCEPTED)
}
async fn start_service(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    start_package(a, id, Vec::new()).await?;
    Ok(StatusCode::ACCEPTED)
}
async fn enable_service(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let lifecycle = a.plugins.lock().await.lifecycle.clone();
    let _operation = lifecycle.lock().await;
    {
        let mut plugins = a.plugins.lock().await;
        plugins.manifest(&id)?;
        plugins.enabled.insert(id.clone());
        plugins.service_enabled.insert(id.clone());
        plugins.save()?;
    }
    start_package_locked(a, id, Vec::new()).await?;
    Ok(StatusCode::ACCEPTED)
}
async fn disable_service(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let lifecycle = a.plugins.lock().await.lifecycle.clone();
    let _operation = lifecycle.lock().await;
    let revoked = a.runtime_tokens.lock().await.get(&id).cloned();
    {
        let mut plugins = a.plugins.lock().await;
        plugins.manifest(&id)?;
        plugins.service_enabled.remove(&id);
        plugins.save()?;
    }
    stop_running_package(&a, &id).await?;
    if let Some(token) = revoked {
        revoke_runtime(&a, &id, &token).await;
    }
    Ok(StatusCode::OK)
}
async fn stop_plugin(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let lifecycle = a.plugins.lock().await.lifecycle.clone();
    let _operation = lifecycle.lock().await;
    let revoked = a.runtime_tokens.lock().await.get(&id).cloned();
    let uses_led = {
        let plugins = a.plugins.lock().await;
        plugins.running.contains_key(&id) && plugins.manifest(&id)?.permissions.iter().any(|p| p == "led")
    };
    stop_running_package(&a, &id).await?;
    if let Some(token) = revoked {
        revoke_runtime(&a, &id, &token).await;
    }
    if uses_led {
        let _ = a.led.off().await;
    }
    Ok(StatusCode::OK)
}
async fn revoke_runtime(app: &App, id: &str, expected: &str) {
    let revoke = {
        let mut tokens = app.runtime_tokens.lock().await;
        if tokens.get(id).map(String::as_str) == Some(expected) {
            tokens.remove(id);
            true
        } else {
            false
        }
    };
    if revoke {
        app.speech_readers.lock().await.remove(id);
        app.speech_claims.lock().await.remove(id);
    }
}
async fn package_logs(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<String, ApiError> {
    validate_id_or_yam(&id)?;
    let path = a.plugins.lock().await.state_root(&id).join("runtime.log");
    Ok(fs::read_to_string(path).unwrap_or_default())
}
fn runtime_token(headers: &HeaderMap) -> Result<&str> {
    headers
        .get("x-luna-token")
        .and_then(|value| value.to_str().ok())
        .context("missing X-Luna-Token")
}
async fn runtime_permission(
    app: &App,
    id: &str,
    headers: &HeaderMap,
    permission: &str,
) -> Result<(), ApiError> {
    validate_id_or_yam(id)?;
    let token = runtime_token(headers)?;
    if app.runtime_tokens.lock().await.get(id).map(String::as_str) != Some(token) {
        return Err(ApiError(anyhow!("invalid runtime token")));
    }
    let manifest = app.plugins.lock().await.manifest(id)?;
    if !manifest.permissions.iter().any(|item| item == permission) {
        return Err(ApiError(anyhow!("package lacks {permission} permission")));
    }
    Ok(())
}
async fn runtime_device(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<DeviceMetadata>, ApiError> {
    runtime_permission(&a, &id, &headers, "exec").await?;
    Ok(Json(device_metadata(&a).await?))
}
async fn runtime_sensors(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<SensorInfo>>, ApiError> {
    runtime_permission(&a, &id, &headers, "sensors").await?;
    Ok(get_sensors().await)
}
async fn runtime_led(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(frame): Json<Frame>,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "led").await?;
    a.led.draw(frame.colors).await?;
    Ok(StatusCode::OK)
}
async fn runtime_led_ring(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(frame): Json<Frame>,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "led").await?;
    a.led.draw_ring(frame.colors).await?;
    Ok(StatusCode::OK)
}
async fn runtime_led_logo(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(frame): Json<Frame>,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "led").await?;
    a.led.draw_logo(frame.colors).await?;
    Ok(StatusCode::OK)
}
async fn runtime_led_off(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "led").await?;
    a.led.off().await?;
    Ok(StatusCode::OK)
}
async fn runtime_led_ring_off(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "led").await?;
    a.led.off_ring().await?;
    Ok(StatusCode::OK)
}
async fn runtime_led_logo_off(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "led").await?;
    a.led.off_logo().await?;
    Ok(StatusCode::OK)
}
async fn runtime_clock(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<ClockRequest>,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "clock").await?;
    put_clock_request(a.clock.as_ref(), request)
}
async fn runtime_clock_off(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "clock").await?;
    clock_off_request(a.clock.as_ref())
}
async fn get_settings(State(a): State<App>) -> Json<Settings> {
    Json(a.settings.lock().await.clone())
}
async fn device_metadata(a: &App) -> Result<DeviceMetadata> {
    Ok(DeviceMetadata {
        profile: a.device.lock().await.clone(),
        room: Room::load(&a.root)?.room,
    })
}
async fn get_device(State(a): State<App>) -> Result<Json<DeviceMetadata>, ApiError> {
    Ok(Json(device_metadata(&a).await?))
}
async fn get_room(State(a): State<App>) -> Result<Json<Room>, ApiError> {
    Ok(Json(Room::load(&a.root)?))
}
async fn put_room(State(a): State<App>, Json(room): Json<Room>) -> Result<Json<Room>, ApiError> {
    let room = room.normalized()?;
    room.save(&a.root)?;
    Ok(Json(room))
}
async fn get_sensors() -> Json<Vec<SensorInfo>> {
    Json(
        discover_sensors()
            .into_iter()
            .map(|sensor| SensorInfo {
                value: sensor_value(&sensor).ok(),
                id: sensor.id,
                kind: sensor.kind,
                unit: sensor.unit,
            })
            .collect(),
    )
}
#[derive(Debug, Deserialize)]
struct VolumeRequest {
    percent: u8,
}

#[derive(Debug, Default, Deserialize)]
struct VolumeStepRequest {
    #[serde(default)]
    percent: Option<u8>,
}

#[derive(Debug, Serialize)]
struct VolumeResponse {
    percent: u8,
}

fn amixer_volume() -> Result<u8> {
    let output = amixer_output(&["get".into(), "Master".into()])?;
    let percent = output
        .match_indices('%')
        .next_back()
        .and_then(|(index, _)| {
            output[..index]
                .chars()
                .rev()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
                .parse::<u8>()
                .ok()
        })
        .context("cannot parse Master volume from amixer output")?;
    Ok(percent)
}

fn volume_manager_message(percent: u8, muted: bool) -> Vec<u8> {
    // quasar.proto.QuasarMessage.volume_manager_message = 200
    // quasar.proto.VolumeManager.set_volume_signal = 2
    // SetVolumeSignal.platform_volume is an integer in 0..10000.
    let mut signal = Vec::new();
    push_varint(&mut signal, 1 << 3);
    push_varint(&mut signal, u64::from(percent) * 100);
    push_varint(&mut signal, 3 << 3);
    push_varint(&mut signal, u64::from(muted));
    push_bytes(&mut signal, 4, b"luna");

    let mut manager = Vec::new();
    push_bytes(&mut manager, 2, &signal);
    let mut message = Vec::new();
    push_bytes(&mut message, 200, &manager);
    message
}

fn set_volume_via_maind(percent: u8, muted: bool) -> Result<()> {
    let endpoint = ipc_endpoint("volume_manager")?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut stream =
        connect_ipc(endpoint, deadline)?.context("maind volume manager is unavailable")?;
    write_ipc_frame(&mut stream, &volume_manager_message(percent, muted))
}

fn set_volume_backend(percent: u8, muted: bool) -> Result<()> {
    // Prefer maind/VQE so Alice, the touch sensor and Luna share one state.
    // Some older Mini firmware advertises volume_manager.port but does not
    // actually expose the listener; retain ALSA as a compatibility fallback.
    if set_volume_via_maind(percent, muted).is_ok() {
        return Ok(());
    }
    amixer_output(&[
        "set".into(),
        "Master".into(),
        format!("{}% {}", percent, if muted { "mute" } else { "unmute" }),
    ])?;
    Ok(())
}

async fn get_bluetooth_status() -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(
        task::spawn_blocking(bluetooth::status)
            .await
            .context("Bluetooth status task failed")??,
    ))
}

async fn put_bluetooth_power(
    Json(request): Json<BluetoothPowerRequest>,
) -> Result<StatusCode, ApiError> {
    task::spawn_blocking(move || bluetooth::power(request.on))
        .await
        .context("Bluetooth power task failed")??;
    Ok(StatusCode::OK)
}

async fn put_bluetooth_pairable(
    Json(request): Json<BluetoothPowerRequest>,
) -> Result<StatusCode, ApiError> {
    task::spawn_blocking(move || bluetooth::pairable(request.on))
        .await
        .context("Bluetooth pairing mode task failed")??;
    Ok(StatusCode::OK)
}

async fn put_bluetooth_scan(
    Json(request): Json<BluetoothScanRequest>,
) -> Result<StatusCode, ApiError> {
    task::spawn_blocking(move || bluetooth::scan(request.on))
        .await
        .context("Bluetooth scan task failed")??;
    Ok(StatusCode::OK)
}

async fn post_bluetooth_action(
    AxumPath(action): AxumPath<String>,
    Json(request): Json<BluetoothDeviceRequest>,
) -> Result<StatusCode, ApiError> {
    task::spawn_blocking(move || bluetooth::device_action(&action, &request.address))
        .await
        .context("Bluetooth action task failed")??;
    Ok(StatusCode::OK)
}

async fn get_volume() -> Result<Json<VolumeResponse>, ApiError> {
    Ok(Json(VolumeResponse {
        percent: amixer_volume()?,
    }))
}

async fn set_volume(Json(request): Json<VolumeRequest>) -> Result<Json<VolumeResponse>, ApiError> {
    if request.percent > 100 {
        return Err(ApiError(anyhow!("volume must be 0..100")));
    }
    set_volume_backend(request.percent, false)?;
    Ok(Json(VolumeResponse {
        percent: amixer_volume()?,
    }))
}

async fn change_volume(
    direction: i16,
    request: Option<Json<VolumeStepRequest>>,
) -> Result<Json<VolumeResponse>, ApiError> {
    let step = request
        .and_then(|Json(request)| request.percent)
        .unwrap_or(5);
    if step == 0 || step > 100 {
        return Err(ApiError(anyhow!("volume step must be 1..100")));
    }
    let current = amixer_volume()? as i16;
    let next = (current + direction * step as i16).clamp(0, 100) as u8;
    set_volume_backend(next, false)?;
    Ok(Json(VolumeResponse {
        percent: amixer_volume()?,
    }))
}

async fn volume_up(
    request: Option<Json<VolumeStepRequest>>,
) -> Result<Json<VolumeResponse>, ApiError> {
    change_volume(1, request).await
}

async fn volume_down(
    request: Option<Json<VolumeStepRequest>>,
) -> Result<Json<VolumeResponse>, ApiError> {
    change_volume(-1, request).await
}

async fn set_mute(muted: bool) -> Result<Json<VolumeResponse>, ApiError> {
    set_volume_backend(amixer_volume()?, muted)?;
    Ok(Json(VolumeResponse {
        percent: amixer_volume()?,
    }))
}

async fn mute_volume() -> Result<Json<VolumeResponse>, ApiError> {
    set_mute(true).await
}

async fn unmute_volume() -> Result<Json<VolumeResponse>, ApiError> {
    set_mute(false).await
}
async fn get_input_devices() -> Result<Json<Vec<InputDeviceInfo>>, ApiError> {
    let listing =
        fs::read_to_string("/proc/bus/input/devices").context("read /proc/bus/input/devices")?;
    Ok(Json(
        list_input_devices(&listing)
            .into_iter()
            .map(|(name, path)| InputDeviceInfo { name, path })
            .collect(),
    ))
}
async fn get_input_event(
    State(a): State<App>,
    Query(query): Query<InputQuery>,
) -> Result<Json<Option<InputEventInfo>>, ApiError> {
    let timeout_ms = query.timeout_ms.unwrap_or(1_000).min(60_000);
    let listing =
        fs::read_to_string("/proc/bus/input/devices").context("read /proc/bus/input/devices")?;
    let available = list_input_devices(&listing);
    let wanted = if let Some(device) = query.device {
        vec![device]
    } else {
        available.iter().map(|(name, _)| name.clone()).collect()
    };
    let devices = resolve_input_devices(&listing, &wanted)?;
    let event = task::spawn_blocking(move || -> Result<Option<InputEvent>> {
        let mut reader = InputReader::open(&devices)?;
        reader.next(timeout_ms, &AtomicBool::new(false))
    })
    .await
    .context("input reader task failed")??;
    Ok(Json(
        event.map(|event| input_event_info(a.input_actions, event)),
    ))
}
async fn get_input_events(
    State(a): State<App>,
    Query(query): Query<InputQuery>,
) -> Result<Json<Vec<InputEventInfo>>, ApiError> {
    let timeout_ms = query.timeout_ms.unwrap_or(5_000).min(60_000);
    let max_events = query.max_events.unwrap_or(128).clamp(1, 1024);
    let idle_ms = query.idle_ms.unwrap_or(250).clamp(20, 2_000);
    let listing =
        fs::read_to_string("/proc/bus/input/devices").context("read /proc/bus/input/devices")?;
    let available = list_input_devices(&listing);
    let wanted = if let Some(device) = query.device {
        vec![device]
    } else {
        available.iter().map(|(name, _)| name.clone()).collect()
    };
    let devices = resolve_input_devices(&listing, &wanted)?;
    let events = task::spawn_blocking(move || -> Result<Vec<InputEvent>> {
        let mut reader = InputReader::open(&devices)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut events = Vec::new();
        while events.len() < max_events {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let wait = if events.is_empty() {
                remaining.as_millis() as u64
            } else {
                remaining.as_millis().min(idle_ms as u128) as u64
            };
            let Some(event) = reader.next(wait, &AtomicBool::new(false))? else {
                break;
            };
            events.push(event);
        }
        Ok(events)
    })
    .await
    .context("input events reader task failed")??;
    Ok(Json(
        events
            .into_iter()
            .map(|event| input_event_info(a.input_actions, event))
            .collect(),
    ))
}
async fn get_input_action(
    State(a): State<App>,
    AxumPath(action_name): AxumPath<String>,
    Query(query): Query<InputQuery>,
) -> Result<Json<Option<InputEventInfo>>, ApiError> {
    let action = a
        .input_actions
        .iter()
        .find(|action| action.name == action_name)
        .with_context(|| format!("input action is not supported: {action_name}"))?;
    let timeout_ms = query.timeout_ms.unwrap_or(60_000).min(60_000);
    let listing =
        fs::read_to_string("/proc/bus/input/devices").context("read /proc/bus/input/devices")?;
    let devices = resolve_input_devices(&listing, &[action.device.to_string()])?;
    let action = *action;
    let event = task::spawn_blocking(move || -> Result<Option<InputEvent>> {
        let mut reader = InputReader::open(&devices)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let event = reader.next(remaining.as_millis() as u64, &AtomicBool::new(false))?;
            let Some(event) = event else {
                return Ok(None);
            };
            if event.kind == action.kind && event.code == action.code && event.value == 1 {
                return Ok(Some(event));
            }
        }
    })
    .await
    .context("input action reader task failed")??;
    Ok(Json(
        event.map(|event| input_event_info(a.input_actions, event)),
    ))
}
async fn get_input_gesture(
    State(a): State<App>,
    AxumPath(gesture_name): AxumPath<String>,
    Query(query): Query<InputQuery>,
) -> Result<Json<InputGestureInfo>, ApiError> {
    let gesture = a
        .input_gestures
        .iter()
        .find(|gesture| gesture.name == gesture_name)
        .with_context(|| format!("input gesture is not supported: {gesture_name}"))?;
    let first_action = gesture
        .actions
        .first()
        .context("input gesture has no actions")?;
    let device = a
        .input_actions
        .iter()
        .find(|action| action.name == *first_action)
        .map(|action| action.device)
        .context("input gesture has no matching first action")?;
    let timeout_ms = query.timeout_ms.unwrap_or(60_000).min(60_000);
    let listing =
        fs::read_to_string("/proc/bus/input/devices").context("read /proc/bus/input/devices")?;
    let devices = resolve_input_devices(&listing, &[device.to_string()])?;
    let gesture = *gesture;
    let actions = a.input_actions;
    let events = task::spawn_blocking(move || -> Result<Vec<InputEvent>> {
        let mut reader = InputReader::open(&devices)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut matched = Vec::with_capacity(gesture.actions.len());
        let mut next = 0usize;
        let mut last_press_time = None;
        while next < gesture.actions.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(Vec::new());
            }
            let wait = if next == 0 {
                remaining.as_millis() as u64
            } else {
                remaining.as_millis().min(gesture.max_gap_ms as u128) as u64
            };
            let Some(event) = reader.next(wait, &AtomicBool::new(false))? else {
                return Ok(Vec::new());
            };
            if event.value != 1 {
                continue;
            }
            if last_press_time
                .is_some_and(|last| event.time_ms.saturating_sub(last) > gesture.max_gap_ms as i64)
            {
                matched.clear();
                next = 0;
            }
            last_press_time = Some(event.time_ms);
            let Some(action) = input_action_for(actions, &event) else {
                continue;
            };
            if action == gesture.actions[next] {
                matched.push(event);
                next += 1;
            } else if action == gesture.actions[0] {
                matched.clear();
                matched.push(event);
                next = 1;
            } else {
                matched.clear();
                next = 0;
            }
        }
        Ok(matched)
    })
    .await
    .context("input gesture reader task failed")??;
    Ok(Json(InputGestureInfo {
        gesture: if events.len() == gesture.actions.len() {
            Some(gesture.name.into())
        } else {
            None
        },
    }))
}
#[derive(Debug, Deserialize)]
struct ClockTextRequest {
    text: String,
    #[serde(default = "text_brightness")]
    brightness: u8,
    #[serde(default = "text_duration")]
    duration_ms: u64,
}
fn text_brightness() -> u8 {
    100
}
fn text_duration() -> u64 {
    2000
}
fn validate_clock_text(request: &ClockTextRequest) -> Result<()> {
    anyhow::ensure!(
        (1..=15000).contains(&request.duration_ms),
        "duration_ms must be 1..15000"
    );
    crate::clock_text::frames(&request.text, request.brightness)?;
    Ok(())
}

// Serialise frame writes with cancellation of an older overlay.
static CLOCK_TEXT_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
static CLOCK_TEXT_EPOCH: AtomicU64 = AtomicU64::new(0);
static CLOCK_TEXT_STOPPING: AtomicBool = AtomicBool::new(false);

async fn show_clock_text(request: ClockTextRequest) -> Result<()> {
    anyhow::ensure!(
        !CLOCK_TEXT_STOPPING.load(Ordering::SeqCst),
        "server is shutting down"
    );
    validate_clock_text(&request)?;
    let layout = clock_layout().context("text display requires a supported segment clock")?;
    anyhow::ensure!(
        clock_text_available(false, Some(layout)),
        "text display requires a supported digit layout"
    );
    let frames = crate::clock_text::frames(&request.text, request.brightness)?;
    let epoch = {
        let _gate = CLOCK_TEXT_GATE.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            !CLOCK_TEXT_STOPPING.load(Ordering::SeqCst),
            "server is shutting down"
        );
        CLOCK_TEXT_EPOCH.fetch_add(1, Ordering::SeqCst) + 1
    };
    // The task owns release even if the HTTP caller disconnects.
    tokio::spawn(async move {
        let duration = Duration::from_millis(request.duration_ms);
        let start = tokio::time::Instant::now();
        let count = frames.len() as u32;
        let mut previous: Option<Vec<(PathBuf, String)>> = None;
        let result: Result<()> = async {
            for (index, frame) in frames.into_iter().enumerate() {
                {
                    let _gate = CLOCK_TEXT_GATE.lock().unwrap_or_else(|e| e.into_inner());
                    if CLOCK_TEXT_EPOCH.load(Ordering::SeqCst) != epoch {
                        return Ok(());
                    }
                    // A new stock frame (e.g. timer) wins; never paint over it again.
                    if let Some(last) = &previous {
                        if clock_read(layout)? != *last {
                            return Ok(());
                        }
                    }
                    clock_write_frame(&frame.into_iter().collect())?;
                    previous = Some(clock_read(layout)?);
                }
                tokio::time::sleep_until(start + duration * (index as u32 + 1) / count).await;
            }
            Ok(())
        }
        .await;
        let _gate = CLOCK_TEXT_GATE.lock().unwrap_or_else(|e| e.into_inner());
        if CLOCK_TEXT_EPOCH.load(Ordering::SeqCst) == epoch {
            clock_release_frame()?;
        }
        result
    })
    .await
    .context("clock overlay task failed")?
}

async fn post_clock_text(
    State(a): State<App>,
    Json(request): Json<ClockTextRequest>,
) -> Result<StatusCode, ApiError> {
    if !clock_text_available(a.clock.is_some(), clock_layout()) {
        return Err(ApiError(anyhow!("text display requires a supported digit layout")));
    }
    show_clock_text(request).await?;
    Ok(StatusCode::OK)
}

fn clock_text_available(station_backend: bool, layout: Option<&ClockLayout>) -> bool {
    !station_backend && matches!(layout, Some(ClockLayout::Cucumber(_)))
}

pub(crate) fn stop_clock_text() -> Result<()> {
    let _gate = CLOCK_TEXT_GATE.lock().unwrap_or_else(|e| e.into_inner());
    CLOCK_TEXT_STOPPING.store(true, Ordering::SeqCst);
    CLOCK_TEXT_EPOCH.fetch_add(1, Ordering::SeqCst);
    if clock_available() {
        clock_release_frame()?;
    }
    Ok(())
}

async fn get_clock(State(a): State<App>) -> Json<ClockInfo> {
    if let Some(clock) = &a.clock {
        return Json(ClockInfo {
            available: true,
            text_available: false,
            segments: clock.segments(),
        });
    }
    Json(ClockInfo {
        available: clock_available(),
        text_available: clock_text_available(false, clock_layout()),
        segments: clock_segments(),
    })
}
async fn post_clock(
    State(a): State<App>,
    Json(request): Json<ClockRequest>,
) -> Result<StatusCode, ApiError> {
    put_clock_request(a.clock.as_ref(), request)
}

async fn clock_off(State(a): State<App>) -> Result<StatusCode, ApiError> {
    clock_off_request(a.clock.as_ref())
}

fn clock_off_request(
    station_clock: Option<&Arc<dyn ClockBackend>>,
) -> Result<StatusCode, ApiError> {
    if let Some(clock) = station_clock {
        clock.apply(ClockCommand::Off)?;
    } else {
        clock_release()?;
    }
    Ok(StatusCode::OK)
}

fn put_clock_request(
    station_clock: Option<&Arc<dyn ClockBackend>>,
    request: ClockRequest,
) -> Result<StatusCode, ApiError> {
    if let Some(clock) = station_clock {
        let command = if let Some(time) = request.time {
            ClockCommand::Time {
                text: time,
                brightness: request.brightness.unwrap_or(170),
            }
        } else if let Some(segments) = request.segments {
            ClockCommand::Segments(segments)
        } else {
            return Err(ApiError(anyhow!("pass time or segments")));
        };
        clock.apply(command)?;
        return Ok(StatusCode::OK);
    }
    if let Some(time) = request.time {
        clock_time(&time, request.brightness.unwrap_or(170))?;
    } else if let Some(segments) = request.segments {
        validate_clock_segments(&segments)?;
        clock_write(&segments)?;
    } else {
        return Err(ApiError(anyhow!("pass time or segments")));
    }
    Ok(StatusCode::OK)
}
async fn put_device(
    State(a): State<App>,
    Json(d): Json<DeviceProfile>,
) -> Result<StatusCode, ApiError> {
    validate_id(&d.model)?;
    write_atomic(
        &a.root.join("device.toml"),
        toml::to_string_pretty(&d)?.as_bytes(),
    )?;
    *a.device.lock().await = d;
    Ok(StatusCode::OK)
}
async fn put_settings(
    State(a): State<App>,
    Json(s): Json<Settings>,
) -> Result<StatusCode, ApiError> {
    if s.led.count == 0 || s.led.count > 128 {
        return Err(ApiError(anyhow!("LED count must be 1..128")));
    };
    if s.web.ui && !s.web.api {
        return Err(ApiError(anyhow!("web.ui requires web.api")));
    }
    safe_relative(&s.lua.share_dir)?;
    if s.lua.share_dir.is_empty() || s.lua.max_runtime_seconds == 0 {
        return Err(ApiError(anyhow!(
            "lua.share_dir and lua.max_runtime_seconds are required"
        )));
    }
    write_atomic(
        &a.root.join("settings.toml"),
        toml::to_string_pretty(&s)?.as_bytes(),
    )?;
    *a.settings.lock().await = s.clone();
    *a.led.settings.lock().await = s.led;
    Ok(StatusCode::OK)
}
async fn led_set(State(a): State<App>, Json(f): Json<Frame>) -> Result<StatusCode, ApiError> {
    a.led.draw(f.colors).await?;
    Ok(StatusCode::OK)
}
async fn led_set_ring(State(a): State<App>, Json(f): Json<Frame>) -> Result<StatusCode, ApiError> {
    a.led.draw_ring(f.colors).await?;
    Ok(StatusCode::OK)
}
async fn led_set_logo(State(a): State<App>, Json(f): Json<Frame>) -> Result<StatusCode, ApiError> {
    a.led.draw_logo(f.colors).await?;
    Ok(StatusCode::OK)
}
async fn led_off(State(a): State<App>) -> Result<StatusCode, ApiError> {
    a.led.off().await?;
    Ok(StatusCode::OK)
}
async fn led_ring_off(State(a): State<App>) -> Result<StatusCode, ApiError> {
    a.led.off_ring().await?;
    Ok(StatusCode::OK)
}
async fn led_logo_off(State(a): State<App>) -> Result<StatusCode, ApiError> {
    a.led.off_logo().await?;
    Ok(StatusCode::OK)
}
#[derive(Debug)]
struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":self.0.to_string()})),
        )
            .into_response()
    }
}

fn start_auto_updater(
    app: App,
    executable: PathBuf,
    listen: String,
    model: String,
    architecture: String,
) {
    task::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            if app.settings.lock().await.updates.enabled {
                let executable = executable.clone();
                let data_dir = app.root.clone();
                let listen = listen.clone();
                let model = model.clone();
                let architecture = architecture.clone();
                match task::spawn_blocking(move || {
                    crate::updater::stage_release_update(
                        &executable,
                        &data_dir,
                        &listen,
                        &model,
                        &architecture,
                    )
                })
                .await
                {
                    Ok(Ok(true)) => {
                        tracing::info!("release update staged; restarting Luna");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        std::process::exit(0);
                    }
                    Ok(Ok(false)) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "automatic update check failed"),
                    Err(error) => tracing::warn!(%error, "automatic update task failed"),
                }
            }
            let interval = app.settings.lock().await.updates.interval_minutes;
            tokio::time::sleep(Duration::from_secs(interval.clamp(5, 24 * 60) * 60)).await;
        }
    });
}
pub fn run() -> Result<()> {
    run_with_station(None)
}

#[tokio::main]
pub async fn run_with_station(station: Option<StationSpec>) -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let args = Args::parse();
    if let Some(RuntimeCommand::Room { value, clear }) = args.command {
        let mutating = clear || value.is_some();
        let room = if mutating {
            fs::create_dir_all(&args.data_dir)?;
            Room { room: value }.normalized()?
        } else {
            Room::load(&args.data_dir)?
        };
        if mutating {
            room.save(&args.data_dir)?;
        }
        println!("{}", room.room.unwrap_or_default());
        return Ok(());
    }
    // One fixed lock is shared by YAM-installed and manually copied Luna
    // binaries, regardless of their executable paths.
    let _instance_guard = crate::instance::acquire()?;
    fs::create_dir_all(&args.data_dir)?;
    let (yam_root, yam_run) = resolve_yam_paths(&args);
    if yam_root.is_some() && yam_run.is_none() {
        tracing::warn!(
            "YAM_ROOT detected but YAM_RUN is unavailable; YAM plugin reads are unlocked"
        );
    }
    let settings_path = args.data_dir.join("settings.toml");
    // A broken settings.toml must not silently fall back to defaults: they
    // re-enable the web API and auto-updates.
    let mut settings: Settings = match fs::read_to_string(&settings_path) {
        Ok(raw) => {
            toml::from_str(&raw).with_context(|| format!("invalid {}", settings_path.display()))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let settings = Settings::default();
            settings
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", settings_path.display()))
        }
    };
    // A station binary carries its model in the executable name.  Only apply
    // hardware defaults when the administrator has not created settings.toml;
    // an explicit settings file remains authoritative.
    let forced_model = args
        .model
        .clone()
        .or_else(|| station.map(|spec| spec.model.to_owned()));
    if !settings_path.exists() {
        if let Some(spec) = station {
            settings.led.profile = spec.led_profile.into();
            settings.led.count = spec.led_count;
        }
    }
    let led = Arc::new(Led {
        settings: Arc::new(Mutex::new(settings.led.clone())),
        backend: station
            .map(|spec| (spec.create_led_backend)().map(Arc::from))
            .transpose()?,
        color_order: station
            .map(|spec| spec.color_order)
            .unwrap_or(ColorOrder::Rgb),
        topology: station.map(|spec| spec.led_topology).unwrap_or_default(),
    });
    let microphone = station
        .map(|spec| (spec.create_microphone_backend)().map(Arc::from))
        .flatten();
    let clock = station
        .map(|spec| (spec.create_clock_backend)().map(Arc::from))
        .flatten();
    let mut device = detect_device(&args.data_dir, forced_model);
    if let Some(spec) = station {
        device.model = spec.model.into();
        device.led = spec.led_topology;
        for capability in spec.capabilities {
            if *capability == "microphone.capture"
                && !Path::new("/system/vendor/quasar/maind").is_file()
            {
                continue;
            }
            if !device.features.iter().any(|feature| feature == capability) {
                device.features.push((*capability).into());
            }
        }
        if spec.led_topology.has_logo {
            if !device.features.iter().any(|feature| feature == "led.logo") {
                device.features.push("led.logo".into());
            }
        }
    }
    let led_features = if configured_rgb_ring(&settings.led) {
        vec![
            "led.ring.rgb".into(),
            format!("led.ring.{}", settings.led.count),
            "led.pattern.v1".into(),
        ]
    } else if Path::new(&settings.led.sysfs_root).exists() {
        vec!["led.sysfs".into()]
    } else {
        vec![]
    };
    for feature in led_features {
        if !device.features.contains(&feature) {
            device.features.push(feature);
        }
    }
    tracing::info!(model=%device.model,features=?device.features,"detected device");
    let app = App {
        root: args.data_dir.clone(),
        settings: Arc::new(Mutex::new(settings)),
        device: Arc::new(Mutex::new(device)),
        led,
        microphone,
        clock,
        plugins: Arc::new(Mutex::new(PluginManager::new(
            args.data_dir.join("plugins"),
            yam_root,
            yam_run,
        )?)),
        runtime_tokens: Arc::new(Mutex::new(HashMap::new())),
        speech_readers: Arc::new(Mutex::new(HashMap::new())),
        speech_claims: Arc::new(Mutex::new(HashMap::new())),
        providers: Arc::new(Mutex::new(providers::Broker::default())),
        custom_web: Arc::new(Mutex::new(HashMap::new())),
        runtime_api_url: crate::updater::runtime_api_url(&args.listen)?,
        input_actions: station.map(|spec| spec.input_actions).unwrap_or(&[]),
        input_gestures: station.map(|spec| spec.input_gestures).unwrap_or(&[]),
        gpio_inputs: station.map(|spec| spec.gpio_inputs).unwrap_or(&[]),
    };
    let updater_device = app.device.lock().await.clone();
    start_auto_updater(
        app.clone(),
        std::env::current_exe()?,
        args.listen.clone(),
        updater_device.model,
        updater_device.architecture,
    );
    let services = {
        let device = app.device.lock().await.clone();
        app.plugins
            .lock()
            .await
            .list(&device)?
            .into_iter()
            .filter(|package| package.enabled && package.service_enabled)
            .map(|package| package.manifest.id)
            .collect::<Vec<_>>()
    };
    for id in services {
        if let Err(error) = start_package(app.clone(), id.clone(), Vec::new()).await {
            tracing::error!(package = %id, %error, "service package failed to start");
        }
    }
    let rescan_app = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let device = rescan_app.device.lock().await.clone();
            if let Err(error) = rescan_app.plugins.lock().await.rescan(&device) {
                tracing::warn!(%error, "YAM plugin rescan failed");
            }
        }
    });
    if let Some(yam_plugins_root) = app.plugins.lock().await.yam_plugins_root() {
        if yam_plugins_root.is_dir() {
            let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
            match notify::recommended_watcher(move |_| {
                let _ = events_tx.send(());
            }) {
                Ok(mut watcher) => {
                    if watcher
                        .watch(&yam_plugins_root, RecursiveMode::Recursive)
                        .is_ok()
                    {
                        let watch_app = app.clone();
                        tokio::spawn(async move {
                            let _watcher: RecommendedWatcher = watcher;
                            while events_rx.recv().await.is_some() {
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                while events_rx.try_recv().is_ok() {}
                                let device = watch_app.device.lock().await.clone();
                                if let Err(error) = watch_app.plugins.lock().await.rescan(&device) {
                                    tracing::warn!(%error, "YAM plugin event rescan failed");
                                }
                            }
                        });
                    } else {
                        tracing::warn!(path=%yam_plugins_root.display(), "cannot watch YAM plugin directory; using periodic rescan");
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot initialize YAM plugin watcher; using periodic rescan")
                }
            }
        }
    }
    let web = app.settings.lock().await.web.clone();
    if web.ui && !web.api {
        bail!("web.ui requires web.api")
    }
    if !web.ui && !web.api {
        tracing::info!("web interface is disabled");
        std::future::pending::<()>().await;
    }
    let mut router = Router::new();
    if web.ui {
        router = router
            .route("/", get(index))
            .route("/web/:id", get(custom_web));
    }
    if web.api {
        router = router
            .route("/api/plugins", get(list).post(install))
            .route("/api/plugins/sources", get(plugin_sources))
            .route("/api/custom-web", get(list_custom_web))
            .route("/api/plugins/rescan", post(rescan_plugins))
            .route("/api/plugins/registry", post(install_registry))
            .route("/api/plugins/:id", delete(remove))
            .route("/api/plugins/:id/toggle", post(toggle))
            .route("/api/plugins/:id/invoke", post(invoke))
            .route("/api/plugins/:id/service/enable", post(enable_service))
            .route("/api/plugins/:id/service/disable", post(disable_service))
            .route("/api/plugins/:id/service/start", post(start_service))
            .route("/api/plugins/:id/stop", post(stop_plugin))
            .route("/api/plugins/:id/logs", get(package_logs))
            .route("/api/lua/files", get(list_lua_files))
            .route("/api/lua/run", post(run_lua_file))
            .route("/api/lua/:id/stop", post(stop_lua_file))
            .route("/api/runtime/:id/device", get(runtime_device))
            .route("/api/runtime/:id/dependencies/:alias", get(dependencies::resolve))
            .route("/api/media", get(providers::status))
            .route("/api/media/select", post(providers::select))
            .route("/api/media/command", post(providers::transport))
            .route("/api/runtime/:id/provider/state", post(providers::report))
            .route("/api/runtime/:id/provider/next", post(providers::poll))
            .route("/api/runtime/:id/provider/result", post(providers::result))
            .route("/api/runtime/:id/media", get(media::capabilities))
            .route("/api/runtime/:id/speech/next", post(media::speech))
            .route(
                "/api/runtime/:id/command/cancel",
                post(media::cancel_command),
            )
            .route("/api/runtime/:id/sensors", get(runtime_sensors))
            .route("/api/runtime/:id/led", post(runtime_led))
            .route("/api/runtime/:id/led/ring", post(runtime_led_ring))
            .route("/api/runtime/:id/led/logo", post(runtime_led_logo))
            .route("/api/runtime/:id/led/off", post(runtime_led_off))
            .route("/api/runtime/:id/led/off/ring", post(runtime_led_ring_off))
            .route("/api/runtime/:id/led/off/logo", post(runtime_led_logo_off))
            .route("/api/runtime/:id/clock", post(runtime_clock))
            .route("/api/runtime/:id/clock/off", post(runtime_clock_off))
            .route("/api/settings", get(get_settings).put(put_settings))
            .route("/api/device", get(get_device).put(put_device))
            .route("/api/room", get(get_room).put(put_room))
            .route("/api/sensors", get(get_sensors))
            .route("/api/bluetooth/status", get(get_bluetooth_status))
            .route("/api/bluetooth/power", put(put_bluetooth_power))
            .route("/api/bluetooth/pairable", put(put_bluetooth_pairable))
            .route("/api/bluetooth/scan", put(put_bluetooth_scan))
            .route("/api/bluetooth/:action", post(post_bluetooth_action))
            .route("/api/volume", get(get_volume).post(set_volume))
            .route("/api/volume/get", get(get_volume))
            .route("/api/volume/set", post(set_volume))
            .route("/api/volume/up", post(volume_up))
            .route("/api/volume/down", post(volume_down))
            .route("/api/volume/mute", post(mute_volume))
            .route("/api/volume/unmute", post(unmute_volume))
            .route("/api/audio/test", post(audio_test))
            .route("/api/microphone/devices", get(get_microphone_devices))
            .route("/api/microphone/record", post(record_microphone))
            .route("/api/alice/command", get(get_alice_command))
            .route("/api/alice/speech", get(get_alice_speech))
            .route("/api/alice/recognize", post(recognize_alice_speech))
            .route("/api/alice/speech/cancel", post(cancel_alice_speech))
            .route("/api/input/devices", get(get_input_devices))
            .route("/api/input/event", get(get_input_event))
            .route("/api/input/events", get(get_input_events))
            .route("/api/input/gpio/:input", get(get_gpio_input))
            .route("/api/input/swipe/:gesture", get(get_input_gesture))
            .route("/api/input/:action", get(get_input_action))
            .route("/api/clock", get(get_clock).post(post_clock))
            .route("/api/clock/off", post(clock_off))
            .route("/api/clock/text", post(post_clock_text))
            .route("/api/led", post(led_set))
            .route("/api/led/ring", post(led_set_ring))
            .route("/api/led/logo", post(led_set_logo))
            .route("/api/led/off", post(led_off))
            .route("/api/led/off/ring", post(led_ring_off))
            .route("/api/led/off/logo", post(led_logo_off));
    }
    let router = router
        .layer(DefaultBodyLimit::max(67 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(app.clone());
    let l = TcpListener::bind(&args.listen).await?;
    tracing::info!("web UI: http://{}", args.listen);
    axum::serve(l, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            stop_all_packages(&app).await;
        })
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn stop_all_packages(app: &App) {
    if let Err(error) = stop_clock_text() {
        tracing::warn!(%error, "failed to release clock text during shutdown");
    }
    let running = {
        let mut plugins = app.plugins.lock().await;
        plugins.shutting_down = true;
        plugins.running.values().cloned().collect::<Vec<_>>()
    };
    for package in &running {
        package.cancelled.store(true, Ordering::Relaxed);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    for package in running {
        if let Err(error) = package.stop(deadline.saturating_duration_since(tokio::time::Instant::now())).await {
            tracing::warn!(%error, "package cleanup incomplete during shutdown");
        }
    }
}
const UI: &str = include_str!("../web/index.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_clock_request_does_not_cancel_overlay() {
        let before = CLOCK_TEXT_EPOCH.load(Ordering::SeqCst);
        for request in [
            ClockRequest { time: None, brightness: None, segments: None },
            ClockRequest { time: Some("invalid".into()), brightness: None, segments: None },
            ClockRequest { time: None, brightness: None, segments: Some(HashMap::from([("INVALID".into(), 100)])) },
        ] {
            assert!(put_clock_request(None, request).is_err());
            assert_eq!(CLOCK_TEXT_EPOCH.load(Ordering::SeqCst), before);
        }
    }

    #[test]
    fn clock_overlay_duration_is_bounded() {
        for duration_ms in [0, 15001, u64::MAX] {
            assert!(validate_clock_text(&ClockTextRequest { text: "HI".into(), brightness: 100, duration_ms }).is_err());
        }
        for duration_ms in [1, 99, 2000, 15000] {
            assert!(validate_clock_text(&ClockTextRequest { text: "HI".into(), brightness: 100, duration_ms }).is_ok());
        }
    }

    #[test]
    fn text_capability_requires_supported_layout_and_no_station_backend() {
        assert!(clock_text_available(false, Some(&ClockLayout::Cucumber("test".into()))));
        assert!(!clock_text_available(true, Some(&ClockLayout::Cucumber("test".into()))));
        assert!(!clock_text_available(false, Some(&ClockLayout::Mini2("test".into()))));
        assert!(!clock_text_available(false, Some(&ClockLayout::Plum("test".into()))));
        assert!(!clock_text_available(false, None));
    }

    #[test]
    fn ipc_decodes_speech_and_alarm() {
        let mut state = Vec::new();
        push_bytes(&mut state, 2, b"hi");
        let mut request = Vec::new();
        push_varint(&mut request, 2 << 3);
        push_varint(&mut request, 1);
        push_bytes(&mut state, 15, &request);
        let mut speech = Vec::new();
        push_bytes(&mut speech, 3, &state);
        assert_eq!(
            decode_ipc_event("aliced", &speech).unwrap().unwrap()["transcript"],
            "hi"
        );
        let alarm = [
            0xba, 0x0d, 0x08, 0x0a, 0x06, 0x0a, 0x02, b'a', b'1', 0x10, 0x02,
        ];
        assert_eq!(
            decode_ipc_event("alarmd", &alarm).unwrap().unwrap()["id"],
            "a1"
        );
    }

    #[test]
    fn ipc_decodes_local_command_spotter_event() {
        let mut command = Vec::new();
        push_varint(&mut command, 1 << 3);
        push_varint(&mut command, 2);
        push_bytes(&mut command, 3, "громче".as_bytes());
        let mut remoting = Vec::new();
        push_bytes(&mut remoting, 7, b"CommandSpotterCapability");
        push_bytes(&mut remoting, 15, &command);
        let mut message = Vec::new();
        push_bytes(&mut message, 210, &remoting);
        let event = decode_ipc_event("aliced", &message).unwrap().unwrap();
        assert_eq!(event["source"], "command");
        assert_eq!(event["transcript"], "громче");
    }

    #[test]
    fn ipc_frame_length_uses_signed_10_mib_bound() {
        assert_eq!(
            ipc_frame_length((IPC_MAX_FRAME_SIZE as i32).to_le_bytes()).unwrap(),
            IPC_MAX_FRAME_SIZE
        );
        assert!(ipc_frame_length((-1i32).to_le_bytes()).is_err());
        assert!(ipc_frame_length(((IPC_MAX_FRAME_SIZE + 1) as i32).to_le_bytes()).is_err());
    }

    #[test]
    fn alice_cancel_uses_remoting_envelope() {
        let mut method = Vec::new();
        push_varint(&mut method, 1 << 3);
        push_varint(&mut method, 3);
        let mut remoting = Vec::new();
        push_bytes(&mut remoting, 7, b"AliceCapability");
        push_bytes(&mut remoting, 8, &method);
        let mut message = Vec::new();
        push_bytes(&mut message, 210, &remoting);
        let remoting = nested_field(&message, 210).unwrap().unwrap();
        assert_eq!(
            text_field(&remoting, 7).unwrap().as_deref(),
            Some("AliceCapability")
        );
        let method = nested_field(&remoting, 8).unwrap().unwrap();
        assert_eq!(varint_field(&method, 1).unwrap(), Some(3));
    }

    #[test]
    fn voice_command_matches_alice_prefix_and_normalizes_text() {
        assert!(command_matches("ананас", "Алиса, ананас!"));
        assert!(command_matches("ананас", "ананас"));
        assert!(!command_matches("ананас", "алиса банан"));
    }

    #[test]
    fn model_names_are_normalized_for_package_compatibility() {
        assert_eq!(canonical_model("Yandex Micro"), "yandex micro");
        assert_eq!(canonical_model(" yandexmini_2 "), "yandexmini_2");
    }

    #[test]
    fn availability_reports_device_capabilities_but_not_exec() {
        let device = DeviceProfile {
            model: "yandexmicro".into(),
            architecture: "aarch64".into(),
            features: vec!["led.ring.rgb".into(), "audio.play.mediad".into()],
            source: "test".into(),
            led: LedTopology::default(),
        };
        assert!(lua_api_available(&device, "led").unwrap());
        assert!(lua_api_available(&device, "audio").unwrap());
        assert!(!lua_api_available(&device, "bluetooth").unwrap());
        assert!(lua_api_available(&device, "exec").is_err());
    }

    #[test]
    fn led_colors_are_expanded_evenly_across_a_zone() {
        assert_eq!(
            expand_colors(vec!["a".into()], 6, "ring").unwrap(),
            vec!["a"; 6]
        );
        assert_eq!(
            expand_colors(vec!["a".into(), "b".into()], 6, "ring").unwrap(),
            vec!["a", "a", "a", "b", "b", "b"]
        );
        assert_eq!(
            expand_colors(vec!["a".into(), "b".into(), "c".into()], 8, "ring").unwrap(),
            vec!["a", "a", "a", "b", "b", "b", "c", "c"]
        );
        assert!(expand_colors(Vec::new(), 6, "ring").is_err());
        assert!(expand_colors(vec!["a".into(); 7], 6, "ring").is_err());
    }

    #[test]
    fn network_policy_blocks_loopback() {
        let policy = NetworkPolicy {
            allow: vec!["*".into()],
            allow_insecure_http: true,
        };
        assert!(network_url_allowed(&policy, "http://127.0.0.1/").is_err());
    }

    #[test]
    fn input_events_decode_for_both_linux_word_sizes() {
        let mut event64 = vec![];
        event64.extend(10i64.to_ne_bytes());
        event64.extend(2_000i64.to_ne_bytes());
        event64.extend(1u16.to_ne_bytes());
        event64.extend(2u16.to_ne_bytes());
        event64.extend((-1i32).to_ne_bytes());
        assert_eq!(decode_input_event("mute", &event64, 8).unwrap().value, -1);

        let mut event32 = vec![];
        event32.extend(10i32.to_ne_bytes());
        event32.extend(2_000i32.to_ne_bytes());
        event32.extend(3u16.to_ne_bytes());
        event32.extend(4u16.to_ne_bytes());
        event32.extend(5i32.to_ne_bytes());
        assert_eq!(
            decode_input_event("mute", &event32, 4).unwrap().time_ms,
            10_002
        );
        assert!(decode_input_event("mute", &event32[..15], 4).is_err());
    }

    #[test]
    fn input_scope_is_limited_to_declared_devices() {
        let manifest: Manifest = toml::from_str(
            "id='p'\nname='p'\nversion='1'\nentry='main.lua'\npermissions=['input']\n[input]\ndevices=['mute']\n",
        )
        .unwrap();
        assert!(validate_input_policy(&manifest).is_ok());
        let devices = resolve_input_devices(
            "N: Name=\"mute\"\nH: Handlers=event2\n\nN: Name=\"other\"\nH: Handlers=event3\n",
            input_scope(&manifest).unwrap(),
        )
        .unwrap();
        assert_eq!(
            devices,
            vec![("mute".into(), PathBuf::from("/dev/input/event2"))]
        );
    }

    #[test]
    fn yam_ids_are_strictly_namespaced() {
        assert!(validate_yam_id("vasya.clock").is_ok());
        assert!(validate_yam_id("vasya.clock.extra").is_err());
        assert!(validate_yam_id("Vasya.clock").is_err());
        assert!(validate_id("vasya.clock").is_err());
    }

    #[test]
    fn yam_root_is_detected_from_packaged_executable_path() {
        assert_eq!(
            yam_root_from_executable(Path::new("/data/pkgs/yam/luna/0.3.0/bin/luna")),
            Some(PathBuf::from("/data"))
        );
        assert!(yam_root_from_executable(Path::new("/data/bin/luna")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn yam_plugin_is_listed_through_share_symlinks() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("luna-yam-test-{}", Uuid::new_v4()));
        let yam_root = root.join("yam");
        let package = yam_root.join("pkgs/vasya/clock/1.0.0/share/luna/plugins/vasya.clock");
        let shared = yam_root.join("share/luna/plugins/vasya.clock");
        fs::create_dir_all(&package).unwrap();
        fs::create_dir_all(shared.parent().unwrap()).unwrap();
        fs::write(
            package.join("plugin.toml"),
            "id='vasya.clock'\nname='Clock'\nversion='1.0.0'\nentry='main.lua'\n",
        )
        .unwrap();
        fs::write(package.join("main.lua"), "return 1").unwrap();
        fs::create_dir(&shared).unwrap();
        symlink(package.join("plugin.toml"), shared.join("plugin.toml")).unwrap();
        symlink(package.join("main.lua"), shared.join("main.lua")).unwrap();

        let manager = PluginManager::new(root.join("data/plugins"), Some(yam_root), None).unwrap();
        let device = DeviceProfile {
            model: "test".into(),
            architecture: "aarch64".into(),
            features: vec![],
            source: "test".into(),
            led: LedTopology::default(),
        };
        let plugins = manager.list(&device).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.id, "vasya.clock");
        assert_eq!(plugins[0].source, PluginSource::Yam);
        assert_eq!(
            fs::read_to_string(plugins[0].directory.join("main.lua")).unwrap(),
            "return 1"
        );
        assert_eq!(
            manager.state_root("vasya.clock"),
            root.join("data/plugins/plugin-state/vasya.clock")
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod lifecycle_tests {
    use super::*;

    async fn fixture() -> (App, PathBuf) {
        let root = std::env::temp_dir().join(format!("luna-lifecycle-{}", Uuid::new_v4()));
        let package = root.join("plugins/test-service");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("plugin.toml"), "id='test-service'\nname='Test'\nversion='1.0.0'\nentry='run.sh'\npermissions=['exec']\n[launch]\nkind='binary'\npath='run.sh'\ninterpreter='sh'\n").unwrap();
        fs::write(package.join("run.sh"), "if [ -f ready ] && [ ! -f cleaned ]; then touch overlap; fi\nrm -f cleaned\ntrap 'sleep 0.2; touch cleaned; exit 0' TERM\ntouch ready\nwhile :; do sleep 0.02; done\n").unwrap();
        let mut manager = PluginManager::new(root.join("plugins"), None, None).unwrap();
        manager.enabled.insert("test-service".into());
        let mut settings = Settings::default();
        settings.allow_plugin_binaries = true;
        let app = App {
            root, settings: Arc::new(Mutex::new(settings)),
            device: Arc::new(Mutex::new(DeviceProfile { model: "test".into(), architecture: "x86_64".into(), features: vec![], source: "test".into(), led: LedTopology::default() })),
            led: Arc::new(Led { settings: Arc::new(Mutex::new(LedSettings::default())), backend: None, color_order: ColorOrder::Rgb, topology: LedTopology::default() }),
            microphone: None, clock: None, plugins: Arc::new(Mutex::new(manager)),
            runtime_tokens: Arc::new(Mutex::new(HashMap::new())), speech_readers: Arc::new(Mutex::new(HashMap::new())), speech_claims: Arc::new(Mutex::new(HashMap::new())),
            providers: Arc::new(Mutex::new(providers::Broker::default())),
            custom_web: Arc::new(Mutex::new(HashMap::new())),
            runtime_api_url: "http://127.0.0.1:1".into(), input_actions: &[], input_gestures: &[], gpio_inputs: &[],
        };
        start_package(app.clone(), "test-service".into(), vec![]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !package.join("ready").exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
        (app, package)
    }

    #[tokio::test]
    async fn stop_endpoint_waits_for_recovery() {
        let (app, package) = fixture().await;
        stop_plugin(State(app.clone()), AxumPath("test-service".into())).await.unwrap();
        let cleaned = package.join("cleaned").exists();
        tokio::time::sleep(Duration::from_secs(1)).await;
        fs::remove_dir_all(&app.root).unwrap();
        assert!(cleaned, "stop endpoint returned before supervisor recovery");
    }
    #[tokio::test]
    async fn restart_waits_for_previous_recovery() {
        let (app, package) = fixture().await;
        start_package(app.clone(), "test-service".into(), vec![]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop_plugin(State(app.clone()), AxumPath("test-service".into())).await.unwrap();
        assert!(!package.join("overlap").exists(), "replacement started before old recovery");
        assert!(package.join("cleaned").exists());
        fs::remove_dir_all(&app.root).unwrap();
    }

    #[tokio::test]
    async fn shutdown_waits_for_recovery_and_rejects_restart() {
        let (app, package) = fixture().await;
        stop_all_packages(&app).await;
        assert!(package.join("cleaned").exists());
        assert!(app.plugins.lock().await.running.is_empty());
        assert!(app.runtime_tokens.lock().await.is_empty());
        assert!(start_package(app.clone(), "test-service".into(), vec![]).await.is_err());
        fs::remove_dir_all(&app.root).unwrap();
    }

    #[tokio::test]
    async fn incomplete_cleanup_times_out_and_remains_cancelled() {
        let (_finished, receiver) = tokio::sync::watch::channel(false);
        let run = RunningPackage { cancelled: Arc::new(AtomicBool::new(false)), finished: receiver };
        assert!(run.stop(Duration::from_millis(20)).await.is_err());
        assert!(run.cancelled.load(Ordering::Relaxed));
        assert!(!*run.finished.borrow());
    }

    #[tokio::test]
    async fn concurrent_restarts_do_not_overlap() {
        let (app, package) = fixture().await;
        let (first, second) = tokio::join!(
            start_package(app.clone(), "test-service".into(), vec![]),
            start_package(app.clone(), "test-service".into(), vec![]),
        );
        first.unwrap();
        second.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop_plugin(State(app.clone()), AxumPath("test-service".into())).await.unwrap();
        assert!(!package.join("overlap").exists(), "concurrent replacements overlapped");
        fs::remove_dir_all(&app.root).unwrap();
    }

    async fn yam_binary_fixture() -> (App, PathBuf, PathBuf) {
        use std::os::unix::fs::symlink;
        let (app, _) = fixture().await;
        stop_plugin(State(app.clone()), AxumPath("test-service".into())).await.unwrap();
        let yam = app.root.join("yam");
        let run = app.root.join("yam-run");
        let selected = yam.join("pkgs/example/worker/1.0.0");
        let package = selected.join("share/luna/plugins/example.worker");
        let shared = yam.join("share/luna/plugins/example.worker");
        fs::create_dir_all(&package).unwrap();
        fs::create_dir_all(&shared).unwrap();
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join("yam.lock"), "").unwrap();
        symlink("1.0.0", selected.parent().unwrap().join("current")).unwrap();
        fs::write(package.join("plugin.toml"), "id='example.worker'\nname='Worker'\nversion='1.0.0'\nentry='run.sh'\npermissions=['exec']\n[launch]\nkind='binary'\ninterpreter='sh'\n").unwrap();
        fs::write(package.join("run.sh"), "trap 'sleep 0.2; touch \"$LUNA_PLUGIN_STATE/cleaned\"; exit 0' TERM\npwd > \"$LUNA_PLUGIN_STATE/cwd\"\nprintf '%s' \"$LUNA_PLUGIN_ID\" > \"$LUNA_PLUGIN_STATE/id\"\ntouch \"$LUNA_PLUGIN_STATE/ready\"\nwhile :; do sleep 0.02; done\n").unwrap();
        symlink(package.join("plugin.toml"), shared.join("plugin.toml")).unwrap();
        // Execution uses the pinned package, not a common-tree executable link.
        let state = app.plugins.lock().await.state_root("example.worker");
        let mut manager = app.plugins.lock().await;
        manager.yam_root = Some(yam);
        manager.yam_run = Some(run.clone());
        manager.enabled.insert("example.worker".into());
        drop(manager);
        (app, state, run)
    }

    fn yam_exclusive_available(run: &Path) -> bool {
        use std::os::fd::AsRawFd;
        let file = fs::File::open(run.join("yam.lock")).unwrap();
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    }

    #[tokio::test]
    async fn yam_binary_lifecycle_pins_payload_and_authorizes_namespaced_runtime() {
        let (app, state, run) = yam_binary_fixture().await;
        let device = app.device.lock().await.clone();
        let visible = app.plugins.lock().await.list(&device).unwrap();
        let plugin = visible.iter().find(|p| p.manifest.id == "example.worker").unwrap();
        assert!(plugin.compatible);
        assert_eq!(plugin.source, PluginSource::Yam);
        start_package(app.clone(), "example.worker".into(), vec![]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !state.join("ready").exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
        assert!(!yam_exclusive_available(&run));
        assert_eq!(fs::read_to_string(state.join("id")).unwrap(), "example.worker");
        let cwd = fs::read_to_string(state.join("cwd")).unwrap();
        assert!(cwd.trim().ends_with("pkgs/example/worker/1.0.0/share/luna/plugins/example.worker"));
        let token = app.runtime_tokens.lock().await.get("example.worker").unwrap().clone();
        let mut headers = HeaderMap::new();
        headers.insert("x-luna-token", token.parse().unwrap());
        runtime_permission(&app, "example.worker", &headers, "exec").await.unwrap();
        assert!(runtime_permission(&app, "example.worker", &headers, "sensors").await.is_err());
        assert!(runtime_permission(&app, "example.other", &headers, "exec").await.is_err());
        stop_plugin(State(app.clone()), AxumPath("example.worker".into())).await.unwrap();
        assert!(state.join("cleaned").exists());
        assert!(yam_exclusive_available(&run));
        assert!(runtime_permission(&app, "example.worker", &headers, "exec").await.is_err());
        package_logs(State(app.clone()), AxumPath("example.worker".into())).await.unwrap();
        fs::remove_dir_all(&app.root).unwrap();
    }

    #[tokio::test]
    async fn yam_binary_still_requires_enable_and_admin_opt_in() {
        let (app, state, run) = yam_binary_fixture().await;
        app.settings.lock().await.allow_plugin_binaries = false;
        assert!(start_package(app.clone(), "example.worker".into(), vec![]).await.is_err());
        app.settings.lock().await.allow_plugin_binaries = true;
        app.plugins.lock().await.enabled.remove("example.worker");
        assert!(start_package(app.clone(), "example.worker".into(), vec![]).await.is_err());
        assert!(!state.join("ready").exists());
        assert!(yam_exclusive_available(&run));
        assert!(app.runtime_tokens.lock().await.get("example.worker").is_none());
        fs::remove_dir_all(&app.root).unwrap();
    }

    #[tokio::test]
    async fn yam_binary_failed_exit_releases_lease_and_runtime_token() {
        let (app, _state, run) = yam_binary_fixture().await;
        let script = app.root.join("yam/pkgs/example/worker/1.0.0/share/luna/plugins/example.worker/run.sh");
        fs::write(script, "exit 7\n").unwrap();
        start_package(app.clone(), "example.worker".into(), vec![]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.plugins.lock().await.running.contains_key("example.worker") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert!(yam_exclusive_available(&run));
        assert!(app.runtime_tokens.lock().await.get("example.worker").is_none());
        fs::remove_dir_all(&app.root).unwrap();
    }

    #[tokio::test]
    async fn yam_dependency_resolution_is_declared_enabled_compatible_and_nonexecuting() {
        use std::os::unix::fs::symlink;
        let (app, state, run) = yam_binary_fixture().await;
        let caller = app.root.join("yam/pkgs/example/worker/1.0.0/share/luna/plugins/example.worker/plugin.toml");
        let mut text = fs::read_to_string(&caller).unwrap();
        text.push_str("\n[dependencies]\nstation-volume='station.volume'\n");
        fs::write(&caller, text).unwrap();
        let selected = app.root.join("yam/pkgs/station/volume/1.0.2");
        let root = selected.join("share/luna/plugins/station.volume");
        let shared = app.root.join("yam/share/luna/plugins/station.volume");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&shared).unwrap();
        symlink("1.0.2", selected.parent().unwrap().join("current")).unwrap();
        let descriptor = "id='station.volume'\nname='Volume'\nversion='1.0.2'\nentry='station-volume.sh'\npermissions=['exec']\n[launch]\nkind='binary'\ninterpreter='sh'\n";
        fs::write(root.join("plugin.toml"), descriptor).unwrap();
        fs::write(root.join("station-volume.sh"), "touch unexpectedly-executed\n").unwrap();
        symlink(root.join("plugin.toml"), shared.join("plugin.toml")).unwrap();
        app.plugins.lock().await.enabled.insert("station.volume".into());
        start_package(app.clone(), "example.worker".into(), vec![]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !state.join("ready").exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
        let token = app.runtime_tokens.lock().await.get("example.worker").unwrap().clone();
        let mut headers = HeaderMap::new();
        headers.insert("x-luna-token", token.parse().unwrap());
        let resolve = |alias: &str| dependencies::resolve(State(app.clone()), AxumPath(("example.worker".into(), alias.into())), headers.clone());
        assert!(resolve("undeclared").await.is_err());
        let Json(launcher) = resolve("station-volume").await.unwrap();
        assert_eq!(launcher.plugin_id, "station.volume");
        assert_eq!(launcher.program, root.join("station-volume.sh"));
        assert_eq!(launcher.interpreter.as_deref(), Some("sh"));
        assert!(!root.join("unexpectedly-executed").exists());
        assert!(!yam_exclusive_available(&run));
        app.plugins.lock().await.enabled.remove("station.volume");
        assert!(resolve("station-volume").await.is_err());
        app.plugins.lock().await.enabled.insert("station.volume".into());
        fs::write(root.join("plugin.toml"), format!("{descriptor}\n[compatibility]\nmodels=['not-this-device']\n")).unwrap();
        assert!(resolve("station-volume").await.is_err());
        fs::write(root.join("plugin.toml"), format!("{descriptor}mode='service'\n")).unwrap();
        assert!(resolve("station-volume").await.is_err());
        fs::write(root.join("plugin.toml"), descriptor).unwrap();
        stop_plugin(State(app.clone()), AxumPath("example.worker".into())).await.unwrap();
        assert!(resolve("station-volume").await.is_err());
        assert!(yam_exclusive_available(&run));
        fs::remove_dir_all(&app.root).unwrap();
    }

}
