//! Pinned, package-owned binary plugins. YAM remains the file/package manager;
//! Luna owns process supervision and runtime permissions.
use super::*;

/// Closing the descriptor releases flock, including error/cancellation paths.
pub(super) struct Lease {
    _file: fs::File,
}

impl Lease {
    pub(super) fn acquire(run: Option<&Path>) -> Result<Self> {
        let run = run.context("YAM binary plugins require --yam-run or YAM_RUN")?;
        let file = fs::OpenOptions::new()
            .read(true)
            .open(run.join("yam.lock"))
            .context("open existing YAM transaction lock")?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // Fail rather than block the runtime or start against a changing tree.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
                bail!("YAM transaction is active; retry binary plugin start afterwards")
            }
            Ok(Self { _file: file })
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            bail!("YAM binary plugin leases require Unix flock")
        }
    }
}

/// Caller holds the YAM shared lock. Resolve through the package's current
/// selector, not a freely mutable common-tree executable link.
pub(super) fn pinned_root(yam: &Path, shared: &Path, manifest: &Manifest) -> Result<PathBuf> {
    validate_yam_id(&manifest.id)?;
    let (namespace, name) = manifest
        .id
        .split_once('.')
        .context("invalid YAM plugin id")?;
    let yam = fs::canonicalize(yam)?;
    let package = yam.join("pkgs").join(namespace).join(name);
    let selected = fs::canonicalize(package.join("current"))
        .context("YAM binary plugin has no selected package version")?;
    if selected.parent() != Some(package.as_path()) || !selected.is_dir() {
        bail!("YAM current selector escapes its package version directory")
    }
    let root = selected.join("share/luna/plugins").join(&manifest.id);
    let descriptor = fs::canonicalize(shared.join("plugin.toml"))?;
    if descriptor != root.join("plugin.toml") {
        bail!("YAM plugin descriptor does not belong to the selected package version")
    }
    validate_policy(manifest)?;
    safe_relative(&manifest.entry)?;
    safe_relative(manifest.launch.path(manifest))?;
    let executable = fs::canonicalize(root.join(manifest.launch.path(manifest)))?;
    if !executable.starts_with(&root) || !executable.is_file() {
        bail!("YAM binary launcher escapes its plugin payload or is not a file")
    }
    for directory in &manifest.launch.library_paths {
        safe_relative(directory)?;
        let directory = fs::canonicalize(root.join(directory))?;
        if !directory.starts_with(&root) || !directory.is_dir() {
            bail!("YAM binary library directory escapes its plugin payload")
        }
    }
    Ok(root)
}

fn validate_policy(manifest: &Manifest) -> Result<()> {
    dependencies::validate_policy(manifest)?;
    if manifest.launch.kind != LaunchKind::Binary {
        bail!("YAM binary launcher requires launch.kind=binary")
    }
    if !manifest.permissions.iter().any(|p| p == "exec") {
        bail!("binary packages require the exec permission")
    }
    if manifest
        .launch
        .interpreter
        .as_deref()
        .is_some_and(|i| i != "sh")
    {
        bail!("launch.interpreter supports only sh")
    }
    validate_input_policy(manifest)?;
    validate_ipc_policy(manifest)?;
    validate_command_policy(manifest)?;
    if let Some(provider) = &manifest.media_provider {
        provider.validate()?;
        if !manifest.permissions.iter().any(|p| p == "media.provider") {
            bail!("media_provider requires media.provider permission")
        }
    }
    Ok(())
}

// YAM payloads declare private library roots explicitly. Do not inherit a
// different package's loader path or guess legacy apkg layout for these plugins.
pub(super) fn library_path(manifest: &Manifest, root: &Path) -> String {
    manifest
        .launch
        .library_paths
        .iter()
        .map(|directory| root.join(directory).display().to_string())
        .collect::<Vec<_>>()
        .join(":")
}

pub(super) struct Prepared {
    pub manifest: Manifest,
    pub root: PathBuf,
    pub lease: Option<Lease>,
}

pub(super) fn prepare(
    id: &str,
    location: PluginLocation,
    yam: Option<&Path>,
    run: Option<&Path>,
) -> Result<Prepared> {
    let lease = if location.source == PluginSource::Yam {
        Some(Lease::acquire(run)?)
    } else {
        None
    };
    let manifest: Manifest =
        toml::from_str(&fs::read_to_string(location.root.join("plugin.toml"))?)?;
    if manifest.id != id {
        bail!("plugin descriptor id does not match requested package")
    }
    let root = if lease.is_some() {
        pinned_root(
            yam.context("YAM root unavailable")?,
            &location.root,
            &manifest,
        )?
    } else {
        location.root
    };
    Ok(Prepared {
        manifest,
        root,
        lease,
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
            let base = std::env::temp_dir().join(format!("luna-yam-binary-{}", Uuid::new_v4()));
            let yam = base.join("yam");
            let run = base.join("run");
            let selected = yam.join("pkgs/example/worker/1.0.0");
            let root = selected.join("share/luna/plugins/example.worker");
            let shared = yam.join("share/luna/plugins/example.worker");
            fs::create_dir_all(root.join("bin")).unwrap();
            fs::create_dir_all(&shared).unwrap();
            fs::create_dir_all(&run).unwrap();
            fs::write(run.join("yam.lock"), "").unwrap();
            fs::write(root.join("plugin.toml"), "id='example.worker'\nname='Worker'\nversion='1.0.0'\nentry='bin/run.sh'\npermissions=['exec']\n[launch]\nkind='binary'\ninterpreter='sh'\n").unwrap();
            fs::write(root.join("bin/run.sh"), "exit 0\n").unwrap();
            symlink("1.0.0", selected.parent().unwrap().join("current")).unwrap();
            symlink(root.join("plugin.toml"), shared.join("plugin.toml")).unwrap();
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
                "example.worker",
                PluginLocation {
                    root: self.shared.clone(),
                    source: PluginSource::Yam,
                    package: Some("example/worker".into()),
                },
                Some(&self.yam),
                Some(&self.run),
            )
        }
        fn exclusive_available(&self) -> bool {
            let file = fs::File::open(self.run.join("yam.lock")).unwrap();
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }
    #[test]
    fn binary_is_pinned_and_lease_blocks_package_transactions() {
        let f = Fixture::new();
        let prepared = f.prepare().unwrap();
        assert_eq!(prepared.root, f.root);
        assert!(!f.exclusive_available());
        drop(prepared);
        assert!(f.exclusive_available());
    }
    #[test]
    fn missing_or_busy_transaction_lock_fails_closed() {
        let f = Fixture::new();
        assert!(Lease::acquire(None).is_err());
        let file = fs::File::open(f.run.join("yam.lock")).unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(f.prepare().is_err());
        drop(file);
        fs::remove_file(f.run.join("yam.lock")).unwrap();
        assert!(f.prepare().is_err());
    }
    #[test]
    fn stale_descriptor_and_foreign_payload_are_rejected() {
        let f = Fixture::new();
        let current = f.yam.join("pkgs/example/worker/current");
        fs::remove_file(&current).unwrap();
        let other = f.yam.join("pkgs/example/worker/2.0.0");
        fs::create_dir_all(&other).unwrap();
        symlink("2.0.0", &current).unwrap();
        assert!(f.prepare().is_err());
        fs::remove_file(&current).unwrap();
        symlink(&f.base, &current).unwrap();
        assert!(f.prepare().is_err());
        assert!(f.exclusive_available());
    }
    #[test]
    fn escaping_launcher_and_descriptor_id_are_rejected() {
        let f = Fixture::new();
        fs::remove_file(f.root.join("bin/run.sh")).unwrap();
        fs::write(f.base.join("foreign.sh"), "exit 0").unwrap();
        symlink(f.base.join("foreign.sh"), f.root.join("bin/run.sh")).unwrap();
        assert!(f.prepare().is_err());
        fs::write(
            f.root.join("plugin.toml"),
            "id='example.other'\nname='Other'\nversion='1.0.0'\nentry='bin/run.sh'\n",
        )
        .unwrap();
        assert!(f.prepare().is_err());
        assert!(f.exclusive_available());
    }
    #[test]
    fn runtime_policy_is_validated_before_execution() {
        let f = Fixture::new();
        let original = fs::read_to_string(f.root.join("plugin.toml")).unwrap();
        for invalid in [
            original.replace("['exec']", "[]"),
            original.replace("interpreter='sh'", "interpreter='python'"),
            original.replace("bin/run.sh", "../outside.sh"),
        ] {
            fs::write(f.root.join("plugin.toml"), invalid).unwrap();
            assert!(f.prepare().is_err());
            assert!(f.exclusive_available());
        }
    }

    #[test]
    fn private_library_roots_are_validated_and_not_inherited() {
        let f = Fixture::new();
        fs::create_dir_all(f.root.join("lib")).unwrap();
        let original = fs::read_to_string(f.root.join("plugin.toml")).unwrap();
        fs::write(
            f.root.join("plugin.toml"),
            format!("{original}library_paths=['lib']\n"),
        )
        .unwrap();
        let prepared = f.prepare().unwrap();
        assert_eq!(
            library_path(&prepared.manifest, &prepared.root),
            f.root.join("lib").display().to_string()
        );
        drop(prepared);
        fs::remove_dir(f.root.join("lib")).unwrap();
        symlink(&f.base, f.root.join("lib")).unwrap();
        assert!(f.prepare().is_err());
        assert!(f.exclusive_available());
    }
}
