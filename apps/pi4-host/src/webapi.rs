//! HTTP control API — route surface matches `apps/p4-bridge/main/webapi.c`.

use crate::airplay::AirplayManager;
use crate::capture::CaptureHandle;
use crate::chromecast::ChromecastManager;
use crate::config::Config;
use crate::control::{ControlState, SpeakerTarget};
use crate::mdns_svc::MdnsHub;
use crate::record::Recorder;
use crate::stats::Stats;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Config,
    pub control: Arc<ControlState>,
    pub capture: CaptureHandle,
    pub recorder: Arc<Recorder>,
    pub stats: Arc<Stats>,
    pub mdns: MdnsHub,
    pub airplay: Arc<AirplayManager>,
    pub chromecast: Arc<ChromecastManager>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/status", get(status_get))
        .route("/api/speakers", get(speakers_get))
        .route("/api/stats", get(stats_get))
        .route("/api/ota", get(ota_get))
        .route("/api/record", get(record_get))
        .route("/api/record/files", get(record_files_get))
        .route("/api/record/download", get(record_download_get))
        .route("/api/select", post(select_post))
        .route("/api/play", post(play_post))
        .route("/api/stop", post(stop_post))
        .route("/api/volume", post(volume_post))
        .route("/api/record/start", post(record_start_post))
        .route("/api/record/stop", post(record_stop_post))
        .route("/api/record/format", post(record_format_post))
        .route("/", get(root_get))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn root_get(State(st): State<AppState>) -> Json<Value> {
    Json(json!({
        "name": st.cfg.device_name,
        "version": env!("CARGO_PKG_VERSION"),
        "platform": "raspberry-pi",
        "api": "/api/status",
        "platform": std::env::consts::OS,
    }))
}

async fn status_get(State(st): State<AppState>) -> Json<Value> {
    let targets = st.control.targets();
    let rec = st.recorder.status();
    Json(json!({
        "name": st.cfg.device_name,
        "streaming": st.control.streaming(),
        "volume_db": st.control.volume_db(),
        "rms_dbfs": st.capture.rms_dbfs(),
        "targets": targets,
        "record": rec,
        "platform": std::env::consts::OS,
        "version": env!("CARGO_PKG_VERSION"),
        "airplay": st.airplay.info(&targets),
        "chromecast": st.chromecast.info(&targets),
        "parity": {
            "airplay2": "stub",
            "chromecast": "stub",
            "capture": st.capture.backend(),
            "capture_features": {
                "cpal": cfg!(feature = "cpal"),
                "alsa": cfg!(feature = "alsa"),
            },
            "http_api": "ok",
            "record": "ok",
            "mdns": "ok",
        }
    }))
}

async fn speakers_get(State(st): State<AppState>) -> Json<Value> {
    st.mdns.mark_browsed();
    let speakers = st.mdns.speakers();
    Json(json!({ "speakers": speakers }))
}

async fn stats_get(State(st): State<AppState>) -> Json<Value> {
    let snap = st.stats.snapshot(st.capture.rms_dbfs());
    let rec = st.recorder.status();
    Json(json!({
        "uptime_s": snap.uptime_s,
        "version": snap.version,
        "platform": snap.platform,
        "capture_blocks": snap.capture_blocks,
        "sessions_started": snap.sessions_started,
        "sessions_stopped": snap.sessions_stopped,
        "rms_dbfs": snap.rms_dbfs,
        "sd": {
            "mounted": rec.mounted,
            "free_mb": rec.free_mb,
            "total_mb": rec.total_mb,
            "recording": rec.recording,
        },
        "note": snap.note,
    }))
}

async fn ota_get() -> Json<Value> {
    Json(json!({
        "supported": false,
        "note": "Pi host updates via Buildroot image rebuild or SCP binary, not TCP 5001 OTA",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn record_get(State(st): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(st.recorder.status()).unwrap_or(json!({})))
}

#[derive(Deserialize)]
struct RecordStartBody {
    filename: Option<String>,
}

async fn record_start_post(
    State(st): State<AppState>,
    body: Option<Json<RecordStartBody>>,
) -> Response {
    let filename = body.and_then(|b| b.filename.clone());
    match st.recorder.start(filename) {
        Ok(s) => (StatusCode::OK, Json(serde_json::to_value(s).unwrap())).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn record_stop_post(State(st): State<AppState>) -> Response {
    match st.recorder.stop() {
        Ok(s) => (StatusCode::OK, Json(serde_json::to_value(s).unwrap())).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn record_format_post() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "format not supported on Pi rootfs; wipe the data partition offline",
        })),
    )
}

async fn record_files_get(State(st): State<AppState>) -> Response {
    match st.recorder.list_files() {
        Ok(files) => (
            StatusCode::OK,
            Json(json!({
                "mounted": true,
                "files": files,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "mounted": false, "files": [], "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct DownloadQuery {
    file: String,
}

async fn record_download_get(
    State(st): State<AppState>,
    Query(q): Query<DownloadQuery>,
) -> Response {
    match st.recorder.resolve_download(&q.file) {
        Ok(path) => match tokio::fs::read(&path).await {
            Ok(data) => {
                let filename = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "rec.wav".into());
                (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "audio/wav".to_string()),
                        (
                            header::CONTENT_DISPOSITION,
                            format!("attachment; filename=\"{filename}\""),
                        ),
                    ],
                    data,
                )
                    .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct SelectBody {
    speakers: Vec<SpeakerTarget>,
}

async fn select_post(State(st): State<AppState>, Json(body): Json<SelectBody>) -> Json<Value> {
    st.control.set_targets(body.speakers);
    Json(json!({ "ok": true, "targets": st.control.targets() }))
}

async fn play_post(State(st): State<AppState>) -> Json<Value> {
    st.control.request_play();
    Json(json!({ "ok": true }))
}

async fn stop_post(State(st): State<AppState>) -> Json<Value> {
    st.control.request_stop();
    Json(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct VolumeBody {
    db: f32,
}

async fn volume_post(State(st): State<AppState>, Json(body): Json<VolumeBody>) -> Json<Value> {
    st.control.set_volume_db(body.db);
    st.airplay.set_volume(st.control.volume_db());
    Json(json!({ "ok": true, "volume_db": st.control.volume_db() }))
}
