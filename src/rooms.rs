//! Optional user metadata owned by the host, not by package resolution or hardware.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::Path};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Room {
    pub room: Option<String>,
}

impl Room {
    pub fn normalized(self) -> Result<Self> {
        let room = match self.room {
            Some(raw) => {
                if raw.chars().any(char::is_control) || raw.chars().count() > 64 {
                    bail!("room must be at most 64 characters without control characters")
                }
                let trimmed = raw.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_owned())
            }
            None => None,
        };
        Ok(Self { room })
    }

    pub fn load(root: &Path) -> Result<Self> {
        match fs::read_to_string(root.join("room.toml")) {
            Ok(raw) => toml::from_str::<Self>(&raw)
                .context("invalid room.toml")?
                .normalized(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("read room.toml"),
        }
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let room = self.clone().normalized()?;
        // Unique create_new files also support concurrent CLI/HTTP writers. Each
        // rename publishes one complete value; last successful rename wins.
        let temporary = root.join(format!(".room-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary)
                .context("create room temporary file")?;
            file.write_all(toml::to_string_pretty(&room)?.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, root.join("room.toml")).context("replace room.toml")?;
            #[cfg(unix)]
            fs::File::open(root)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_clear_unicode_and_unknown_fields() {
        for value in [None, Some("".into()), Some("   ".into())] {
            assert_eq!(Room { room: value }.normalized().unwrap(), Room::default());
        }
        assert_eq!(
            Room {
                room: Some(" Кухня ".into())
            }
            .normalized()
            .unwrap()
            .room
            .as_deref(),
            Some("Кухня")
        );
        assert!(Room {
            room: Some("я".repeat(64))
        }
        .normalized()
        .is_ok());
        for raw in ["bad\nroom".into(), "bad\0room".into(), "я".repeat(65)] {
            assert!(Room { room: Some(raw) }.normalized().is_err());
        }
        assert!(serde_json::from_str::<Room>(r#"{"room":"Kitchen","model":"spoof"}"#).is_err());
    }

    #[test]
    fn persists_without_hardware_override_and_rejects_corruption() {
        let root = std::env::temp_dir().join(format!("yam-room-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        assert_eq!(Room::load(&root).unwrap(), Room::default());
        Room {
            room: Some(" Детская ".into()),
        }
        .save(&root)
        .unwrap();
        assert_eq!(Room::load(&root).unwrap().room.as_deref(), Some("Детская"));
        assert!(!root.join("device.toml").exists());
        let before = fs::read(root.join("room.toml")).unwrap();
        assert!(Room {
            room: Some("bad\nroom".into())
        }
        .save(&root)
        .is_err());
        assert_eq!(fs::read(root.join("room.toml")).unwrap(), before);
        Room::default().save(&root).unwrap();
        assert_eq!(Room::load(&root).unwrap(), Room::default());
        fs::write(root.join("room.toml"), "not valid [").unwrap();
        assert!(Room::load(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_writers_publish_complete_values() {
        let root = std::env::temp_dir().join(format!("yam-room-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        std::thread::scope(|scope| {
            for i in 0..16 {
                let root = &root;
                scope.spawn(move || {
                    Room {
                        room: Some(format!("Комната {i}")),
                    }
                    .save(root)
                    .unwrap();
                    assert!(Room::load(root)
                        .unwrap()
                        .room
                        .unwrap()
                        .starts_with("Комната "));
                });
            }
        });
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
