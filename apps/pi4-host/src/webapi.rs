//! HTTP control API — route surface matches `apps/p4-bridge/main/webapi.c`
//! plus Pi management: settings, collection, music ID, embedded web UI.

use crate::airplay::AirplayManager;
use crate::capture::CaptureHandle;
use crate::chromecast::ChromecastManager;
use crate::collection;
use crate::config::Config;
use crate::control::{ControlState, SpeakerTarget};
use crate::mdns_svc::MdnsHub;
use crate::music_id;
use crate::record::Recorder;
use crate::settings::{SettingsPatch, SettingsStore};
use crate::stats::Stats;
use crate::stream_tap::{self, StreamParams};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    #[allow(dead_code)]
    pub cfg: Config,
    pub control: Arc<ControlState>,
    pub capture: CaptureHandle,
    pub recorder: Arc<Recorder>,
    pub stats: Arc<Stats>,
    pub mdns: MdnsHub,
    pub airplay: Arc<AirplayManager>,
    pub chromecast: Arc<ChromecastManager>,
    pub settings: Arc<SettingsStore>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        // Management UI (embedded)
        .route("/", get(ui_get))
        .route("/ui", get(ui_get))
        .route("/ui/", get(ui_get))
        // Live capture stream (ESP capture_tap / Chromecast PCM URL parity)
        .route("/capture.wav", get(stream_get))
        .route("/stream", get(stream_get))
        .route("/stream.wav", get(stream_get))
        // Core API (p4-bridge parity)
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
        // Management extensions
        .route(
            "/api/settings",
            get(settings_get).put(settings_put).post(settings_put),
        )
        .route("/api/collection", get(collection_get))
        .route("/api/record/identify", post(record_identify_post))
        .route("/api/record/tag", post(record_tag_post))
        .route("/api", get(api_root_get))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

const UI_HTML: &str = include_str!("../static/index.html");

async fn ui_get() -> Html<&'static str> {
    Html(UI_HTML)
}

async fn api_root_get(State(st): State<AppState>) -> Json<Value> {
    Json(json!({
        "name": st.settings.get().device_name,
        "version": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        "api": "/api/status",
        "ui": "/",
        "endpoints": [
            "GET /api/status",
            "GET /api/speakers",
            "POST /api/select",
            "POST /api/play",
            "POST /api/stop",
            "POST /api/volume",
            "GET|PUT /api/settings",
            "GET /api/record",
            "POST /api/record/start",
            "POST /api/record/stop",
            "GET /api/record/files",
            "GET /api/record/download?file=",
            "POST /api/record/identify",
            "GET /api/collection",
            "GET /api/stats",
            "GET /capture.wav",
            "GET /stream",
            "GET /stream.wav",
            "GET :8088/capture.wav",
        ]
    }))
}

async fn stream_get(State(st): State<AppState>) -> Response {
    let params = StreamParams::from_config(&st.cfg);
    stream_tap::axum_live_response(st.capture.clone(), params)
}

fn status_json(st: &AppState) -> Value {
    let targets = st.control.targets();
    let mut rec = st.recorder.status();
    let settings = st.settings.get();
    rec.auto_record = settings.auto_record;
    json!({
        "name": settings.device_name,
        "streaming": st.control.streaming(),
        "volume_db": st.control.volume_db(),
        "rms_dbfs": st.capture.rms_dbfs(),
        "targets": targets,
        "record": rec,
        "platform": std::env::consts::OS,
        "version": env!("CARGO_PKG_VERSION"),
        "airplay": st.airplay.info(&targets),
        "chromecast": st.chromecast.info(&targets),
        "settings": {
            "auto_record": settings.auto_record,
            "music_id_enabled": settings.music_id_enabled,
            "music_id_ready": settings.music_id_ready,
            "force_open": settings.force_open,
            "record_dir": settings.record_dir,
        },
        "parity": {
            "airplay2": format!("live/{}", st.airplay.transport_label()),
            "chromecast": "ok",
            "capture": st.capture.backend(),
            "capture_features": {
                "cpal": cfg!(feature = "cpal"),
                "alsa": cfg!(feature = "alsa"),
            },
            "http_api": "ok",
            "record": "ok",
            "mdns": "ok",
            "settings": "ok",
            "music_id": if settings.music_id_ready { "ready" } else { "degraded" },
            "web_ui": "ok",
            "capture_stream": "ok",
        },
        "capture_stream": {
            "port": st.cfg.stream_port,
            "path": "/capture.wav",
            "format": "audio/wav",
            "sample_rate": stream_tap::STREAM_RATE,
            "channels": stream_tap::STREAM_CHANNELS,
            "bits": stream_tap::STREAM_BITS,
            "urls": [
                format!("http://0.0.0.0:{}/capture.wav", st.cfg.stream_port),
                "/capture.wav",
                "/stream",
                "/stream.wav",
            ],
        }
    })
}

async fn status_get(State(st): State<AppState>) -> Json<Value> {
    Json(status_json(&st))
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

async fn settings_get(State(st): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(st.settings.get()).unwrap_or(json!({})))
}

async fn settings_put(State(st): State<AppState>, Json(body): Json<SettingsPatch>) -> Response {
    match st.settings.patch(body) {
        Ok(s) => {
            // Keep control volume in sync when settings change it.
            st.control.set_volume_db(s.volume_db);
            st.airplay.set_volume(s.volume_db);
            (StatusCode::OK, Json(serde_json::to_value(s).unwrap())).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn collection_get(State(st): State<AppState>) -> Response {
    match collection::list_collection(&st.recorder) {
        Ok(entries) => (
            StatusCode::OK,
            Json(json!({
                "record_dir": st.recorder.record_dir(),
                "entries": entries,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "entries": [], "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn record_get(State(st): State<AppState>) -> Json<Value> {
    let mut rec = st.recorder.status();
    rec.auto_record = st.settings.get().auto_record;
    Json(serde_json::to_value(rec).unwrap_or(json!({})))
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
        Ok(mut s) => {
            s.auto_record = st.settings.get().auto_record;
            (StatusCode::OK, Json(serde_json::to_value(s).unwrap())).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn record_stop_post(State(st): State<AppState>) -> Response {
    match st.recorder.stop() {
        Ok(mut s) => {
            s.auto_record = st.settings.get().auto_record;
            // Kick music ID if configured (manual stop path).
            let settings = st.settings.get();
            if let Some(name) = st.recorder.peek_last_finished() {
                if settings.music_id_enabled {
                    let dir = st.recorder.record_dir().to_path_buf();
                    // leave name in last_finished for clients; spawn with clone
                    music_id::spawn_identify(dir, name, settings);
                } else if let Some(name) = st.recorder.take_last_finished() {
                    let _ = collection::mark_skipped(
                        st.recorder.record_dir(),
                        &name,
                        "music_id_enabled is false",
                    );
                }
            }
            (StatusCode::OK, Json(serde_json::to_value(s).unwrap())).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct IdentifyBody {
    file: Option<String>,
}

async fn record_identify_post(
    State(st): State<AppState>,
    body: Option<Json<IdentifyBody>>,
) -> Response {
    let name = body.and_then(|b| b.file.clone()).or_else(|| {
        st.recorder
            .peek_last_finished()
            .or_else(|| {
                st.recorder
                    .list_files()
                    .ok()
                    .and_then(|f| f.into_iter().next().map(|x| x.name))
            })
    });
    let Some(name) = name else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no file specified and no recordings found" })),
        )
            .into_response();
    };
    let settings = st.settings.get();
    let dir = st.recorder.record_dir().to_path_buf();
    match music_id::identify_file(&dir, &name, &settings).await {
        Ok(r) => (
            StatusCode::OK,
            Json(serde_json::to_value(r.meta).unwrap_or(json!({}))),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct TagBody {
    file: String,
    artist: Option<String>,
    title: Option<String>,
    album: Option<String>,
}

async fn record_tag_post(State(st): State<AppState>, Json(body): Json<TagBody>) -> Response {
    let dir = st.recorder.record_dir();
    match music_id::tag_manual(
        dir,
        &body.file,
        body.artist,
        body.title,
        body.album,
    ) {
        Ok(meta) => (
            StatusCode::OK,
            Json(serde_json::to_value(meta).unwrap_or(json!({}))),
        )
            .into_response(),
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
        Ok(files) => {
            // Enrich with tags when sidecars exist
            let dir = st.recorder.record_dir();
            let enriched: Vec<Value> = files
                .into_iter()
                .map(|f| {
                    let meta = collection::load_meta(dir, &f.name);
                    json!({
                        "name": f.name,
                        "size": f.size,
                        "mtime": f.mtime,
                        "artist": meta.as_ref().and_then(|m| m.artist.clone()),
                        "title": meta.as_ref().and_then(|m| m.title.clone()),
                        "album": meta.as_ref().and_then(|m| m.album.clone()),
                        "status": meta.as_ref().map(|m| m.status.clone()).unwrap_or_else(|| "untagged".into()),
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(json!({
                    "mounted": true,
                    "files": enriched,
                })),
            )
                .into_response()
        }
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
    // Android/ESP clients often omit tsid/model; fill from the live mDNS cache
    // so stereo pairs (shared tsid) still group correctly on play.
    let discovered = st.mdns.speakers();
    let speakers = body
        .speakers
        .into_iter()
        .map(|mut s| {
            let hit = discovered.iter().find(|d| {
                (!s.device_id.is_empty()
                    && !d.device_id.is_empty()
                    && d.device_id.eq_ignore_ascii_case(&s.device_id))
                    || (d.ip == s.ip && d.port == s.port)
                    || (d.name.eq_ignore_ascii_case(&s.name)
                        && !s.name.is_empty()
                        && d.target_type == s.target_type)
            });
            if let Some(d) = hit {
                if s.tight_sync_id.trim().is_empty() && !d.tight_sync_id.is_empty() {
                    s.tight_sync_id = d.tight_sync_id.clone();
                }
                if s.model.as_ref().map(|m| m.is_empty()).unwrap_or(true) {
                    s.model = d.model.clone();
                }
                if s.device_id.is_empty() {
                    s.device_id = d.device_id.clone();
                }
                // Prefer clean discovered name over stale/raw client names.
                if s.name.contains("._") || s.name.ends_with(".local.") {
                    s.name = d.name.clone();
                }
            }
            s
        })
        .collect();
    st.control.set_targets(speakers);
    // Return full status so Android BridgeStatus.fromJson works.
    Json(status_json(&st))
}

async fn play_post(State(st): State<AppState>) -> Json<Value> {
    st.control.request_play();
    Json(status_json(&st))
}

async fn stop_post(State(st): State<AppState>) -> Json<Value> {
    st.control.request_stop();
    Json(status_json(&st))
}

#[derive(Deserialize)]
struct VolumeBody {
    db: f32,
}

async fn volume_post(State(st): State<AppState>, Json(body): Json<VolumeBody>) -> Json<Value> {
    st.control.set_volume_db(body.db);
    st.airplay.set_volume(st.control.volume_db());
    // Keep settings volume aligned
    let _ = st.settings.patch(SettingsPatch {
        volume_db: Some(st.control.volume_db()),
        ..Default::default()
    });
    Json(status_json(&st))
}
