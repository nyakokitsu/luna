//! Resolve declared oneshot launchers for an active, leased YAM binary caller.
//! No shell commands or provider processes are executed by this endpoint.
use super::*;

#[derive(Debug, Serialize)]
pub(super) struct Launcher {
    pub plugin_id: String,
    pub version: String,
    pub directory: PathBuf,
    pub program: PathBuf,
    pub interpreter: Option<String>,
    pub args: Vec<String>,
    pub library_path: String,
}

pub(super) fn validate_policy(manifest: &Manifest) -> Result<()> {
    if manifest.dependencies.len() > 16 {
        bail!("at most 16 runtime dependency aliases are allowed")
    }
    for (alias, target) in &manifest.dependencies {
        validate_id(alias)?;
        if alias.len() > 64 {
            bail!("runtime dependency alias exceeds 64 bytes")
        }
        validate_yam_id(target)?;
    }
    Ok(())
}

fn active_caller(plugins: &PluginManager, caller: &str) -> Result<()> {
    if !plugins.enabled.contains(caller) || !plugins.running.contains_key(caller) {
        bail!("runtime dependency caller is not active")
    }
    let (location, manifest) = plugins.location_and_manifest(caller)?;
    if location.source != PluginSource::Yam || manifest.launch.kind != LaunchKind::Binary {
        bail!("runtime dependencies require a leased YAM binary caller")
    }
    Ok(())
}

pub(super) async fn resolve(
    State(app): State<App>,
    AxumPath((caller, alias)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Launcher>, ApiError> {
    runtime_permission(&app, &caller, &headers, "exec").await?;
    validate_id(&alias)?;
    let (target, location, yam, run) = {
        let plugins = app.plugins.lock().await;
        active_caller(&plugins, &caller)?;
        let manifest = plugins.manifest(&caller)?;
        validate_policy(&manifest)?;
        let target = manifest
            .dependencies
            .get(&alias)
            .context("runtime dependency alias is not declared")?
            .clone();
        if !plugins.enabled.contains(&target) {
            return Err(ApiError(anyhow!(
                "runtime dependency is disabled or unavailable"
            )));
        }
        let location = plugins.location(&target)?;
        if location.source != PluginSource::Yam {
            return Err(ApiError(anyhow!("runtime dependency must be YAM-owned")));
        }
        (
            target,
            location,
            plugins.yam_root.clone(),
            plugins.yam_run.clone(),
        )
    };
    let requested = target.clone();
    let prepared = task::spawn_blocking(move || {
        yam_binary::prepare(&requested, location, yam.as_deref(), run.as_deref())
    })
    .await??;
    if prepared.manifest.launch.mode != LaunchMode::Oneshot {
        return Err(ApiError(anyhow!(
            "runtime dependency must be a oneshot launcher"
        )));
    }
    let device = app.device.lock().await.clone();
    if let Some(reason) = compatibility_error(&prepared.manifest, &device) {
        return Err(ApiError(anyhow!(
            "runtime dependency is incompatible: {reason}"
        )));
    }
    // An asynchronous resolution must not outlive the invocation that asked for it.
    runtime_permission(&app, &caller, &headers, "exec").await?;
    {
        let plugins = app.plugins.lock().await;
        active_caller(&plugins, &caller)?;
        if !plugins.enabled.contains(&target) {
            return Err(ApiError(anyhow!("runtime dependency was disabled")));
        }
    }
    let manifest = &prepared.manifest;
    Ok(Json(Launcher {
        plugin_id: target,
        version: manifest.version.clone(),
        directory: prepared.root.clone(),
        program: prepared.root.join(manifest.launch.path(manifest)),
        interpreter: manifest.launch.interpreter.clone(),
        args: manifest.launch.args.clone(),
        library_path: yam_binary::library_path(manifest, &prepared.root),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dependency_aliases_and_targets_are_strict() {
        let mut m: Manifest =
            toml::from_str("id='example.caller'\nname='Caller'\nversion='1.0.0'\nentry='run'\n")
                .unwrap();
        m.dependencies
            .insert("station-volume".into(), "station.volume".into());
        assert!(validate_policy(&m).is_ok());
        for (alias, target) in [
            ("../escape", "station.volume"),
            ("volume", "station/volume"),
            ("volume", "station.volume.extra"),
        ] {
            m.dependencies.clear();
            m.dependencies.insert(alias.into(), target.into());
            assert!(validate_policy(&m).is_err());
        }
    }
}
