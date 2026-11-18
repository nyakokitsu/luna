//! Generic Luna LED calibration and preview, without plugin-specific transport.
use super::*;

fn valid_presets(presets: &BTreeMap<String, GainRequest>) -> bool {
    presets.len() <= 16
        && presets.iter().all(|(name, gain)| {
            !name.trim().is_empty()
                && name.len() <= 80
                && valid_gain([gain.gain_r, gain.gain_g, gain.gain_b])
        })
}

pub(super) async fn presets(State(app): State<App>, request: Request<Body>) -> Response {
    let path = app.root.join("led-calibration-presets.json");
    if request.method() == Method::GET {
        let data = match fs::read(&path) {
            Ok(data) if data.len() <= 16384 => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => b"{}".to_vec(),
            _ => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "cannot read LED presets")
                    .into_response()
            }
        };
        let parsed = serde_json::from_slice::<BTreeMap<String, GainRequest>>(&data);
        if !parsed.as_ref().is_ok_and(valid_presets) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid saved LED presets",
            )
                .into_response();
        }
        return (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            data,
        )
            .into_response();
    }
    if request.headers().get("x-luna-local").map(|v| v.as_bytes()) != Some(b"1") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let data = match axum::body::to_bytes(request.into_body(), 16384).await {
        Ok(data) => data,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let parsed = serde_json::from_slice::<BTreeMap<String, GainRequest>>(&data);
    if !parsed.as_ref().is_ok_and(valid_presets) {
        return (
            StatusCode::BAD_REQUEST,
            "expected up to 16 named RGB gain presets",
        )
            .into_response();
    }
    if write_atomic(&path, &data).is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "cannot save LED presets").into_response();
    }
    Json(serde_json::json!({"saved":true})).into_response()
}

#[derive(Default)]
pub(super) struct Preview {
    id: Option<String>,
    expires: Option<Instant>,
    colors: Vec<String>,
    zone: Option<LedZone>,
    idle_until: Option<Instant>,
    volume_until: Option<Instant>,
    quiet_until: Option<Instant>,
    volume: Option<u8>,
    drawn: bool,
    error: String,
}
impl Preview {
    pub(super) fn active(&self) -> bool {
        self.id.is_some() && self.expires.is_some_and(|t| Instant::now() < t)
    }
    fn safe(&self) -> bool {
        let now = Instant::now();
        self.active()
            && self.idle_until.is_some_and(|t| now < t)
            && self.volume_until.is_some_and(|t| now < t)
            && !self.quiet_until.is_some_and(|t| now < t)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreviewRequest {
    #[serde(default)]
    id: String,
    #[serde(default)]
    colors: Vec<String>,
    #[serde(default)]
    zone: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GainRequest {
    gain_r: f64,
    gain_g: f64,
    gain_b: f64,
}
pub(super) fn valid_gain(gain: [f64; 3]) -> bool {
    gain.iter()
        .all(|v| v.is_finite() && *v >= 0.05 && *v <= 4.0)
}
pub(super) fn default_gain() -> [f64; 3] {
    [1.0; 3]
}

pub(super) async fn settings(State(app): State<App>, request: Request<Body>) -> Response {
    if request.method() == Method::GET {
        let g = app.led.settings.lock().await.calibration_gain;
        return Json(GainRequest {
            gain_r: g[0],
            gain_g: g[1],
            gain_b: g[2],
        })
        .into_response();
    }
    if request.headers().get("x-luna-local").map(|v| v.as_bytes()) != Some(b"1") {
        return (StatusCode::FORBIDDEN, "X-Luna-Local: 1 required").into_response();
    }
    let bytes = match axum::body::to_bytes(request.into_body(), 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request too large").into_response(),
    };
    let input: GainRequest = match serde_json::from_slice(&bytes) {
        Ok(input) => input,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid gains").into_response(),
    };
    let gain = [input.gain_r, input.gain_g, input.gain_b];
    if !valid_gain(gain) {
        return (StatusCode::BAD_REQUEST, "gains must be 0.05..4").into_response();
    }
    let mut settings = app.settings.lock().await;
    let mut next = settings.clone();
    next.led.calibration_gain = gain;
    let text = match toml::to_string_pretty(&next) {
        Ok(text) => text,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if write_atomic(&app.root.join("settings.toml"), text.as_bytes()).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    *settings = next.clone();
    *app.led.settings.lock().await = next.led;
    Json(serde_json::json!({"saved":true,"restart_required":false})).into_response()
}

async fn zone(led: &Led) -> (LedZone, usize) {
    let count = led.ring_count().await;
    if count > 0 {
        (LedZone::Ring, count)
    } else {
        (LedZone::Logo, led.topology.logo_count)
    }
}

fn select_zone(topology: LedTopology, requested: &str) -> Result<(LedZone, usize)> {
    let request = if requested.is_empty() {
        if topology.ring_count > 0 {
            "ring"
        } else {
            "logo"
        }
    } else {
        requested
    };
    match request {
        "ring" if topology.ring_count > 0 => Ok((LedZone::Ring, topology.ring_count)),
        "logo" if topology.has_logo && topology.logo_count > 0 => {
            Ok((LedZone::Logo, topology.logo_count))
        }
        _ => bail!("requested LED zone is not available"),
    }
}

fn zone_metadata(topology: LedTopology, reason: Option<&str>) -> Vec<serde_json::Value> {
    let mut zones = Vec::new();
    for (name, count) in [
        ("ring", topology.ring_count),
        (
            "logo",
            if topology.has_logo {
                topology.logo_count
            } else {
                0
            },
        ),
    ] {
        if count > 0 {
            zones.push(serde_json::json!({"id":name,"count":count,"supported":reason.is_none(),"reason":reason}));
        }
    }
    zones
}

fn backend_reason(led: &Led) -> Option<&'static str> {
    match &led.backend {
        Some(backend) => backend.calibration_preview_unavailable_reason(),
        None => Some("No qualified native LED backend"),
    }
}

pub(super) async fn status(State(app): State<App>) -> Json<serde_json::Value> {
    let (_, count) = zone(&app.led).await;
    let reason = backend_reason(&app.led);
    let zones = zone_metadata(app.led.topology, reason);
    let model = app.device.lock().await.model.clone();
    let p = app.led.panel_preview.lock().await;
    Json(
        serde_json::json!({"led_count":if reason.is_none(){count}else{0},"model":model,"zones":zones,"unavailable_reason":reason,
        "defaults":{"gain_r":1,"gain_g":1,"gain_b":1},
        "preview":{"active":p.active(),"allowed":p.safe(),"suppressed":if p.safe(){""}else{"Alice/volume or native state unavailable"},"last_error":p.error},
        "scope":"All Luna LED clients (not native maind)","settings_apply":"live","calibration_by_backend":true}),
    )
}

pub(super) async fn preview(State(app): State<App>, request: Request<Body>) -> Response {
    if request.headers().get("x-luna-local").map(|v| v.as_bytes()) != Some(b"1") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let stopping = request.method() == Method::DELETE;
    let body = match axum::body::to_bytes(request.into_body(), 8192).await {
        Ok(body) => body,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let input: PreviewRequest = match serde_json::from_slice(&body) {
        Ok(input) => input,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid preview frame").into_response(),
    };
    let chosen = select_zone(app.led.topology, &input.zone);
    if !stopping && (backend_reason(&app.led).is_some() || chosen.is_err()) {
        return (
            StatusCode::NOT_IMPLEMENTED,
            backend_reason(&app.led).unwrap_or("Requested native LED zone is unavailable"),
        )
            .into_response();
    }
    let mut p = app.led.panel_preview.lock().await;
    if p.active() && p.id.as_deref() != Some(input.id.as_str()) {
        return (StatusCode::CONFLICT, "preview owned by another session").into_response();
    }
    if stopping {
        if p.id.is_some() && p.id.as_deref() != Some(input.id.as_str()) {
            return StatusCode::CONFLICT.into_response();
        }
        p.expires = Some(Instant::now());
        return Json(serde_json::json!({"stopped":true})).into_response();
    }
    let (selected_zone, count) = chosen.expect("validated zone");
    if p.active()
        && p.zone
            .is_some_and(|zone| zone.name() != selected_zone.name())
    {
        return (
            StatusCode::CONFLICT,
            "stop preview before changing LED zone",
        )
            .into_response();
    }
    if input.colors.len() != count || input.colors.iter().any(|v| parse_color(v).is_err()) {
        return (
            StatusCode::BAD_REQUEST,
            "frame must match zone count and contain #RRGGBB colours",
        )
            .into_response();
    }
    let fresh = !p.active();
    let id = if fresh {
        Uuid::new_v4().to_string()
    } else {
        input.id
    };
    if fresh {
        p.idle_until = None;
        p.volume_until = None;
        p.quiet_until = None;
        p.volume = None;
    }
    p.id = Some(id.clone());
    p.colors = input.colors.clone();
    p.zone = Some(selected_zone);
    p.expires = Some(Instant::now() + Duration::from_secs(4));
    drop(p);
    if fresh {
        tokio::spawn(run(app.led.clone(), id.clone()));
    }
    Json(serde_json::json!({"id":id,"ttl_ms":4000,"sent":input.colors})).into_response()
}

async fn release(led: &Led, zone: LedZone) -> Result<()> {
    if let Some(backend) = led.backend.clone() {
        task::spawn_blocking(move || match zone {
            LedZone::Ring => backend.restore_ring_trigger(),
            LedZone::Logo => backend.restore_logo_trigger(),
        })
        .await??;
    }
    Ok(())
}

async fn run(led: Arc<Led>, id: String) {
    let speech_led = led.clone();
    let speech_id = id.clone();
    let speech = tokio::spawn(async move {
        let reader = Arc::new(std::sync::Mutex::new(media::SpeechReader::default()));
        loop {
            let reader = reader.clone();
            let result = task::spawn_blocking(move || {
                reader
                    .lock()
                    .map_err(|_| anyhow!("speech lock poisoned"))?
                    .next()
            })
            .await;
            let mut p = speech_led.panel_preview.lock().await;
            if p.id.as_deref() != Some(&speech_id) || !p.active() {
                break;
            }
            p.idle_until = match result {
                Ok(Ok(Some(value))) if value["phase"] == "idle" => {
                    Some(Instant::now() + Duration::from_secs(2))
                }
                _ => None,
            };
            drop(p);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let volume_led = led.clone();
    let volume_id = id.clone();
    let volume = tokio::spawn(async move {
        loop {
            let value = volume_io::read_current(current_volume).await;
            let mut p = volume_led.panel_preview.lock().await;
            if p.id.as_deref() != Some(&volume_id) || !p.active() {
                break;
            }
            if let Ok(value) = value {
                if p.volume.is_some_and(|old| old != value) {
                    p.quiet_until = Some(Instant::now() + Duration::from_secs(2));
                }
                p.volume = Some(value);
                p.volume_until = Some(Instant::now() + Duration::from_secs(2));
            } else {
                p.volume_until = None;
            }
            drop(p);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    let zone = led.panel_preview.lock().await.zone.unwrap_or(LedZone::Ring);
    let count = match zone {
        LedZone::Ring => led.topology.ring_count,
        LedZone::Logo => led.topology.logo_count,
    };
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tick.tick().await;
        let mut p = led.panel_preview.lock().await;
        if p.id.as_deref() != Some(&id) {
            break;
        }
        if !p.active() {
            if p.drawn {
                let _ = release(&led, zone).await;
            }
            *p = Preview::default();
            break;
        }
        if p.safe() {
            let colors = p.colors.clone();
            match led.draw_zone_unleased(colors, count, zone, false).await {
                Ok(()) => {
                    p.drawn = true;
                    p.error.clear();
                }
                Err(error) => p.error = error.to_string(),
            }
        } else if p.drawn {
            let _ = release(&led, zone).await;
            p.drawn = false;
        }
    }
    speech.abort();
    volume.abort();
}

pub(super) async fn shutdown(led: &Led) {
    let mut p = led.panel_preview.lock().await;
    let zone = p.zone.unwrap_or(LedZone::Ring);
    p.expires = Some(Instant::now());
    if p.drawn {
        let _ = release(led, zone).await;
        p.drawn = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    fn app() -> App {
        let root = std::env::temp_dir().join(format!("luna-led-panel-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let manager = PluginManager::new(root.join("plugins"), None, None).unwrap();
        let mut settings = LedSettings::default();
        settings.count = 3;
        App {
            root,
            started: Instant::now(),
            diagnostics: crate::telemetry::Counters::default(),
            settings: Arc::new(Mutex::new(Settings::default())),
            device: Arc::new(Mutex::new(DeviceProfile {
                model: "test".into(),
                architecture: "test".into(),
                features: vec![],
                source: "test".into(),
                led: LedTopology::default(),
            })),
            led: Arc::new(Led {
                panel_preview: Arc::new(Mutex::new(Preview::default())),
                settings: Arc::new(Mutex::new(settings)),
                backend: Some(Arc::new(Recording::default())),
                color_order: ColorOrder::Rgb,
                calibration: None,
                topology: LedTopology {
                    ring_count: 3,
                    has_logo: false,
                    logo_count: 0,
                },
            }),
            microphone: None,
            clock: None,
            plugins: Arc::new(Mutex::new(manager)),
            runtime_tokens: Arc::new(Mutex::new(HashMap::new())),
            speech_readers: Arc::new(Mutex::new(HashMap::new())),
            alice_readers: Arc::new(Mutex::new(HashMap::new())),
            speech_claims: Arc::new(Mutex::new(HashMap::new())),
            providers: Arc::new(Mutex::new(providers::Broker::default())),
            custom_web: Arc::new(Mutex::new(HashMap::new())),
            runtime_api_url: "http://127.0.0.1:1".into(),
            input_actions: &[],
            input_gestures: &[],
            gpio_inputs: &[],
        }
    }

    async fn request(app: App, route: &str, method: Method, body: &str, local: bool) -> Response {
        let router = Router::new()
            .route("/settings", get(settings).put(settings))
            .route("/presets", get(presets).put(presets))
            .route("/preview", post(preview).delete(preview))
            .with_state(app);
        let mut request = Request::builder()
            .method(method)
            .uri(route)
            .header("Content-Type", "application/json");
        if local {
            request = request.header("X-Luna-Local", "1");
        }
        router
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn gain_http_validates_and_persists_without_changing_binary_opt_in() {
        let app = app();
        let good = r#"{"gain_r":1.5,"gain_g":0.5,"gain_b":1}"#;
        assert_eq!(
            request(app.clone(), "/settings", Method::PUT, good, false)
                .await
                .status(),
            403
        );
        assert_eq!(
            request(app.clone(), "/settings", Method::PUT, good, true)
                .await
                .status(),
            200
        );
        assert_eq!(
            app.led.settings.lock().await.calibration_gain,
            [1.5, 0.5, 1.0]
        );
        let saved: Settings =
            toml::from_str(&fs::read_to_string(app.root.join("settings.toml")).unwrap()).unwrap();
        assert_eq!(saved.led.calibration_gain, [1.5, 0.5, 1.0]);
        assert!(!saved.allow_plugin_binaries);
        for bad in [
            r#"{"gain_r":0,"gain_g":1,"gain_b":1}"#,
            r#"{"gain_r":5,"gain_g":1,"gain_b":1}"#,
            r#"{"gain_r":1,"gain_g":1,"gain_b":1,"exec":"anything"}"#,
        ] {
            assert_eq!(
                request(app.clone(), "/settings", Method::PUT, bad, true)
                    .await
                    .status(),
                400
            );
        }
        assert_eq!(
            request(
                app.clone(),
                "/settings",
                Method::PUT,
                &" ".repeat(1025),
                true
            )
            .await
            .status(),
            413
        );
        assert_eq!(
            app.led.settings.lock().await.calibration_gain,
            [1.5, 0.5, 1.0]
        );
        fs::remove_dir_all(&app.root).unwrap();
    }

    #[tokio::test]
    async fn preset_http_accepts_only_bounded_rgb_objects() {
        let app = app();
        let good = r#"{"neutral":{"gain_r":1,"gain_g":1,"gain_b":1}}"#;
        assert_eq!(
            request(app.clone(), "/presets", Method::PUT, good, false)
                .await
                .status(),
            403
        );
        assert_eq!(
            request(app.clone(), "/presets", Method::PUT, good, true)
                .await
                .status(),
            200
        );
        let response = request(app.clone(), "/presets", Method::GET, "", false).await;
        let body = axum::body::to_bytes(response.into_body(), 16384)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["neutral"]["gain_r"],
            1
        );
        for bad in [
            r#"{"":{"gain_r":1,"gain_g":1,"gain_b":1}}"#,
            r#"{"bad":{"gain_r":1,"gain_g":1,"gain_b":1,"timing_mode":"anything"}}"#,
            r#"{"bad":{"gain_r":0,"gain_g":1,"gain_b":1}}"#,
        ] {
            assert_eq!(
                request(app.clone(), "/presets", Method::PUT, bad, true)
                    .await
                    .status(),
                400
            );
        }
        assert_eq!(
            request(
                app.clone(),
                "/presets",
                Method::PUT,
                &" ".repeat(16385),
                true
            )
            .await
            .status(),
            413
        );
        fs::remove_dir_all(&app.root).unwrap();
    }

    #[tokio::test]
    async fn preview_http_enforces_session_zone_and_frame() {
        let app = app();
        {
            let mut p = app.led.panel_preview.lock().await;
            p.id = Some("owner".into());
            p.zone = Some(LedZone::Ring);
            p.expires = Some(Instant::now() + Duration::from_secs(2));
        }
        let good = r##"{"id":"owner","zone":"ring","colors":["#100000","#100000","#100000"]}"##;
        assert_eq!(
            request(app.clone(), "/preview", Method::POST, good, false)
                .await
                .status(),
            403
        );
        assert_eq!(
            request(app.clone(), "/preview", Method::POST, good, true)
                .await
                .status(),
            200
        );
        assert!(app.led.panel_preview.lock().await.active());
        let wrong = r##"{"id":"owner","zone":"ring","colors":["#100000"]}"##;
        assert_eq!(
            request(app.clone(), "/preview", Method::POST, wrong, true)
                .await
                .status(),
            400
        );
        let foreign = good.replace("owner", "foreign");
        assert_eq!(
            request(app.clone(), "/preview", Method::POST, &foreign, true)
                .await
                .status(),
            409
        );
        assert_eq!(
            request(
                app.clone(),
                "/preview",
                Method::DELETE,
                r#"{"id":"foreign"}"#,
                true
            )
            .await
            .status(),
            409
        );
        assert_eq!(
            request(
                app.clone(),
                "/preview",
                Method::DELETE,
                r#"{"id":"owner"}"#,
                true
            )
            .await
            .status(),
            200
        );
        assert!(!app.led.panel_preview.lock().await.active());
        fs::remove_dir_all(&app.root).unwrap();
    }

    #[test]
    fn standalone_calibration_page_is_not_registered_or_bundled() {
        let gateway = include_str!("gateway.rs");
        assert!(!gateway.contains(".route(\"/calibration\""));
        assert!(!std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("web/led-calibration.html")
            .exists());
        // Removing the standalone UI must not remove the API used by clients.
        for route in [
            "/api/led-panel/status",
            "/api/led-panel/settings",
            "/api/led-panel/preview",
            "/api/led-panel/presets",
        ] {
            assert!(gateway.contains(route), "{route}");
        }
    }

    #[tokio::test]
    async fn gains_also_apply_to_custom_sysfs_clients() {
        let root = std::env::temp_dir().join(format!("luna-led-gains-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let mut settings = LedSettings::default();
        settings.profile = "custom".into();
        settings.count = 1;
        settings.sysfs_root = root.to_string_lossy().into_owned();
        settings.path_template = Some("{root}/{channel}-{index}".into());
        settings.calibration_gain = [2.0, 0.5, 1.0];
        let led = Led {
            panel_preview: Arc::new(Mutex::new(Preview::default())),
            settings: Arc::new(Mutex::new(settings)),
            backend: None,
            color_order: ColorOrder::Rgb,
            calibration: None,
            topology: LedTopology::default(),
        };
        led.draw_zone(vec!["#204080".into()], 1, LedZone::Ring)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(root.join("red-1")).unwrap(), "64");
        assert_eq!(fs::read_to_string(root.join("green-1")).unwrap(), "32");
        assert_eq!(fs::read_to_string(root.join("blue-1")).unwrap(), "128");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn topology_is_not_restricted_to_bench_speakers() {
        for count in [3, 4, 6, 16, 24, 36, 89] {
            let topology = LedTopology {
                ring_count: count,
                has_logo: true,
                logo_count: 6,
            };
            let zones = zone_metadata(topology, None);
            assert_eq!(zones.len(), 2);
            assert_eq!(zones[0]["count"], count);
            assert_eq!(zones[1]["id"], "logo");
            assert!(matches!(
                select_zone(topology, "logo"),
                Ok((LedZone::Logo, 6))
            ));
            assert!(select_zone(topology, "unknown").is_err());
        }
        let logo = LedTopology {
            has_logo: true,
            logo_count: 6,
            ..LedTopology::default()
        };
        assert_eq!(zone_metadata(logo, None)[0]["id"], "logo");
        assert!(matches!(select_zone(logo, ""), Ok((LedZone::Logo, 6))));
        assert!(select_zone(logo, "ring").is_err());
        assert!(zone_metadata(LedTopology::default(), None).is_empty());
        assert_eq!(
            zone_metadata(logo, Some("backend busy"))[0]["supported"],
            false
        );
    }
    #[derive(Default)]
    struct Recording {
        frames: std::sync::Mutex<Vec<Vec<u8>>>,
        releases: std::sync::atomic::AtomicUsize,
    }
    impl LedBackend for Recording {
        fn show(&self, frame: &[u8]) -> Result<()> {
            self.frames.lock().unwrap().push(frame.to_vec());
            Ok(())
        }
        fn restore_ring_trigger(&self) -> Result<()> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[tokio::test]
    async fn generic_preview_owns_luna_writes_and_does_not_double_gain() {
        let backend = Arc::new(Recording::default());
        let mut settings = LedSettings::default();
        settings.calibration_gain = [2.0, 1.0, 1.0];
        let led = Led {
            panel_preview: Arc::new(Mutex::new(Preview::default())),
            settings: Arc::new(Mutex::new(settings)),
            backend: Some(backend.clone()),
            color_order: ColorOrder::Rgb,
            calibration: None,
            topology: LedTopology::default(),
        };
        {
            let mut p = led.panel_preview.lock().await;
            p.id = Some("test".into());
            p.expires = Some(Instant::now() + Duration::from_secs(4));
        }
        led.draw_zone(vec!["#200000".into()], 1, LedZone::Ring)
            .await
            .unwrap();
        led.off_zone(LedZone::Ring).await.unwrap();
        assert!(backend.frames.lock().unwrap().is_empty());
        led.draw_zone_unleased(vec!["#200000".into()], 1, LedZone::Ring, false)
            .await
            .unwrap();
        assert_eq!(backend.frames.lock().unwrap()[0], vec![32, 0, 0]);
        {
            led.panel_preview.lock().await.expires = Some(Instant::now());
        }
        led.draw_zone(vec!["#200000".into()], 1, LedZone::Ring)
            .await
            .unwrap();
        assert_eq!(backend.frames.lock().unwrap()[1], vec![64, 0, 0]);
        release(&led, LedZone::Ring).await.unwrap();
        assert_eq!(backend.releases.load(Ordering::SeqCst), 1);
        assert_eq!(backend.frames.lock().unwrap().len(), 2); // no blank frame over Alice
    }
    #[test]
    fn calibration_gains_are_bounded() {
        assert!(valid_gain([1.0, 0.5, 1.5]));
        assert!(!valid_gain([0.0, 1.0, 1.0]));
        assert!(!valid_gain([f64::NAN, 1.0, 1.0]));
    }
    #[test]
    fn preview_fails_closed_without_both_native_observers() {
        let mut p = Preview::default();
        p.id = Some("test".into());
        p.expires = Some(Instant::now() + Duration::from_secs(4));
        assert!(!p.safe());
        p.idle_until = Some(Instant::now() + Duration::from_secs(1));
        assert!(!p.safe());
        p.volume_until = p.idle_until;
        assert!(p.safe());
        p.quiet_until = p.idle_until;
        assert!(!p.safe());
    }
}
