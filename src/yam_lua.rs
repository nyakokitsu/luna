//! Resolve YAM Lua payloads through their selected package, not common-tree
//! file links. The lease must outlive both Lua execution and its web page.
use super::*;

pub(super) struct Prepared {
    pub manifest: Manifest,
    pub root: PathBuf,
    _lease: Option<yam_binary::Lease>,
}

pub(super) fn prepare(
    id: &str,
    location: PluginLocation,
    yam: Option<&Path>,
    run: Option<&Path>,
) -> Result<Prepared> {
    let lease = if location.source == PluginSource::Yam {
        Some(yam_binary::Lease::acquire(run)?)
    } else {
        None
    };
    let manifest: Manifest =
        toml::from_str(&fs::read_to_string(location.root.join("plugin.toml"))?)?;
    if manifest.id != id || manifest.launch.kind != LaunchKind::Lua {
        bail!("Lua descriptor id or launch kind does not match requested plugin")
    }
    let root = if lease.is_some() {
        yam_payload::resolve(yam.context("YAM root unavailable")?, &location.root, id)?.root
    } else {
        location.root
    };
    safe_relative(&manifest.entry)?;
    safe_relative(manifest.launch.path(&manifest))?;
    let entry = fs::canonicalize(root.join(manifest.launch.path(&manifest)))?;
    if !entry.starts_with(fs::canonicalize(&root)?) || !entry.is_file() {
        bail!("Lua entry escapes its plugin directory or is not a file")
    }
    Ok(Prepared {
        manifest,
        root,
        _lease: lease,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::{fd::AsRawFd, unix::fs::symlink};

    struct Fixture {
        base: PathBuf,
        yam: PathBuf,
        run: PathBuf,
        root: PathBuf,
        shared: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let base = std::env::temp_dir().join(format!("luna-yam-lua-{}", Uuid::new_v4()));
            let yam = base.join("yam");
            let run = base.join("run");
            let selected = yam.join("pkgs/example/page/1.0.0");
            let root = selected.join("share/luna/plugins/example.page");
            let shared = yam.join("share/luna/plugins/example.page");
            fs::create_dir_all(&root).unwrap();
            fs::create_dir_all(&shared).unwrap();
            fs::create_dir_all(&run).unwrap();
            fs::write(run.join("yam.lock"), "").unwrap();
            fs::write(root.join("plugin.toml"),
                "id='example.page'\nname='Page'\nversion='1.0.0'\nentry='main.lua'\npermissions=['web']\n").unwrap();
            fs::write(root.join("main.lua"), "register.customweb('index.html')").unwrap();
            fs::write(root.join("index.html"), "<h1>Package page</h1>").unwrap();
            symlink("1.0.0", selected.parent().unwrap().join("current")).unwrap();
            for name in ["plugin.toml", "main.lua", "index.html"] {
                symlink(root.join(name), shared.join(name)).unwrap();
            }
            Self {
                base,
                yam,
                run,
                root,
                shared,
            }
        }

        fn prepare(&self) -> Result<Prepared> {
            prepare(
                "example.page",
                PluginLocation {
                    root: self.shared.clone(),
                    source: PluginSource::Yam,
                    package: Some("example/page".into()),
                },
                Some(&self.yam),
                Some(&self.run),
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn pins_page_and_holds_transaction_lease_until_stop() {
        let f = Fixture::new();
        let prepared = f.prepare().unwrap();
        assert_eq!(prepared.root, f.root);
        let page = fs::canonicalize(prepared.root.join("index.html")).unwrap();
        assert!(page.starts_with(&prepared.root));
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(f.run.join("yam.lock"))
            .unwrap();
        assert_ne!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        drop(prepared);
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }

    #[test]
    fn rejects_foreign_descriptor() {
        let f = Fixture::new();
        fs::remove_file(f.shared.join("plugin.toml")).unwrap();
        let foreign = f.base.join("foreign.toml");
        fs::copy(f.root.join("plugin.toml"), &foreign).unwrap();
        symlink(foreign, f.shared.join("plugin.toml")).unwrap();
        assert!(f.prepare().is_err());
    }

    #[test]
    fn rejects_entry_escape() {
        let f = Fixture::new();
        fs::remove_file(f.root.join("main.lua")).unwrap();
        let outside = f.base.join("outside.lua");
        fs::write(&outside, "").unwrap();
        symlink(outside, f.root.join("main.lua")).unwrap();
        assert!(f.prepare().is_err());
    }

    #[test]
    fn rejects_escaping_current() {
        let f = Fixture::new();
        let current = f.yam.join("pkgs/example/page/current");
        fs::remove_file(&current).unwrap();
        symlink(&f.base, current).unwrap();
        assert!(f.prepare().is_err());
    }

    #[test]
    fn refuses_start_during_transaction() {
        let f = Fixture::new();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(f.run.join("yam.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(f.prepare().is_err());
    }
}
