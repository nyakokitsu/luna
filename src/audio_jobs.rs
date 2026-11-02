//! Bounded, idempotent and cancellable audio. Results survive restart; audio does not.
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_HISTORY: usize = 64;
const TTL: u64 = 24 * 3600;

#[derive(Clone, Serialize, Deserialize, Debug)]
struct Outcome {
    id: String,
    state: String,
    cleanup: String,
    sha256: String,
    bytes: usize,
    created: u64,
    error: Option<String>,
    #[serde(default, skip_serializing)]
    key: String,
}

struct Job {
    value: Outcome,
    cancel: Arc<AtomicBool>,
}
struct Manager {
    root: PathBuf,
    jobs: Mutex<HashMap<String, Job>>,
}
type Shared = Arc<Manager>;
type Playback = dyn Fn(&std::path::Path, &AtomicBool) -> Result<(), String> + Send + Sync;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn terminal(state: &str) -> bool {
    !matches!(state, "queued" | "playing")
}
fn fail(status: StatusCode, text: &'static str) -> Response {
    (status, Json(serde_json::json!({"error":text}))).into_response()
}

impl Manager {
    fn new(root: PathBuf) -> anyhow::Result<Shared> {
        fs::create_dir_all(&root)?;
        let manager = Arc::new(Self {
            root,
            jobs: Mutex::new(HashMap::new()),
        });
        for entry in fs::read_dir(&manager.root)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(data) = fs::read(&path) else { continue };
            let Ok(mut value) = serde_json::from_slice::<Outcome>(&data) else {
                continue;
            };
            if Uuid::parse_str(&value.id).is_err() {
                continue;
            }
            if !terminal(&value.state) {
                value.state = "interrupted".into();
                value.error = Some("runtime restarted".into());
            }
            manager.cleanup(&mut value);
            if now().saturating_sub(value.created) > TTL {
                let _ = fs::remove_file(path);
                continue;
            }
            manager.persist(&value)?;
            manager.jobs.lock().unwrap().insert(
                value.id.clone(),
                Job {
                    value,
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
        }
        // Only this module's generated UUID payloads are eligible for orphan cleanup.
        for entry in fs::read_dir(&manager.root)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) == Some("wav")
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| Uuid::parse_str(s).is_ok())
            {
                let _ = fs::remove_file(path);
            }
        }
        Ok(manager)
    }

    fn persist(&self, value: &Outcome) -> std::io::Result<()> {
        // The private idempotency key is retained on disk, not exposed over HTTP.
        let mut data = serde_json::to_value(value)?;
        data["key"] = serde_json::Value::String(value.key.clone());
        let path = self.root.join(format!("{}.json", value.id));
        let temp = self.root.join(format!("{}.tmp", value.id));
        use std::io::Write;
        let mut file = fs::File::create(&temp)?;
        file.write_all(&serde_json::to_vec(&data)?)?;
        file.sync_all()?;
        fs::rename(temp, path)?;
        fs::File::open(&self.root)?.sync_all()
    }

    fn cleanup(&self, value: &mut Outcome) {
        match fs::remove_file(self.root.join(format!("{}.wav", value.id))) {
            Ok(()) => value.cleanup = "complete".into(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => value.cleanup = "complete".into(),
            Err(_) => value.cleanup = "failed".into(),
        }
    }

    fn submit(
        self: &Arc<Self>,
        key: String,
        bytes: &[u8],
        playback: Arc<Playback>,
    ) -> Result<Outcome, Response> {
        if bytes.len() > MAX_BYTES {
            return Err(fail(StatusCode::PAYLOAD_TOO_LARGE, "WAV exceeds 64 MiB"));
        }
        crate::audio::validate_wav(bytes)
            .map_err(|_| fail(StatusCode::BAD_REQUEST, "invalid or unsupported WAV"))?;
        let sha = format!("{:x}", Sha256::digest(bytes));
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| fail(StatusCode::INTERNAL_SERVER_ERROR, "job state unavailable"))?;
        if let Some(old) = jobs.values().find(|j| j.value.key == key) {
            return if old.value.sha256 == sha {
                Ok(old.value.clone())
            } else {
                Err(fail(
                    StatusCode::CONFLICT,
                    "idempotency key already used for different audio",
                ))
            };
        }
        if jobs.values().any(|j| !terminal(&j.value.state)) {
            return Err(fail(StatusCode::CONFLICT, "audio output is busy"));
        }
        if jobs.len() >= MAX_HISTORY {
            if let Some(id) = jobs
                .values()
                .filter(|j| terminal(&j.value.state))
                .min_by_key(|j| j.value.created)
                .map(|j| j.value.id.clone())
            {
                jobs.remove(&id);
                let _ = fs::remove_file(self.root.join(format!("{id}.json")));
            }
        }
        let id = Uuid::new_v4().to_string();
        let value = Outcome {
            id: id.clone(),
            state: "queued".into(),
            cleanup: "pending".into(),
            sha256: sha,
            bytes: bytes.len(),
            created: now(),
            error: None,
            key,
        };
        let path = self.root.join(format!("{id}.wav"));
        fs::write(&path, bytes).map_err(|_| {
            fail(
                StatusCode::INSUFFICIENT_STORAGE,
                "audio storage unavailable",
            )
        })?;
        if self.persist(&value).is_err() {
            let _ = fs::remove_file(&path);
            return Err(fail(
                StatusCode::INSUFFICIENT_STORAGE,
                "job persistence failed",
            ));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        jobs.insert(
            id.clone(),
            Job {
                value: value.clone(),
                cancel: cancel.clone(),
            },
        );
        drop(jobs);
        let manager = self.clone();
        tokio::task::spawn_blocking(move || {
            {
                let mut jobs = manager.jobs.lock().unwrap();
                if let Some(job) = jobs.get_mut(&id) {
                    job.value.state = "playing".into();
                    let _ = manager.persist(&job.value);
                }
            }
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| playback(&path, &cancel)));
            let mut jobs = manager.jobs.lock().unwrap();
            if let Some(job) = jobs.get_mut(&id) {
                job.value.state = if cancel.load(Ordering::Relaxed) {
                    "cancelled"
                } else if matches!(result, Ok(Ok(()))) {
                    "succeeded"
                } else {
                    "failed"
                }
                .into();
                if job.value.state == "failed" {
                    job.value.error = Some("playback backend failed".into());
                }
                manager.cleanup(&mut job.value);
                let _ = manager.persist(&job.value);
            }
        });
        Ok(value)
    }
}

async fn create(State(manager): State<Shared>, headers: HeaderMap, bytes: Bytes) -> Response {
    let Some(pcm) = crate::audio::playback_pcm() else {
        return fail(
            StatusCode::NOT_IMPLEMENTED,
            "cancellable playback backend unavailable",
        );
    };
    if !crate::audio::available() {
        return fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "playback backend unavailable",
        );
    }
    let key = headers
        .get("idempotency-key")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if key.is_empty()
        || key.len() > 80
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_:.,".contains(&b))
    {
        return fail(StatusCode::BAD_REQUEST, "valid Idempotency-Key required");
    }
    let playback: Arc<Playback> = Arc::new(move |path, cancel| {
        crate::audio::play_alsa_cancel(path, &pcm, cancel).map_err(|_| "playback failed".into())
    });
    let key = key.to_owned();
    match tokio::task::spawn_blocking(move || manager.submit(key, &bytes, playback)).await {
        Ok(Ok(v)) => (StatusCode::ACCEPTED, Json(v)).into_response(),
        Ok(Err(r)) => r,
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, "audio worker failed"),
    }
}
async fn list(State(manager): State<Shared>) -> Response {
    let values: Vec<_> = manager
        .jobs
        .lock()
        .unwrap()
        .values()
        .map(|j| j.value.clone())
        .collect();
    Json(values).into_response()
}
async fn status(State(manager): State<Shared>, Path(id): Path<String>) -> Response {
    match manager.jobs.lock().unwrap().get(&id) {
        Some(job) => Json(job.value.clone()).into_response(),
        None => fail(StatusCode::NOT_FOUND, "audio job not found"),
    }
}
async fn cancel(State(manager): State<Shared>, Path(id): Path<String>) -> Response {
    match manager.jobs.lock().unwrap().get(&id) {
        Some(job) => {
            job.cancel.store(true, Ordering::Relaxed);
            (StatusCode::ACCEPTED, Json(job.value.clone())).into_response()
        }
        None => fail(StatusCode::NOT_FOUND, "audio job not found"),
    }
}
async fn remove(State(manager): State<Shared>, Path(id): Path<String>) -> Response {
    let mut jobs = manager.jobs.lock().unwrap();
    if let Some(job) = jobs.get(&id) {
        if !terminal(&job.value.state) {
            return fail(StatusCode::CONFLICT, "cancel active job before removal");
        }
    }
    let Some(mut job) = jobs.remove(&id) else {
        return fail(StatusCode::NOT_FOUND, "audio job not found");
    };
    manager.cleanup(&mut job.value);
    if job.value.cleanup != "complete" {
        jobs.insert(id, job);
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "audio cleanup failed");
    }
    let _ = fs::remove_file(manager.root.join(format!("{id}.json")));
    StatusCode::NO_CONTENT.into_response()
}

pub(crate) struct Shutdown(Shared);
impl Shutdown {
    pub(crate) async fn stop(&self) {
        for job in self.0.jobs.lock().unwrap().values() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        for _ in 0..250 {
            if self
                .0
                .jobs
                .lock()
                .unwrap()
                .values()
                .all(|j| terminal(&j.value.state))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
pub(crate) fn router(root: PathBuf) -> anyhow::Result<(Router, Shutdown)> {
    let manager = Manager::new(root.join("audio-jobs"))?;
    Ok((
        Router::new()
            .route("/api/audio/jobs", get(list).post(create))
            .route("/api/audio/jobs/:id", get(status).delete(remove))
            .route("/api/audio/jobs/:id/cancel", post(cancel))
            .with_state(manager.clone()),
        Shutdown(manager),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wav() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend(38u32.to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(8000u32.to_le_bytes());
        b.extend(16000u32.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend(2u32.to_le_bytes());
        b.extend([0, 0]);
        b
    }
    async fn done(manager: &Shared, id: &str) -> Outcome {
        for _ in 0..100 {
            let v = manager.jobs.lock().unwrap()[id].value.clone();
            if terminal(&v.state) {
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("job did not finish")
    }
    #[tokio::test]
    async fn success_deduplicates_and_cleans_payload() {
        let root = std::env::temp_dir().join(format!("luna-audio-test-{}", Uuid::new_v4()));
        let m = Manager::new(root.clone()).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let play: Arc<Playback> = Arc::new(move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        let job = m.submit("same".into(), &wav(), play.clone()).unwrap();
        let finished = done(&m, &job.id).await;
        assert_eq!(finished.state, "succeeded");
        assert_eq!(finished.cleanup, "complete");
        assert!(!root.join(format!("{}.wav", job.id)).exists());
        assert_eq!(m.submit("same".into(), &wav(), play).unwrap().id, job.id);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(!serde_json::to_string(&finished).unwrap().contains("same"));
        let reopened = Manager::new(root.clone()).unwrap();
        assert_eq!(reopened.jobs.lock().unwrap()[&job.id].value.key, "same");
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn cancellation_and_failure_are_not_success() {
        let root = std::env::temp_dir().join(format!("luna-audio-test-{}", Uuid::new_v4()));
        let m = Manager::new(root.clone()).unwrap();
        let play: Arc<Playback> = Arc::new(|_, stop| {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err("cancel".into())
        });
        let job = m.submit("cancel".into(), &wav(), play.clone()).unwrap();
        assert!(m.submit("busy".into(), &wav(), play).is_err());
        m.jobs.lock().unwrap()[&job.id]
            .cancel
            .store(true, Ordering::Relaxed);
        assert_eq!(done(&m, &job.id).await.state, "cancelled");
        let job = m
            .submit("fail".into(), &wav(), Arc::new(|_, _| Err("fail".into())))
            .unwrap();
        let result = done(&m, &job.id).await;
        assert_eq!(result.state, "failed");
        assert_eq!(result.cleanup, "complete");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn restart_interrupts_without_replay_and_cleans_orphan() {
        let root = std::env::temp_dir().join(format!("luna-audio-test-{}", Uuid::new_v4()));
        let m = Manager::new(root.clone()).unwrap();
        let id = Uuid::new_v4().to_string();
        let value = Outcome {
            id: id.clone(),
            state: "playing".into(),
            cleanup: "pending".into(),
            sha256: "hash".into(),
            bytes: 44,
            created: now(),
            error: None,
            key: "old".into(),
        };
        m.persist(&value).unwrap();
        fs::write(root.join(format!("{id}.wav")), b"orphan").unwrap();
        drop(m);
        let reopened = Manager::new(root.clone()).unwrap();
        let jobs = reopened.jobs.lock().unwrap();
        assert_eq!(jobs[&id].value.state, "interrupted");
        assert_eq!(jobs[&id].value.cleanup, "complete");
        assert!(!root.join(format!("{id}.wav")).exists());
        drop(jobs);
        fs::remove_dir_all(root).unwrap();
    }
}
