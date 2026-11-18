//! Opt-in anonymous usage statistics.
//!
//! Nothing leaves the station until the owner answers the consent prompt in the
//! web panel or the installer, and the answer lives in `telemetry.toml` next to
//! `settings.toml`. A separate file keeps the installer able to record a choice
//! without materialising default settings, which would otherwise pin the LED
//! defaults of a station build.
//!
//! The payload describes the station, never the person: no serial numbers, host
//! names, room text, file names, plugin settings or addresses end up in it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Collector that receives every report.
pub(crate) const ENDPOINT: &str = "https://stats.x-servers.ru/v1/luna/events";
/// `LUNA_TELEMETRY_ENDPOINT` points a build at a local collector for testing.
const ENDPOINT_ENV: &str = "LUNA_TELEMETRY_ENDPOINT";
const STATE_FILE: &str = "telemetry.toml";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// First report waits for the runtime to settle; the rest follow hourly.
pub(crate) const STARTUP_DELAY: Duration = Duration::from_secs(60);
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Owner's answer to the consent prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Consent {
    /// Never asked; the panel shows the first-run prompt.
    #[default]
    Unset,
    Granted,
    Declined,
}

impl Consent {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Consent::Unset => "unset",
            Consent::Granted => "granted",
            Consent::Declined => "declined",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub consent: Consent,
    /// Random identifier, generated on the first opt-in; it is not derived from
    /// any hardware or account detail.
    #[serde(default)]
    pub install_id: String,
}

impl State {
    pub(crate) fn path(root: &Path) -> PathBuf {
        root.join(STATE_FILE)
    }

    /// A missing or unreadable file means "not asked yet", the safe default.
    pub(crate) fn load(root: &Path) -> Self {
        let path = Self::path(root);
        match fs::read_to_string(&path) {
            Ok(raw) => match toml::from_str::<Self>(&raw) {
                Ok(state) => state,
                Err(error) => {
                    tracing::warn!(%error, path=%path.display(), "invalid telemetry.toml; statistics stay off");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub(crate) fn save(&self, root: &Path) -> Result<()> {
        fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
        let path = Self::path(root);
        let temporary = root.join(format!(".telemetry-{}.tmp", uuid::Uuid::new_v4()));
        fs::write(&temporary, toml::to_string_pretty(self)?)
            .with_context(|| format!("write {}", temporary.display()))?;
        fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))
    }

    /// Records an explicit owner decision. Disabling keeps the identifier so a
    /// later re-enable stays the same anonymous installation instead of
    /// counting as a new station.
    pub(crate) fn record(root: &Path, enabled: bool) -> Result<Self> {
        let mut state = Self::load(root);
        state.enabled = enabled;
        state.consent = if enabled {
            Consent::Granted
        } else {
            Consent::Declined
        };
        if enabled && state.install_id.is_empty() {
            state.install_id = uuid::Uuid::new_v4().to_string();
        }
        state.save(root)?;
        Ok(state)
    }
}

/// `LUNA_TELEMETRY_ENDPOINT` wins so tests and self-hosted collectors work
/// without recompiling the runtime.
pub(crate) fn endpoint() -> String {
    std::env::var(ENDPOINT_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| value.starts_with("http://") || value.starts_with("https://"))
        .unwrap_or_else(|| ENDPOINT.to_owned())
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Builds the report envelope; `body` supplies the event-specific fields.
pub(crate) fn envelope(state: &State, event: &str, body: Value) -> Value {
    let mut report = Map::new();
    report.insert("schema".into(), json!(1));
    report.insert("event".into(), json!(event));
    report.insert("install_id".into(), json!(state.install_id));
    report.insert("version".into(), json!(env!("CARGO_PKG_VERSION")));
    report.insert("sent_at".into(), json!(unix_now()));
    if let Value::Object(fields) = body {
        for (key, value) in fields {
            report.insert(key, value);
        }
    }
    Value::Object(report)
}

/// Delivers one report. Blocking on purpose: callers run it on a blocking task
/// or from the `telemetry` command, never on the async runtime thread.
pub(crate) fn send(state: &State, event: &str, body: Value) -> Result<()> {
    if !state.enabled {
        bail!("anonymous statistics are disabled");
    }
    if state.install_id.is_empty() {
        bail!("anonymous statistics have no install identifier");
    }
    let report = envelope(state, event, body);
    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("luna/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build statistics client")?;
    let response = client
        .post(endpoint())
        .json(&report)
        .send()
        .context("send statistics")?;
    if !response.status().is_success() {
        bail!("statistics endpoint answered {}", response.status());
    }
    Ok(())
}

/// Counts warnings and errors so each report can carry the runtime's health.
#[derive(Clone, Default)]
pub(crate) struct Counters {
    warnings: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
}

impl Counters {
    pub(crate) fn warnings(&self) -> u64 {
        self.warnings.load(Ordering::Relaxed)
    }

    pub(crate) fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }
}

impl<S> tracing_subscriber::Layer<S> for Counters
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let counter = match *event.metadata().level() {
            tracing::Level::ERROR => &self.errors,
            tracing::Level::WARN => &self.warnings,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Host-level figures that say something about the fleet without identifying a
/// station: kernel, memory, load, CPU temperature and uptime.
pub(crate) fn system_info(started: Instant) -> Value {
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "kernel": first_line("/proc/sys/kernel/osrelease"),
        "uptime_s": system_uptime(),
        "luna_uptime_s": started.elapsed().as_secs(),
        "mem_total_kb": meminfo_kb("MemTotal"),
        "mem_available_kb": meminfo_kb("MemAvailable"),
        "load1": load_average(),
        "cpu_temp_c": cpu_temperature(),
    })
}

fn first_line(path: &str) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let line = raw.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_owned())
}

fn system_uptime() -> Option<u64> {
    let raw = fs::read_to_string("/proc/uptime").ok()?;
    let seconds = raw.split_whitespace().next()?.parse::<f64>().ok()?;
    u64::try_from(seconds as i64).ok()
}

fn meminfo_kb(field: &str) -> Option<u64> {
    let raw = fs::read_to_string("/proc/meminfo").ok()?;
    raw.lines().find_map(|line| {
        line.strip_prefix(field)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

fn load_average() -> Option<f64> {
    let raw = fs::read_to_string("/proc/loadavg").ok()?;
    raw.split_whitespace().next()?.parse().ok()
}

fn cpu_temperature() -> Option<f64> {
    let zones = fs::read_dir("/sys/class/thermal").ok()?;
    for zone in zones.flatten() {
        let path = zone.path().join("temp");
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(milli) = raw.trim().parse::<i64>() {
            return Some(milli as f64 / 1000.0);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("luna-telemetry-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn consent_is_opt_in_and_sticky() {
        let root = temp_root();
        let fresh = State::load(&root);
        assert!(!fresh.enabled);
        assert_eq!(fresh.consent, Consent::Unset);
        assert!(fresh.install_id.is_empty());

        let granted = State::record(&root, true).unwrap();
        assert!(granted.enabled);
        assert_eq!(granted.consent, Consent::Granted);
        assert_eq!(granted.install_id.len(), 36);

        let declined = State::record(&root, false).unwrap();
        assert!(!declined.enabled);
        assert_eq!(declined.consent, Consent::Declined);
        assert_eq!(declined.install_id, granted.install_id);
        assert_eq!(State::load(&root).consent, Consent::Declined);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn disabled_state_never_sends_or_opens_a_socket() {
        let root = temp_root();
        let state = State::record(&root, false).unwrap();
        let error = send(&state, "startup", json!({ "device": {} })).unwrap_err();
        assert!(error.to_string().contains("disabled"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn envelope_keeps_identity_and_event_fields() {
        let state = State {
            enabled: true,
            consent: Consent::Granted,
            install_id: "7ab0f8a0-0000-4000-8000-000000000000".into(),
        };
        let report = envelope(
            &state,
            "heartbeat",
            json!({ "device": { "model": "yandexmini3" } }),
        );
        assert_eq!(report["schema"], 1);
        assert_eq!(report["event"], "heartbeat");
        assert_eq!(report["install_id"], state.install_id);
        assert_eq!(report["device"]["model"], "yandexmini3");
        assert!(report["sent_at"].as_u64().is_some_and(|value| value > 0));
    }

    #[test]
    fn toml_round_trip_uses_lowercase_consent() {
        let state = State {
            enabled: true,
            consent: Consent::Declined,
            install_id: "abc".into(),
        };
        fs::create_dir_all(temp_root()).ok();
        let encoded = toml::to_string_pretty(&state).unwrap();
        assert!(encoded.contains("consent = \"declined\""));
        let decoded: State = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.consent, Consent::Declined);
        assert!(decoded.enabled);
    }
}
