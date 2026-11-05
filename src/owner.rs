//! Explicitly opted-in owner tools. Never infer firmware partitions or vendor cache paths.
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, process::Stdio, sync::Arc};
use tokio::{io::AsyncReadExt, process::Command};

fn error(code: StatusCode, message: &str) -> Response {
    (code, Json(serde_json::json!({"error":message}))).into_response()
}
fn valid(kind: &str, name: &str) -> bool {
    ["scripts", "firmware"].contains(&kind)
        && !name.starts_with('.')
        && (1..=96).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
async fn inventory(State(root): State<Arc<PathBuf>>) -> Response {
    let mut entries = vec![];
    for kind in ["scripts", "firmware"] {
        if let Ok(dir) = fs::read_dir(root.join(kind)) {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if valid(kind, &name) && entry.file_type().is_ok_and(|t| t.is_file()) {
                    entries.push(serde_json::json!({"kind":kind,"name":name,"bytes":entry.metadata().map(|m|m.len()).unwrap_or(0)}));
                }
            }
        }
    }
    Json(entries).into_response()
}
async fn upload(
    State(root): State<Arc<PathBuf>>,
    Path((kind, name)): Path<(String, String)>,
    bytes: Bytes,
) -> Response {
    if !valid(&kind, &name) {
        return error(StatusCode::BAD_REQUEST, "invalid owner filename");
    }
    if bytes.is_empty() || bytes.len() > 64 * 1024 * 1024 {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file must be 1 byte to 64 MiB",
        );
    }
    let directory = root.join(&kind);
    let target = directory.join(&name);
    // Create-only: replacement requires an explicit delete. No accidental script/firmware overwrite.
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        fs::create_dir_all(&directory)?;
        let existing: u64 = fs::read_dir(&directory)?
            .filter_map(Result::ok)
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        if existing + bytes.len() as u64 > 256 * 1024 * 1024 {
            return Err(std::io::Error::other("owner quota exceeded"));
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&target)?;
        f.write_all(&bytes)?;
        f.sync_all()
    })();
    match result {
        Ok(())=>Json(serde_json::json!({"name":name,"kind":kind,"bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes)),"flashed":false})).into_response(),
        Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists=>error(StatusCode::CONFLICT,"file already exists"),
        Err(_)=>error(StatusCode::INSUFFICIENT_STORAGE,"owner storage unavailable"),
    }
}
async fn download(
    State(root): State<Arc<PathBuf>>,
    Path((kind, name)): Path<(String, String)>,
) -> Response {
    if !valid(&kind, &name) {
        return error(StatusCode::BAD_REQUEST, "invalid filename");
    }
    let path = root.join(kind).join(name);
    if !path
        .symlink_metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= 64 * 1024 * 1024)
    {
        return error(StatusCode::NOT_FOUND, "file unavailable");
    }
    match fs::read(path) {
        Ok(b) => ([("content-type", "application/octet-stream")], b).into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "file unavailable"),
    }
}
async fn delete(
    State(root): State<Arc<PathBuf>>,
    Path((kind, name)): Path<(String, String)>,
) -> Response {
    if !valid(&kind, &name) {
        return error(StatusCode::BAD_REQUEST, "invalid filename");
    }
    match fs::remove_file(root.join(kind).join(name)) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "file unavailable"),
    }
}
#[derive(Deserialize)]
struct Execution {
    command: String,
    #[serde(default = "default_timeout")]
    timeout: u64,
}
fn default_timeout() -> u64 {
    10
}
async fn execute(Json(value): Json<Execution>) -> Response {
    if value.command.is_empty()
        || value.command.len() > 4096
        || value.command.contains('\0')
        || !(1..=30).contains(&value.timeout)
    {
        return error(StatusCode::BAD_REQUEST, "invalid command or timeout");
    }
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", &value.command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let Ok(mut child) = command.spawn() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "shell unavailable");
    };
    let pid = child.id();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let stdout = tokio::spawn(async move {
        let mut b = vec![];
        let _ = out.take(65536).read_to_end(&mut b).await;
        b
    });
    let stderr = tokio::spawn(async move {
        let mut b = vec![];
        let _ = err.take(65536).read_to_end(&mut b).await;
        b
    });
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(value.timeout), child.wait()).await;
    // Kill only this job's process group, including descendants holding the output pipes.
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let timed_out = result.is_err();
    let code = result.ok().and_then(Result::ok).and_then(|s| s.code());
    if timed_out {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    let output = stdout.await.unwrap_or_default();
    let errors = stderr.await.unwrap_or_default();
    Json(serde_json::json!({"exit_code":code,"timed_out":timed_out,"stdout":String::from_utf8_lossy(&output),"stderr":String::from_utf8_lossy(&errors),"output_limit":65536})).into_response()
}
async fn system() -> Response {
    let read = |p: &str| fs::read_to_string(p).unwrap_or_default();
    Json(serde_json::json!({"uptime":read("/proc/uptime"),"memory":read("/proc/meminfo"),"load":read("/proc/loadavg"),"network":read("/proc/net/wireless"),"kernel":read("/proc/version")})).into_response()
}
#[derive(Deserialize)]
struct Script {
    name: String,
    #[serde(default)]
    source: Option<String>,
}
async fn run_script(State(root): State<Arc<PathBuf>>, Json(value): Json<Script>) -> Response {
    if !valid("scripts", &value.name) || !value.name.ends_with(".sh") {
        return error(StatusCode::BAD_REQUEST, "invalid script name");
    }
    let temporary = value.source.is_some();
    let path = if let Some(source) = &value.source {
        if source.len() > 1024 * 1024 || source.contains('\0') {
            return error(StatusCode::BAD_REQUEST, "invalid script source");
        }
        if fs::create_dir_all(root.as_ref()).is_err() {
            return error(
                StatusCode::INSUFFICIENT_STORAGE,
                "script storage unavailable",
            );
        }
        let p = root.join(format!(".script-{}.sh", uuid::Uuid::new_v4()));
        if fs::write(&p, source).is_err() {
            return error(
                StatusCode::INSUFFICIENT_STORAGE,
                "script storage unavailable",
            );
        }
        p
    } else {
        root.join("scripts").join(&value.name)
    };
    if !path
        .symlink_metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= 1024 * 1024)
    {
        return error(StatusCode::NOT_FOUND, "script unavailable");
    }
    let quoted = path.to_string_lossy().replace('\'', "'\\''");
    let result = execute(Json(Execution {
        command: format!("/bin/sh '{quoted}'"),
        timeout: 30,
    }))
    .await;
    if temporary {
        let _ = fs::remove_file(path);
    }
    result
}
pub(crate) fn enabled() -> bool {
    std::env::var("LUNA_ENABLE_OWNER_TOOLS").as_deref() == Ok("1")
}
pub(crate) fn router(root: PathBuf) -> Router {
    Router::new()
        .route("/api/owner/files", get(inventory))
        .route(
            "/api/owner/files/:kind/:name",
            get(download).post(upload).delete(delete),
        )
        .route("/api/owner/execute", post(execute))
        .route("/api/owner/system", get(system))
        .route("/api/owner/scripts/run", post(run_script))
        .with_state(Arc::new(root.join("owner-files")))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn confined_filenames() {
        assert!(valid("scripts", "hello.sh"));
        for n in ["../etc/passwd", "a/b", "a\\b", ".hidden", ""] {
            assert!(!valid("scripts", n));
        }
        assert!(!valid("partitions", "rootfs"));
    }
    #[tokio::test]
    async fn timeout_kills_owned_descendants_and_reaps() {
        let started = std::time::Instant::now();
        let response = execute(Json(Execution {
            command: "sleep 30 & wait".into(),
            timeout: 1,
        }))
        .await;
        let data = axum::body::to_bytes(response.into_body(), 200000)
            .await
            .unwrap();
        let data: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(data["timed_out"], true);
        assert!(started.elapsed().as_secs() < 4);
    }
}
