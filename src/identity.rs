//! Station identity, optional server registration and configuration backup.
//!
//! The station id is the first seven characters of `mv_uuid` from
//! `/bkupgrade/new_nvram_config`, so every station keeps the identifier the
//! firmware already assigned to it. The signing key is generated locally and
//! never leaves the station; only its public half is registered.
//!
//! Registration is optional. Mesh discovery works with the local identity
//! alone, and a station that never registers sends nothing to the network.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// Firmware NVRAM dump that carries `mv_uuid`.
pub(crate) const NVRAM_CONFIG: &str = "/bkupgrade/new_nvram_config";
const STATE_FILE: &str = "identity.toml";
const REGISTRY_ENV: &str = "LUNA_REGISTRY_ENDPOINT";
const NVRAM_ENV: &str = "LUNA_NVRAM_CONFIG";
const STATION_ID_ENV: &str = "LUNA_STATION_ID";
const DEFAULT_REGISTRY: &str = "https://stats.x-servers.ru";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Configuration backup is a settings snapshot, not a package archive.
const MAX_BACKUP_FILE: u64 = 192 * 1024;

/// Persistent station identity. `secret_key` is a 32-byte Ed25519 seed in hex
/// and is never included in API responses or backups.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    #[serde(default)]
    pub station_id: String,
    #[serde(default)]
    pub public_key: String,
    #[serde(default)]
    secret_key: String,
    /// Where `station_id` came from: `nvram`, `env` or `generated`.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub registered_at: Option<u64>,
    #[serde(default)]
    pub backup_at: Option<u64>,
}

impl State {
    fn path(root: &Path) -> PathBuf {
        root.join(STATE_FILE)
    }

    /// Loads the identity, creating the key pair on first start. A damaged
    /// file is replaced rather than trusted: an unreadable key cannot sign.
    pub(crate) fn load(root: &Path) -> Result<Self> {
        let path = Self::path(root);
        match fs::read_to_string(&path) {
            Ok(raw) => match toml::from_str::<Self>(&raw) {
                Ok(mut state) => {
                    if state.repair() {
                        state.save(root)?;
                    }
                    Ok(state)
                }
                Err(error) => {
                    tracing::warn!(%error, path=%path.display(), "invalid identity.toml; creating a new local identity");
                    let state = Self::create();
                    state.save(root)?;
                    Ok(state)
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let state = Self::create();
                state.save(root)?;
                Ok(state)
            }
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    fn create() -> Self {
        let (station_id, source) = resolve_station_id();
        let (secret_key, public_key) = generate_key();
        Self {
            station_id,
            public_key,
            secret_key,
            source: source.into(),
            registered_at: None,
            backup_at: None,
        }
    }

    /// Restores missing or unusable fields. Returns whether anything changed.
    fn repair(&mut self) -> bool {
        let mut changed = false;
        if !valid_station_id(&self.station_id) {
            let (station_id, source) = resolve_station_id();
            self.station_id = station_id;
            self.source = source.into();
            changed = true;
        } else if self.station_id.chars().any(|c| c.is_ascii_uppercase()) {
            self.station_id = self.station_id.to_ascii_lowercase();
            changed = true;
        }
        let secret = decode_hex::<32>(&self.secret_key);
        let public = decode_hex::<32>(&self.public_key);
        let matches = match (secret, public) {
            (Some(secret), Some(public)) => {
                SigningKey::from_bytes(&secret).verifying_key().to_bytes() == public
            }
            _ => false,
        };
        if !matches {
            let (secret_key, public_key) = generate_key();
            self.secret_key = secret_key;
            self.public_key = public_key;
            // A new key invalidates any earlier registration.
            self.registered_at = None;
            self.backup_at = None;
            changed = true;
        }
        if self.source.is_empty() {
            self.source = "generated".into();
            changed = true;
        }
        changed
    }

    /// Writes the key material with owner-only permissions.
    fn save(&self, root: &Path) -> Result<()> {
        fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
        let temporary = root.join(format!(".identity-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary)
                .context("create identity temporary file")?;
            file.write_all(toml::to_string_pretty(self)?.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, Self::path(root)).context("replace identity.toml")?;
            #[cfg(unix)]
            fs::File::open(root)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(crate) fn registered(&self) -> bool {
        self.registered_at.is_some()
    }

    pub(crate) fn signature(&self, payload: &str) -> Result<String> {
        let secret =
            decode_hex::<32>(&self.secret_key).context("identity has no usable signing key")?;
        let key = SigningKey::from_bytes(&secret);
        Ok(encode_hex(&key.sign(payload.as_bytes()).to_bytes()))
    }
}

/// Verifies a hex Ed25519 signature over `payload` with a raw public key.
pub(crate) fn verify(public_key: &str, payload: &str, signature: &str) -> bool {
    let Some(public) = decode_hex::<32>(public_key) else {
        return false;
    };
    let Ok(public) = VerifyingKey::from_bytes(&public) else {
        return false;
    };
    let Some(signature) = decode_hex::<64>(signature) else {
        return false;
    };
    public
        .verify(payload.as_bytes(), &Signature::from_bytes(&signature))
        .is_ok()
}

/// Station ids are short and URL-safe; `mv_uuid` prefixes are hexadecimal.
pub(crate) fn valid_station_id(value: &str) -> bool {
    (4..=32).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

pub(crate) fn valid_public_key(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Collector that stores registrations and backups.
pub(crate) fn endpoint() -> String {
    std::env::var(REGISTRY_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| value.starts_with("http://") || value.starts_with("https://"))
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_owned())
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Canonical registration payload; the collector rebuilds it byte for byte.
pub(crate) fn register_payload(station_id: &str, public_key: &str, ts: u64) -> String {
    format!("luna-register-v1|{station_id}|{public_key}|{ts}")
}

/// Canonical backup payload. The hash covers the exact JSON text that is sent.
pub(crate) fn config_payload(station_id: &str, created_at: u64, hash: &str) -> String {
    format!("luna-config-v1|{station_id}|{created_at}|{hash}")
}

/// Registers the public key with the collector. Blocking: run it on a
/// blocking task so the async runtime stays free.
pub(crate) fn register(root: &Path, model: &str) -> Result<State> {
    let state = State::load(root)?;
    let ts = unix_now();
    let payload = register_payload(&state.station_id, &state.public_key, ts);
    let signature = state.signature(&payload)?;
    let body = json!({
        "station_id": state.station_id,
        "public_key": state.public_key,
        "ts": ts,
        "signature": signature,
        "model": model,
        "version": env!("CARGO_PKG_VERSION"),
    });
    let response = client()?
        .post(format!("{}/v1/luna/register", endpoint()))
        .json(&body)
        .send()
        .context("register station")?;
    if !response.status().is_success() {
        bail!(
            "registration endpoint answered {}: {}",
            response.status(),
            response_error(response)
        );
    }
    let mut state = state;
    state.registered_at = Some(ts);
    state.save(root)?;
    Ok(state)
}

/// Snapshot of the owner-visible configuration. Signing keys, the anonymous
/// install id and plugin data are deliberately left out.
pub(crate) fn config_snapshot(root: &Path, state: &State) -> Result<String> {
    let mut files = Map::new();
    for (key, name) in [
        ("settings", "settings.toml"),
        ("device", "device.toml"),
        ("room", "room.toml"),
    ] {
        let path = root.join(name);
        match fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX_BACKUP_FILE => {
                bail!("{name} is larger than the configuration backup limit")
            }
            Ok(_) => {
                let text =
                    fs::read_to_string(&path).with_context(|| format!("read {name} for backup"))?;
                files.insert(key.into(), json!(text));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("stat {name}")),
        }
    }
    let created_at = unix_now();
    let snapshot = json!({
        "schema": 1,
        "station_id": state.station_id,
        "created_at": created_at,
        "files": files,
    });
    Ok(serde_json::to_string(&snapshot)?)
}

/// Uploads the configuration snapshot. Blocking for the same reason as
/// [`register`].
pub(crate) fn backup(root: &Path) -> Result<(u64, u64)> {
    let state = State::load(root)?;
    if !state.registered() {
        bail!("station is not registered yet");
    }
    let config = config_snapshot(root, &state)?;
    if config.len() as u64 > MAX_BACKUP_FILE {
        bail!("configuration snapshot is larger than the backup limit");
    }
    let created_at = serde_json::from_str::<Value>(&config)?
        .get("created_at")
        .and_then(Value::as_u64)
        .unwrap_or_else(unix_now);
    let hash = sha256_hex(config.as_bytes());
    let payload = config_payload(&state.station_id, created_at, &hash);
    let signature = state.signature(&payload)?;
    let body = json!({
        "station_id": state.station_id,
        "created_at": created_at,
        "config": config,
        "signature": signature,
    });
    let response = client()?
        .post(format!("{}/v1/luna/config", endpoint()))
        .json(&body)
        .send()
        .context("upload configuration backup")?;
    if !response.status().is_success() {
        bail!(
            "backup endpoint answered {}: {}",
            response.status(),
            response_error(response)
        );
    }
    let mut state = state;
    state.backup_at = Some(unix_now());
    state.save(root)?;
    Ok((created_at, config.len() as u64))
}

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("luna/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build registration client")
}

fn response_error(response: reqwest::blocking::Response) -> String {
    let status = response.status();
    let text = response.text().unwrap_or_default();
    let text = text.trim();
    if text.is_empty() {
        return status.to_string();
    }
    text.chars().take(240).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    encode_hex(&hasher.finalize())
}

/// `LUNA_STATION_ID` is for development and tests; otherwise the firmware
/// NVRAM dump wins and a random persisted id is the last resort.
fn resolve_station_id() -> (String, &'static str) {
    if let Ok(value) = std::env::var(STATION_ID_ENV) {
        let value = value.trim().to_ascii_lowercase();
        if valid_station_id(&value) {
            return (value, "env");
        }
    }
    let path = std::env::var(NVRAM_ENV).unwrap_or_else(|_| NVRAM_CONFIG.to_owned());
    // The dump is not guaranteed to be UTF-8: scan it lossily instead of
    // treating an unrelated binary byte as a missing identity.
    if let Ok(bytes) = fs::read(&path) {
        let raw = String::from_utf8_lossy(&bytes);
        if let Some(station_id) = station_id_from_nvram(&raw) {
            return (station_id, "nvram");
        }
        tracing::warn!(path=%path, "mv_uuid is missing from the NVRAM config");
    }
    let generated = uuid::Uuid::new_v4().simple().to_string();
    (generated[..7].to_owned(), "generated")
}

/// Reads `mv_uuid` from a firmware NVRAM dump and keeps its first seven
/// characters. The dump is not formal JSON, so `key=value`, `key: value` and
/// quoted `"key": "value"` forms are all accepted.
pub(crate) fn station_id_from_nvram(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let line = line.trim().trim_start_matches(['"', '{', ',', ' ']);
        if !line.to_ascii_lowercase().starts_with("mv_uuid") {
            continue;
        }
        let rest = &line["mv_uuid".len()..];
        let rest = rest.trim_start_matches(['"', ':', '=', ' ', '\t']);
        let token = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>();
        let token = token.replace('-', "");
        if token.len() >= 7 {
            return Some(token[..7].to_ascii_lowercase());
        }
    }
    None
}

fn generate_key() -> (String, String) {
    let mut seed = [0u8; 32];
    seed[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    seed[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let key = SigningKey::from_bytes(&seed);
    (
        encode_hex(&seed),
        encode_hex(&key.verifying_key().to_bytes()),
    )
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(value.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("luna-identity-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn station_id_is_the_first_seven_chars_of_mv_uuid() {
        let raw = "board=station\nmv_uuid=0a1b2c3d4e5f6071\n";
        assert_eq!(station_id_from_nvram(raw).as_deref(), Some("0a1b2c3"));
        let quoted = r#"{"mv_uuid": "ABCDEF1234567890"}"#;
        assert_eq!(station_id_from_nvram(quoted).as_deref(), Some("abcdef1"));
        let spaced = "mv_uuid: a1b2c3d4";
        assert_eq!(station_id_from_nvram(spaced).as_deref(), Some("a1b2c3d"));
        assert_eq!(
            station_id_from_nvram("MV_UUID = a1b2c3d4e5").as_deref(),
            Some("a1b2c3d")
        );
        assert_eq!(station_id_from_nvram("mv_uuid=short"), None);
        assert_eq!(station_id_from_nvram("board=station"), None);
    }

    #[test]
    fn signature_round_trip_rejects_tampering() {
        let root = temp_root();
        let state = State::load(&root).unwrap();
        let payload = register_payload(&state.station_id, &state.public_key, 42);
        let signature = state.signature(&payload).unwrap();
        assert!(verify(&state.public_key, &payload, &signature));
        assert!(!verify(
            &state.public_key,
            &format!("{payload}x"),
            &signature
        ));
        assert!(!verify(&"00".repeat(32), &payload, &signature));
        assert!(!verify(&state.public_key, &payload, "zz"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn identity_is_stable_across_reloads() {
        let root = temp_root();
        let first = State::load(&root).unwrap();
        assert_eq!(first.station_id.len(), 7);
        assert!(valid_station_id(&first.station_id));
        assert_eq!(first.public_key.len(), 64);
        assert!(!first.registered());
        let second = State::load(&root).unwrap();
        assert_eq!(first.station_id, second.station_id);
        assert_eq!(first.public_key, second.public_key);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn backup_contains_settings_but_no_key_material() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let state = State::load(&root).unwrap();
        fs::write(root.join("settings.toml"), "led = { count = 6 }\n").unwrap();
        fs::write(root.join("room.toml"), "room = \"Kitchen\"\n").unwrap();
        let snapshot = config_snapshot(&root, &state).unwrap();
        let value: Value = serde_json::from_str(&snapshot).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(value["station_id"], state.station_id);
        assert!(value["files"]["settings"]
            .as_str()
            .unwrap()
            .contains("count"));
        assert_eq!(value["files"]["room"], "room = \"Kitchen\"\n");
        assert!(value["files"].get("device").is_none());
        assert!(!snapshot.contains(&state.secret_key));
        assert!(!snapshot.contains(&state.public_key));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rotated_or_damaged_key_is_regenerated() {
        let root = temp_root();
        let mut state = State::load(&root).unwrap();
        state.public_key = "00".repeat(32);
        state.registered_at = Some(1);
        let mut repaired = state.clone();
        assert!(repaired.repair());
        assert_ne!(repaired.public_key, state.public_key);
        assert_eq!(repaired.registered_at, None);
        fs::remove_dir_all(root).ok();
    }
}
