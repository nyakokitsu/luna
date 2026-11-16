use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::{ffi::OsString, fs, path::Path, process::Command, time::Duration};

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

pub(crate) fn stage_release_update(
    executable: &Path,
    data_dir: &Path,
    listen: &str,
    model: &str,
    architecture: &str,
) -> Result<bool> {
    const RELEASES: &str = "https://github.com/nyakokitsu/luna/releases/latest/download";
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let manifest: serde_json::Value = client
        .get(format!("{RELEASES}/manifest.json"))
        .header("User-Agent", "luna-auto-updater")
        .send()?
        .error_for_status()?
        .json()?;
    let entry = manifest
        .get("devices")
        .and_then(serde_json::Value::as_array)
        .and_then(|devices| {
            devices.iter().find(|entry| {
                entry.get("device").and_then(serde_json::Value::as_str) == Some(model)
                    || entry
                        .get("models")
                        .and_then(serde_json::Value::as_array)
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
        .and_then(serde_json::Value::as_str)
        .context("Luna release manifest has no target")?;
    if (target.starts_with("aarch64-") && !matches!(architecture, "aarch64" | "arm64"))
        || (target.starts_with("armv7-") && !matches!(architecture, "arm" | "armv7" | "armv7l"))
    {
        bail!("Luna release target {target} does not match device architecture {architecture}");
    }
    let asset = entry
        .get("asset")
        .and_then(serde_json::Value::as_str)
        .context("Luna release manifest has no asset")?;
    let sums = client
        .get(format!("{RELEASES}/SHA256SUMS"))
        .header("User-Agent", "luna-auto-updater")
        .send()?
        .error_for_status()?
        .text()?;
    let expected = sums
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            match (fields.next(), fields.next()) {
                (Some(hash), Some(name)) if name == asset => Some(hash),
                _ => None,
            }
        })
        .context("release SHA256SUMS has no Luna checksum")?;
    if format!(
        "{:x}",
        Sha256::digest(fs::read(executable).unwrap_or_default())
    ) == expected
    {
        return Ok(false);
    }
    let binary = client
        .get(format!("{RELEASES}/{asset}"))
        .header("User-Agent", "luna-auto-updater")
        .send()?
        .error_for_status()?
        .bytes()?
        .to_vec();
    if format!("{:x}", Sha256::digest(&binary)) != expected {
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
        assert!(script.ends_with("\n"));
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
}
