//! Process-wide singleton protection for Luna.
//!
//! Luna can be installed by YAM or copied to an arbitrary directory by hand.
//! The executable path is therefore not a useful identity.  A fixed, locked
//! file gives both installation methods one shared instance namespace.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

const LOCK_PATH: &str = "/tmp/luna.lock";
const LOCK_EX: libc::c_int = 2;
const LOCK_NB: libc::c_int = 4;
const LOCK_UN: libc::c_int = 8;

pub(crate) struct InstanceGuard {
    file: File,
    path: PathBuf,
}

pub(crate) fn acquire() -> Result<InstanceGuard> {
    acquire_at(Path::new(LOCK_PATH))
}

fn acquire_at(path: &Path) -> Result<InstanceGuard> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open Luna instance lock {}", path.display()))?;

    if try_lock(&file)? {
        write_owner(&file)?;
        stop_legacy_instances()?;
        return Ok(InstanceGuard {
            file,
            path: path.to_path_buf(),
        });
    }

    // A previous Luna may have been installed manually, through YAM, or from
    // another release directory.  Ask that verified Luna to leave first.
    let owner = read_owner(&file);
    if let Some(pid) = owner.pid {
        if is_luna_process(pid) {
            tracing::info!(pid, "another Luna instance is running; replacing it");
            terminate(pid)?;
            for _ in 0..50 {
                if try_lock(&file)? {
                    write_owner(&file)?;
                    return Ok(InstanceGuard {
                        file,
                        path: path.to_path_buf(),
                    });
                }
                std::thread::sleep(Duration::from_millis(100));
            }

            // A verified Luna that ignored SIGTERM is still safe to replace.
            if process_exists(pid) && is_luna_process(pid) {
                tracing::warn!(pid, "Luna did not stop after SIGTERM; sending SIGKILL");
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
                for _ in 0..30 {
                    if try_lock(&file)? {
                        write_owner(&file)?;
                        return Ok(InstanceGuard {
                            file,
                            path: path.to_path_buf(),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    let details = owner
        .raw
        .filter(|raw| !raw.trim().is_empty())
        .unwrap_or_else(|| "owner information is unavailable".to_owned());
    bail!(
        "another process holds the Luna instance lock at {} ({details}); stop it manually",
        path.display()
    )
}

fn try_lock(file: &File) -> Result<bool> {
    let result = unsafe { libc::flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(error).context("lock Luna instance")
}

fn write_owner(file: &File) -> Result<()> {
    let pid = std::process::id();
    let executable = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "unknown".to_owned());
    let content = format!("pid={pid}\nexe={executable}\nmarker=luna-instance-v1\n");
    file.set_len(0).context("truncate Luna instance lock")?;
    let mut writer = file;
    writer.seek(SeekFrom::Start(0))?;
    writer.write_all(content.as_bytes())?;
    writer.sync_data().ok();
    Ok(())
}

struct Owner {
    pid: Option<u32>,
    raw: Option<String>,
}

fn read_owner(file: &File) -> Owner {
    let mut raw = String::new();
    let mut reader = file;
    let _ = reader.seek(SeekFrom::Start(0));
    let _ = reader.read_to_string(&mut raw);
    let pid = raw.lines().find_map(|line| {
        line.strip_prefix("pid=")
            .and_then(|value| value.parse().ok())
    });
    Owner {
        pid,
        raw: Some(raw),
    }
}

fn process_exists(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn is_luna_process(pid: u32) -> bool {
    let path = format!("/proc/{pid}/cmdline");
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let command = String::from_utf8_lossy(&bytes).replace('\0', " ");
    let tokens: Vec<&str> = command.split_whitespace().collect();
    if !tokens
        .iter()
        .any(|token| *token == "--listen" || token.starts_with("--listen="))
    {
        return false;
    }
    // Only the main executable is eligible. Helpers such as luna-evread and
    // luna-iohub2 must never be terminated by singleton takeover.
    tokens.iter().any(|token| {
        let name = token.rsplit('/').next().unwrap_or(token);
        (name == "luna" || name.starts_with("luna-"))
            && name != "luna-evread"
            && name != "luna-iohub2"
    })
}

fn terminate(pid: u32) -> Result<()> {
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).with_context(|| format!("stop previous Luna process {pid}"));
        }
    }
    Ok(())
}

fn stop_legacy_instances() -> Result<()> {
    let current_pid = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Ok(());
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == current_pid || !is_luna_process(pid) {
            continue;
        }

        tracing::info!(pid, "stopping legacy Luna instance without shared lock");
        terminate(pid)?;
        for _ in 0..50 {
            if !process_exists(pid) || !is_luna_process(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if process_exists(pid) && is_luna_process(pid) {
            tracing::warn!(
                pid,
                "legacy Luna did not stop after SIGTERM; sending SIGKILL"
            );
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
    Ok(())
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), LOCK_UN);
        }
        // Keep the lock file itself in place.  Removing it would allow a
        // racing process to open a new inode while this guard is dropping.
        let _ = &self.path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_is_rejected_when_owner_is_not_luna() {
        let path = std::env::temp_dir().join(format!("luna-lock-test-{}", std::process::id()));
        let first = acquire_at(&path).unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(!try_lock(&second).unwrap());
        drop(second);
        drop(first);
        let _ = std::fs::remove_file(path);
    }
}
