//! Package ownership is established by the selected immutable payload, not
//! inferred from the plugin ID. One YAM package may own several Luna plugins.
use super::*;

pub(super) struct Owned {
    pub root: PathBuf,
    pub package: String,
}

/// Caller holds a shared YAM lease when executing code or serving its page.
pub(super) fn resolve(yam: &Path, shared: &Path, id: &str) -> Result<Owned> {
    validate_yam_id(id)?;
    let yam = fs::canonicalize(yam)?;
    let descriptor = fs::canonicalize(shared.join("plugin.toml"))?;
    let packages = yam.join("pkgs");
    let relative = descriptor
        .strip_prefix(&packages)
        .context("YAM plugin descriptor is outside the package store")?;
    let parts: Vec<_> = relative
        .components()
        .map(|part| {
            part.as_os_str()
                .to_str()
                .context("non-UTF8 YAM payload path")
        })
        .collect::<Result<_>>()?;
    // namespace/name/version/share/luna/plugins/id/plugin.toml
    if parts.len() != 8
        || parts[3..6] != ["share", "luna", "plugins"]
        || parts[6] != id
        || parts[7] != "plugin.toml"
    {
        bail!("YAM plugin descriptor is not at a package-owned plugin path")
    }
    validate_yam_id(&format!("{}.{}", parts[0], parts[1]))?;
    if id.split_once('.').map(|p| p.0) != Some(parts[0]) {
        bail!("YAM plugin namespace differs from its owning package")
    }
    let package = packages.join(parts[0]).join(parts[1]);
    let selected = fs::canonicalize(package.join("current"))
        .context("YAM plugin has no selected package version")?;
    if selected.parent() != Some(package.as_path()) || !selected.is_dir() {
        bail!("YAM current selector escapes its package version directory")
    }
    let root = selected.join("share/luna/plugins").join(id);
    if fs::canonicalize(&root)? != root || descriptor != root.join("plugin.toml") {
        bail!("YAM plugin descriptor does not belong to the selected package version")
    }
    Ok(Owned {
        root,
        package: format!("{}/{}", parts[0], parts[1]),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn rejects_cross_namespace_component() {
        let yam = std::env::temp_dir().join(format!("luna-owner-{}", Uuid::new_v4()));
        let package = yam.join("pkgs/foreign/bundle");
        let root = package.join("1.0.0/share/luna/plugins/example.component");
        let shared = yam.join("share/luna/plugins/example.component");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&shared).unwrap();
        fs::write(root.join("plugin.toml"), "").unwrap();
        symlink("1.0.0", package.join("current")).unwrap();
        symlink(root.join("plugin.toml"), shared.join("plugin.toml")).unwrap();
        let error = resolve(&yam, &shared, "example.component").err().unwrap();
        assert!(error.to_string().contains("namespace"));
        fs::remove_dir_all(yam).unwrap();
    }
}
