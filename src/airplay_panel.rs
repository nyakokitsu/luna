//! A local calibration UI, backed by the AirPlay supervisor's protected Unix
//! socket. No ADB, runtime token extraction, shell commands or new LAN listener.
use super::*;

const SOCKET: &str = "/run/station-airplay/control.sock";
const LIMIT: usize = 16384;

pub(super) async fn page() -> Html<&'static str> {
    Html(include_str!("../web/calibration.html"))
}

fn route_allowed(operation: &str, method: &Method) -> bool {
    matches!(
        (operation, method.as_str()),
        ("settings", "GET" | "PUT")
            | ("status", "GET")
            | ("preview", "POST" | "DELETE")
            | ("presets", "GET" | "PUT")
    )
}

pub(super) async fn proxy(
    State(app): State<App>,
    AxumPath(operation): AxumPath<String>,
    request: Request<Body>,
) -> Response {
    let method = request.method().clone();
    if operation == "preview"
        && method == Method::POST
        && app.led.panel_preview.lock().await.active()
    {
        return (
            StatusCode::CONFLICT,
            "stop the generic Luna LED preview first",
        )
            .into_response();
    }
    if !route_allowed(&operation, &method) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            "unsupported panel operation",
        )
            .into_response();
    }
    if method != Method::GET
        && request.headers().get("x-luna-local").map(|v| v.as_bytes()) != Some(b"1")
    {
        return (StatusCode::FORBIDDEN, "X-Luna-Local: 1 required").into_response();
    }
    let body = match axum::body::to_bytes(request.into_body(), LIMIT).await {
        Ok(body) => body,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "panel request exceeds 16 KiB",
            )
                .into_response()
        }
    };
    if operation == "presets" {
        let path = app.root.join("calibration-presets.json");
        if method == Method::GET {
            let data = match fs::read(&path) {
                Ok(data) if data.len() <= LIMIT => data,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => b"{}".to_vec(),
                _ => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "cannot read presets")
                        .into_response()
                }
            };
            return (
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                data,
            )
                .into_response();
        }
        let value: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => return (StatusCode::BAD_REQUEST, "invalid preset JSON").into_response(),
        };
        if !valid_presets(&value) {
            return (
                StatusCode::BAD_REQUEST,
                "expected up to 16 named preset objects",
            )
                .into_response();
        }
        if write_atomic(&path, &body).is_err() {
            return (StatusCode::INTERNAL_SERVER_ERROR, "cannot save presets").into_response();
        }
        return Json(serde_json::json!({"saved":true})).into_response();
    }
    #[cfg(unix)]
    {
        let client = match reqwest::Client::builder()
            .unix_socket(SOCKET)
            .timeout(Duration::from_secs(4))
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(client) => client,
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "panel transport unavailable",
                )
                    .into_response()
            }
        };
        let mut reply = match client
            .request(method, format!("http://localhost/{operation}"))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
        {
            Ok(reply) => reply,
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "start a compatible AirPlay plugin with controls enabled",
                )
                    .into_response()
            }
        };
        let status = reply.status();
        let mut output = Vec::new();
        loop {
            match reply.chunk().await {
                Ok(Some(chunk)) if output.len() + chunk.len() <= LIMIT => {
                    output.extend_from_slice(&chunk)
                }
                Ok(None) => break,
                _ => return (StatusCode::BAD_GATEWAY, "invalid panel response").into_response(),
            }
        }
        (
            status,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            output,
        )
            .into_response()
    }
    #[cfg(not(unix))]
    (StatusCode::NOT_IMPLEMENTED, "Unix transport required").into_response()
}

fn valid_presets(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|presets| {
        presets.len() <= 16
            && presets.iter().all(|(name, config)| {
                !name.trim().is_empty() && name.len() <= 80 && config.is_object()
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panel_routes_are_fixed() {
        assert!(route_allowed("settings", &Method::PUT));
        assert!(route_allowed("preview", &Method::DELETE));
        assert!(!route_allowed("../../etc/passwd", &Method::GET));
        assert!(!route_allowed("status", &Method::POST));
    }
    #[test]
    fn presets_have_bounded_schema() {
        assert!(valid_presets(
            &serde_json::json!({"neutral":{"spectrum_gain_r":1}})
        ));
        assert!(!valid_presets(&serde_json::json!({"":"oops"})));
        assert!(!valid_presets(&serde_json::json!([])));
    }
}
