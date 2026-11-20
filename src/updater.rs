use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{ffi::OsString, fs, path::Path, process::Command, time::Duration};

const RELEASES: &str = "https://github.com/nyakokitsu/luna/releases/latest/download";
/// Release builds embed the maintainer's Ed25519 key(s). A build without a key
/// refuses automatic updates: there is no other way to tell a genuine release
/// from one uploaded by whoever controls the download URL.
const UPDATE_KEY_ENV: &str = "LUNA_UPDATE_PUBLIC_KEY";

fn shell_quote(value: &std::ffi::OsStr) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
}

/// Restart with the arguments the daemon was started with. Only rewriting
/// listen/data-dir dropped flags such as --yam-root and --yam-run, so after an
/// auto-update every binary YAM plugin was reported as incompatible
/// ("YAM binary plugins require --yam-run or YAM_RUN") until the next reboot.
/// Defaults are added only when the daemon was started without those flags,
/// so a restart keeps both the effective values and any extra flags.
fn restart_arguments(args: Vec<OsString>, listen: &str, data_dir: &Path) -> Vec<OsString> {
    let has = |flag: &str| {
        args.iter()
            .any(|arg| arg == flag || arg.to_string_lossy().starts_with(&format!("{flag}=")))
    };
    let has_listen = has("--listen");
    let has_data_dir = has("--data-dir");
    let mut out = args;
    if !has_listen {
        out.push("--listen".into());
        out.push(listen.into());
    }
    if !has_data_dir {
        out.push("--data-dir".into());
        out.push(data_dir.as_os_str().to_owned());
    }
    out
}

fn restart_script(staged: &Path, executable: &Path, args: &[OsString]) -> String {
    let mut script = format!(
        "#!/bin/sh\nsleep 1\nmv {} {}\nexec {}",
        shell_quote(staged.as_os_str()),
        shell_quote(executable.as_os_str()),
        shell_quote(executable.as_os_str()),
    );
    for arg in args {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    script.push('\n');
    script
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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

fn parse_public_key(value: &str) -> Option<VerifyingKey> {
    VerifyingKey::from_bytes(&decode_hex::<32>(value.trim())?).ok()
}

/// Accepts either the raw 64-byte signature or its 128-character hex form.
fn parse_signature(raw: &[u8]) -> Option<[u8; 64]> {
    if raw.len() == 64 {
        let mut signature = [0u8; 64];
        signature.copy_from_slice(raw);
        return Some(signature);
    }
    decode_hex::<64>(std::str::from_utf8(raw).ok()?.trim())
}

/// Keys compiled into this binary: the current one and, during a rotation,
/// the next one. Both are accepted so a key can be replaced without cutting
/// stations off from updates.
fn embedded_update_keys() -> Vec<VerifyingKey> {
    [
        option_env!("LUNA_UPDATE_PUBLIC_KEY"),
        option_env!("LUNA_UPDATE_PUBLIC_KEY_NEXT"),
    ]
    .into_iter()
    .flatten()
    .filter_map(parse_public_key)
    .collect()
}

fn verify_sums(signature: &[u8], sums: &[u8], keys: &[VerifyingKey]) -> Result<()> {
    if keys.is_empty() {
        bail!("this build has no {UPDATE_KEY_ENV}; refusing unsigned automatic updates");
    }
    let signature = parse_signature(signature).context("release signature is malformed")?;
    let signature = Signature::from_bytes(&signature);
    if keys.iter().any(|key| key.verify(sums, &signature).is_ok()) {
        Ok(())
    } else {
        bail!("SHA256SUMS signature does not match a trusted Luna update key")
    }
}

/// Finds one checksum in the signed `SHA256SUMS` file. Both `sha  file` and
/// `sha *file` forms are accepted.
fn checksum_for<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let hash = fields.next()?;
        let file = fields.next()?.trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then_some(hash)
    })
}

fn version_parts(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.trim().trim_start_matches('v');
    let mut fields = value.split('.');
    let parsed = (
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
    );
    if fields.next().is_some() {
        return None;
    }
    Some(parsed)
}

/// Numeric comparison of `major.minor.patch`, so an old signed release cannot
/// be replayed over a newer installation.
fn is_newer(remote: &str, current: &str) -> bool {
    match (version_parts(remote), version_parts(current)) {
        (Some(remote), Some(current)) => remote > current,
        _ => false,
    }
}

fn release_version(manifest: &Value) -> Option<&str> {
    manifest
        .get("version")
        .and_then(Value::as_str)
        .filter(|version| version_parts(version).is_some())
}

fn current_version() -> &'static str {
    option_env!("LUNA_RELEASE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

fn download(client: &reqwest::blocking::Client, name: &str) -> Result<reqwest::blocking::Response> {
    client
        .get(format!("{RELEASES}/{name}"))
        .header("User-Agent", "luna-auto-updater")
        .send()
        .with_context(|| format!("download release {name}"))?
        .error_for_status()
        .with_context(|| format!("download release {name}"))
}

pub(crate) fn stage_release_update(
    executable: &Path,
    data_dir: &Path,
    listen: &str,
    model: &str,
    architecture: &str,
) -> Result<bool> {
    let keys = embedded_update_keys();
    if keys.is_empty() {
        bail!("this build has no update public key; automatic updates are disabled");
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    // The manifest is fetched before parsing because its checksum must be
    // covered by the signed SHA256SUMS file.
    let manifest_bytes = download(&client, "manifest.json")?.bytes()?.to_vec();
    let sums_bytes = download(&client, "SHA256SUMS")?.bytes()?.to_vec();
    let signature = download(&client, "SHA256SUMS.sig")?.bytes()?.to_vec();
    verify_sums(&signature, &sums_bytes, &keys)?;
    let sums = std::str::from_utf8(&sums_bytes).context("SHA256SUMS is not UTF-8")?;
    let manifest_hash = checksum_for(sums, "manifest.json")
        .context("signed SHA256SUMS does not cover manifest.json")?;
    if sha256_hex(&manifest_bytes) != manifest_hash {
        bail!("release manifest.json checksum mismatch");
    }
    let manifest: Value =
        serde_json::from_slice(&manifest_bytes).context("release manifest is not valid JSON")?;
    if manifest.get("schema_version").and_then(Value::as_u64) != Some(1) {
        bail!("unsupported Luna release manifest schema");
    }
    let version = release_version(&manifest)
        .context("Luna release manifest has no valid version")?
        .to_owned();
    if !is_newer(&version, current_version()) {
        tracing::info!(
            version = %version,
            current = current_version(),
            "Luna release is not newer than the running version; skipping"
        );
        return Ok(false);
    }
    let entry = manifest
        .get("devices")
        .and_then(Value::as_array)
        .and_then(|devices| {
            devices.iter().find(|entry| {
                entry.get("device").and_then(Value::as_str) == Some(model)
                    || entry
                        .get("models")
                        .and_then(Value::as_array)
                        .is_some_and(|models| {
                            models
                                .iter()
                                .any(|item| item.as_str().is_some_and(|name| name == model))
                        })
            })
        })
        .with_context(|| format!("no Luna release asset for model {model}"))?;
    let target = entry
        .get("target")
        .and_then(Value::as_str)
        .context("Luna release manifest has no target")?;
    if (target.starts_with("aarch64-") && !matches!(architecture, "aarch64" | "arm64"))
        || (target.starts_with("armv7-") && !matches!(architecture, "arm" | "armv7" | "armv7l"))
    {
        bail!("Luna release target {target} does not match device architecture {architecture}");
    }
    let asset = entry
        .get("asset")
        .and_then(Value::as_str)
        .context("Luna release manifest has no asset")?;
    let expected = checksum_for(sums, asset).context("release SHA256SUMS has no Luna checksum")?;
    if sha256_hex(&fs::read(executable).unwrap_or_default()) == expected {
        return Ok(false);
    }
    let binary = client
        .get(format!("{RELEASES}/{asset}"))
        .header("User-Agent", "luna-auto-updater")
        .send()?
        .error_for_status()?
        .bytes()?
        .to_vec();
    if sha256_hex(&binary) != expected {
        bail!("release checksum mismatch")
    }
    let staged = executable.with_extension("luna-update");
    fs::write(&staged, binary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }
    let helper = data_dir.join(".luna-restart-update.sh");
    // Keep the daemon's own command line: YAM hosting needs --yam-root and
    // --yam-run, and future flags must survive an update restart too.
    let args = restart_arguments(std::env::args_os().skip(1).collect(), listen, data_dir);
    fs::write(&helper, restart_script(&staged, executable, &args))?;
    Command::new("sh").arg(helper).spawn()?;
    tracing::info!(version = %version, "installing signed Luna release");
    Ok(true)
}

/// Binary packages reach the daemon on loopback unless it listens on one address only.
pub(crate) fn runtime_api_url(listen: &str) -> Result<String> {
    let mut address: std::net::SocketAddr = listen
        .parse()
        .with_context(|| format!("listen address {listen} must be IP:port"))?;
    if address.ip().is_unspecified() {
        address.set_ip(match address {
            std::net::SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
            std::net::SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
        });
    }
    Ok(format!("http://{address}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn restart_script_preserves_yam_arguments() {
        let args = [
            "--listen",
            "0.0.0.0:8099",
            "--data-dir",
            "/data/var/luna/data",
            "--yam-root",
            "/data/yam",
            "--yam-run",
            "/tmp/yam",
        ]
        .map(OsString::from);
        let script = restart_script(
            Path::new("/data/yam/bin/luna.luna-update"),
            Path::new("/data/yam/bin/luna"),
            &args,
        );
        assert!(script.starts_with("#!/bin/sh\nsleep 1\nmv "));
        assert!(script.contains("'--yam-root' '/data/yam'"));
        assert!(script.contains("'--yam-run' '/tmp/yam'"));
        assert!(script.ends_with('\n'));
    }

    #[test]
    fn restart_arguments_add_defaults_only_when_missing() {
        let data_dir = Path::new("/data/var/luna/data");
        let empty = restart_arguments(Vec::new(), "0.0.0.0:8099", data_dir);
        assert_eq!(
            empty,
            vec![
                OsString::from("--listen"),
                OsString::from("0.0.0.0:8099"),
                OsString::from("--data-dir"),
                OsString::from("/data/var/luna/data"),
            ]
        );
        let existing = restart_arguments(
            vec![
                OsString::from("--listen=127.0.0.1:9000"),
                OsString::from("--data-dir"),
                OsString::from("/custom"),
                OsString::from("--yam-run"),
                OsString::from("/tmp/yam"),
            ],
            "0.0.0.0:8099",
            data_dir,
        );
        assert_eq!(
            existing,
            vec![
                OsString::from("--listen=127.0.0.1:9000"),
                OsString::from("--data-dir"),
                OsString::from("/custom"),
                OsString::from("--yam-run"),
                OsString::from("/tmp/yam"),
            ]
        );
    }

    #[test]
    fn restart_script_quotes_every_argument() {
        let script = restart_script(
            Path::new("staged"),
            Path::new("luna"),
            &[OsString::from("a'b")],
        );
        assert!(script.contains("'a'\\''b'"));
    }

    #[test]
    fn restart_script_without_arguments_keeps_defaults() {
        let script = restart_script(Path::new("staged"), Path::new("luna"), &[]);
        assert_eq!(
            script,
            "#!/bin/sh\nsleep 1\nmv 'staged' 'luna'\nexec 'luna'\n"
        );
    }

    #[test]
    fn signed_checksums_accept_either_rotation_key() {
        let current = signing_key();
        let next = SigningKey::from_bytes(&[9u8; 32]);
        let sums = b"abc  luna-mini\n";
        let signature = current.sign(sums);
        let keys = vec![current.verifying_key(), next.verifying_key()];
        assert!(verify_sums(&signature.to_bytes(), sums, &keys).is_ok());
        assert!(verify_sums(signature.to_bytes().as_slice(), sums, &keys).is_ok());
        let hex = signature
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert!(verify_sums(hex.as_bytes(), sums, &keys).is_ok());
        assert!(verify_sums(signature.to_bytes().as_slice(), b"tampered", &keys).is_err());
        assert!(verify_sums(&signature.to_bytes(), sums, &[]).is_err());
        assert!(verify_sums(b"short", sums, &keys).is_err());
    }

    #[test]
    fn checksums_and_versions_guard_the_release() {
        let sums = "a".repeat(64) + "  manifest.json\n" + &"b".repeat(64) + " *luna-mini\n";
        let manifest_hash = "a".repeat(64);
        let asset_hash = "b".repeat(64);
        assert_eq!(
            checksum_for(&sums, "manifest.json"),
            Some(manifest_hash.as_str())
        );
        assert_eq!(checksum_for(&sums, "luna-mini"), Some(asset_hash.as_str()));
        assert_eq!(checksum_for(&sums, "luna-midi"), None);
        assert_eq!(checksum_for("zz manifest.json", "manifest.json"), None);

        assert!(is_newer("0.2.36", "0.2.35"));
        assert!(is_newer("0.3.0", "0.2.99"));
        assert!(!is_newer("0.2.35", "0.2.35"));
        assert!(!is_newer("0.2.34", "0.2.35"));
        assert!(!is_newer("nightly", "0.2.35"));
        assert_eq!(version_parts("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(version_parts("1.2"), None);
    }

    #[test]
    fn manifest_version_is_required() {
        let valid = serde_json::json!({ "schema_version": 1, "version": "0.2.36" });
        assert_eq!(release_version(&valid), Some("0.2.36"));
        let missing = serde_json::json!({ "schema_version": 1 });
        assert_eq!(release_version(&missing), None);
        let malformed = serde_json::json!({ "version": "latest" });
        assert_eq!(release_version(&malformed), None);
    }

    #[test]
    fn signature_from_the_release_tool_verifies() {
        let key = parse_public_key(include_str!("../tests/fixtures/update-key.pub.hex").trim())
            .expect("fixture key is not Ed25519 hex");
        let sums = include_bytes!("../tests/fixtures/SHA256SUMS");
        let signature = include_bytes!("../tests/fixtures/SHA256SUMS.sig");
        assert!(verify_sums(signature, sums, &[key]).is_ok());
    }

    #[test]
    fn build_without_a_key_refuses_automatic_updates() {
        if !embedded_update_keys().is_empty() {
            return;
        }
        let error = stage_release_update(
            Path::new("luna"),
            Path::new("/tmp"),
            "127.0.0.1:8099",
            "yandexmini",
            "aarch64",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("no update public key"),
            "{error}"
        );
    }

    #[test]
    fn public_key_parser_rejects_wrong_length() {
        let key = signing_key().verifying_key();
        let hex = key
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(parse_public_key(&hex), Some(key));
        assert!(parse_public_key("00").is_none());
        assert!(parse_public_key(&"zz".repeat(32)).is_none());
    }
}
