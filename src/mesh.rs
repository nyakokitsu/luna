//! Local station mesh: authenticated UDP discovery with an optional main panel.
//!
//! Every station broadcasts a signed beacon; peers verify the Ed25519 signature
//! with the public key carried in the beacon and pin it to the station id on
//! first sight. Discovery never depends on server registration: a station that
//! was never registered still announces itself and still accepts peers.
//!
//! `main` names the station whose panel manages the mesh. `None` means this
//! station is the main one; `Some(id)` makes the panel point at that station
//! and show the managed-through notice.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use crate::identity;

const CONFIG_FILE: &str = "mesh.toml";
const PORT_ENV: &str = "LUNA_MESH_PORT";
pub(crate) const DEFAULT_PORT: u16 = 8097;
pub(crate) const BEACON_INTERVAL: Duration = Duration::from_secs(5);
pub(crate) const PEER_TTL: Duration = Duration::from_secs(20);
const MAX_CLOCK_SKEW_SECONDS: u64 = 120;
const PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Config {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_name")]
    pub name: String,
    /// `None` means this station is the main panel of the mesh.
    #[serde(default)]
    pub main: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            name: default_name(),
            main: None,
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_name() -> String {
    "luna".into()
}

impl Config {
    pub(crate) fn path(root: &Path) -> PathBuf {
        root.join(CONFIG_FILE)
    }

    /// A missing file keeps the defaults; mesh discovery needs no setup.
    pub(crate) fn load(root: &Path) -> Self {
        let path = Self::path(root);
        match std::fs::read_to_string(&path) {
            Ok(raw) => match toml::from_str::<Self>(&raw) {
                Ok(config) => config.normalized(),
                Err(error) => {
                    tracing::warn!(%error, path=%path.display(), "invalid mesh.toml; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    fn normalized(mut self) -> Self {
        self.name = self.name.trim().to_owned();
        if !valid_name(&self.name) {
            self.name = default_name();
        }
        if self
            .main
            .as_deref()
            .is_some_and(|main| !identity::valid_station_id(main))
        {
            self.main = None;
        }
        self
    }

    pub(crate) fn save(&self, root: &Path) -> Result<()> {
        let config = self.clone().normalized();
        std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
        let temporary = root.join(format!(".mesh-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temporary, toml::to_string_pretty(&config)?)
            .with_context(|| format!("write {}", temporary.display()))?;
        std::fs::rename(&temporary, Self::path(root))
            .with_context(|| format!("replace {}", Self::path(root).display()))
    }
}

pub(crate) fn valid_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && name.chars().count() <= 32 && !name.chars().any(char::is_control)
}

/// A recently seen station on the local network.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Peer {
    pub station_id: String,
    pub model: String,
    pub ip: String,
    pub port: u16,
    /// The peer's own `main`: `None` means the peer is a main station.
    pub main: Option<String>,
    pub public_key: String,
    pub last_seen: u64,
}

impl Peer {
    pub(crate) fn url(&self) -> String {
        format!("http://{}:{}", self.ip, self.port)
    }
}

pub(crate) type Peers = Arc<Mutex<HashMap<String, Peer>>>;

pub(crate) fn new_peers() -> Peers {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Fresh peers, sorted by station id so the panel does not reshuffle them.
pub(crate) async fn snapshot(peers: &Peers) -> Vec<Peer> {
    let now = identity::unix_now();
    let ttl = PEER_TTL.as_secs();
    let mut peers = peers.lock().await;
    peers.retain(|_, peer| now.saturating_sub(peer.last_seen) <= ttl);
    let mut list = peers.values().cloned().collect::<Vec<_>>();
    list.sort_by(|left, right| left.station_id.cmp(&right.station_id));
    list
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Beacon {
    v: u8,
    mesh: String,
    station_id: String,
    public_key: String,
    model: String,
    port: u16,
    #[serde(default)]
    main: Option<String>,
    ts: u64,
    signature: String,
}

impl Beacon {
    fn payload(&self) -> String {
        payload(
            &self.mesh,
            &self.station_id,
            &self.public_key,
            self.port,
            &self.model,
            self.main.as_deref(),
            self.ts,
        )
    }
}

/// Canonical signed bytes. `-` stands in for an absent main station so the
/// separator stays unambiguous.
fn payload(
    mesh: &str,
    station_id: &str,
    public_key: &str,
    port: u16,
    model: &str,
    main: Option<&str>,
    ts: u64,
) -> String {
    format!(
        "luna-mesh-v1|{mesh}|{station_id}|{public_key}|{port}|{model}|{}|{ts}",
        main.unwrap_or("-")
    )
}

/// Validates and verifies one datagram. Used by the receiver and unit tests.
fn parse_beacon(raw: &[u8], now: u64, expected_mesh: &str) -> Result<Beacon> {
    let beacon: Beacon = serde_json::from_slice(raw).context("mesh beacon is not valid JSON")?;
    if beacon.v != PROTOCOL_VERSION {
        bail!("unsupported mesh protocol version {}", beacon.v);
    }
    if beacon.mesh != expected_mesh {
        bail!("mesh name does not match");
    }
    if !identity::valid_station_id(&beacon.station_id) {
        bail!("invalid station id in mesh beacon");
    }
    if !identity::valid_public_key(&beacon.public_key) {
        bail!("invalid public key in mesh beacon");
    }
    if beacon.port == 0 {
        bail!("invalid web port in mesh beacon");
    }
    if beacon.model.is_empty()
        || beacon.model.chars().count() > 48
        || beacon.model.chars().any(char::is_control)
    {
        bail!("invalid model in mesh beacon");
    }
    if beacon
        .main
        .as_deref()
        .is_some_and(|main| !identity::valid_station_id(main))
    {
        bail!("invalid main station in mesh beacon");
    }
    if beacon.ts.abs_diff(now) > MAX_CLOCK_SKEW_SECONDS {
        bail!("mesh beacon timestamp is outside the accepted window");
    }
    if !identity::verify(&beacon.public_key, &beacon.payload(), &beacon.signature) {
        bail!("invalid mesh beacon signature");
    }
    Ok(beacon)
}

async fn remember(peers: &Peers, beacon: &Beacon, ip: String) {
    let now = identity::unix_now();
    let ttl = PEER_TTL.as_secs();
    let mut peers = peers.lock().await;
    peers.retain(|_, peer| now.saturating_sub(peer.last_seen) <= ttl);
    if let Some(existing) = peers.get(&beacon.station_id) {
        if existing.public_key != beacon.public_key {
            tracing::warn!(
                station = %beacon.station_id,
                "mesh peer changed its public key; ignoring the beacon"
            );
            return;
        }
    }
    peers.insert(
        beacon.station_id.clone(),
        Peer {
            station_id: beacon.station_id.clone(),
            model: beacon.model.clone(),
            ip,
            port: beacon.port,
            main: beacon.main.clone(),
            public_key: beacon.public_key.clone(),
            last_seen: now,
        },
    );
}

fn beacon_port() -> u16 {
    std::env::var(PORT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or(DEFAULT_PORT)
}

/// Starts discovery in the background. Errors are logged: a station without a
/// usable network socket still serves its panel.
pub(crate) fn start(
    identity: identity::State,
    model: String,
    web_port: u16,
    config: Arc<Mutex<Config>>,
    peers: Peers,
) {
    tokio::spawn(async move {
        if let Err(error) = run(identity, model, web_port, config, peers).await {
            tracing::warn!(%error, "mesh discovery is unavailable");
        }
    });
}

async fn run(
    identity: identity::State,
    model: String,
    web_port: u16,
    config: Arc<Mutex<Config>>,
    peers: Peers,
) -> Result<()> {
    let port = beacon_port();
    let socket = Arc::new(
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
            .await
            .with_context(|| format!("bind mesh discovery on UDP {port}"))?,
    );
    socket
        .set_broadcast(true)
        .context("enable broadcast for mesh discovery")?;

    let receiver = socket.clone();
    let receiver_peers = peers.clone();
    let receiver_config = config.clone();
    let receiver_identity = identity.clone();
    tokio::spawn(async move {
        let mut buffer = [0u8; 2048];
        loop {
            let (size, source) = match receiver.recv_from(&mut buffer).await {
                Ok(received) => received,
                Err(error) => {
                    tracing::warn!(%error, "mesh discovery receive failed");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
            let (enabled, name) = {
                let config = receiver_config.lock().await;
                (config.enabled, config.name.clone())
            };
            if !enabled {
                continue;
            }
            let now = identity::unix_now();
            let beacon = match parse_beacon(&buffer[..size], now, &name) {
                Ok(beacon) => beacon,
                Err(error) => {
                    tracing::debug!(%error, %source, "ignoring mesh datagram");
                    continue;
                }
            };
            if beacon.station_id == receiver_identity.station_id {
                continue;
            }
            remember(&receiver_peers, &beacon, source.ip().to_string()).await;
        }
    });

    let mut last_main = None;
    loop {
        let (enabled, name, main) = {
            let config = config.lock().await;
            (config.enabled, config.name.clone(), config.main.clone())
        };
        if enabled {
            if main != last_main {
                tracing::info!(main = ?main, "mesh role changed");
                last_main = main.clone();
            }
            let ts = identity::unix_now();
            let mut beacon = Beacon {
                v: PROTOCOL_VERSION,
                mesh: name,
                station_id: identity.station_id.clone(),
                public_key: identity.public_key.clone(),
                model: model.clone(),
                port: web_port,
                main,
                ts,
                signature: String::new(),
            };
            match identity.signature(&beacon.payload()) {
                Ok(signature) => beacon.signature = signature,
                Err(error) => {
                    tracing::warn!(%error, "mesh beacon cannot be signed");
                    tokio::time::sleep(BEACON_INTERVAL).await;
                    continue;
                }
            }
            if let Ok(raw) = serde_json::to_vec(&beacon) {
                if let Err(error) = socket.send_to(&raw, (Ipv4Addr::BROADCAST, port)).await {
                    tracing::debug!(%error, "mesh beacon was not broadcast");
                }
            }
        }
        tokio::time::sleep(BEACON_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("luna-mesh-{}", uuid::Uuid::new_v4()))
    }

    fn signed_beacon(state: &identity::State, main: Option<&str>, ts: u64) -> Vec<u8> {
        let mut beacon = Beacon {
            v: PROTOCOL_VERSION,
            mesh: "luna".into(),
            station_id: state.station_id.clone(),
            public_key: state.public_key.clone(),
            model: "yandexmini".into(),
            port: 8099,
            main: main.map(str::to_owned),
            ts,
            signature: String::new(),
        };
        beacon.signature = state.signature(&beacon.payload()).unwrap();
        serde_json::to_vec(&beacon).unwrap()
    }

    #[test]
    fn mesh_defaults_are_on_and_persist() {
        let root = temp_root();
        let fresh = Config::load(&root);
        assert!(fresh.enabled);
        assert_eq!(fresh.name, "luna");
        assert_eq!(fresh.main, None);

        let config = Config {
            enabled: true,
            name: " kitchen ".into(),
            main: Some("abc1234".into()),
        };
        config.save(&root).unwrap();
        let loaded = Config::load(&root);
        assert_eq!(loaded.name, "kitchen");
        assert_eq!(loaded.main.as_deref(), Some("abc1234"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn beacon_signature_verifies_and_pins_the_key() {
        let root = temp_root();
        let state = identity::State::load(&root).unwrap();
        let now = identity::unix_now();
        let raw = signed_beacon(&state, Some("abc1234"), now);
        let beacon = parse_beacon(&raw, now, "luna").unwrap();
        assert_eq!(beacon.station_id, state.station_id);
        assert_eq!(beacon.main.as_deref(), Some("abc1234"));

        let mut value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        value["model"] = json!("spoofed");
        let tampered = serde_json::to_vec(&value).unwrap();
        assert!(parse_beacon(&tampered, now, "luna").is_err());
        assert!(parse_beacon(&raw, now, "other-mesh").is_err());
        assert!(parse_beacon(&raw, now + MAX_CLOCK_SKEW_SECONDS + 1, "luna").is_err());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn invalid_identity_never_enters_the_peer_list() {
        let root = temp_root();
        let state = identity::State::load(&root).unwrap();
        let now = identity::unix_now();
        let mut value: serde_json::Value =
            serde_json::from_slice(&signed_beacon(&state, None, now)).unwrap();
        value["public_key"] = json!("00".repeat(32));
        let raw = serde_json::to_vec(&value).unwrap();
        assert!(parse_beacon(&raw, now, "luna").is_err());
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn snapshot_drops_expired_peers() {
        let peers = new_peers();
        let now = identity::unix_now();
        peers.lock().await.insert(
            "abc1234".into(),
            Peer {
                station_id: "abc1234".into(),
                model: "yandexmini".into(),
                ip: "192.168.0.10".into(),
                port: 8099,
                main: None,
                public_key: "00".repeat(32),
                last_seen: now.saturating_sub(PEER_TTL.as_secs() + 1),
            },
        );
        assert!(snapshot(&peers).await.is_empty());
    }
}
