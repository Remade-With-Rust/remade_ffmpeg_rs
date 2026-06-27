//! `rff-server` — the HTTP API in front of the `rff` engine.
//!
//! "API first": this server exposes the same engine the CLI uses, so AI agents
//! and remote tools get first-class access to probe/transcode without shelling
//! out. Every mutating route is gated by an [`Authenticator`] (a MATA mID
//! verifier in production; a dev stub locally).
//!
//! Routes:
//! * `GET  /healthz`        — liveness probe
//! * `GET  /v1/codecs`      — list supported codecs
//! * `GET  /v1/formats`     — list supported container formats
//! * `POST /v1/probe`       — inspect a media file `{ "path": "..." }`
//! * `POST /v1/transcode`   — run a transcode job (auth required)

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;
use rff_auth::{Authenticator, DevAllowAll};
use rff_core::{CodecId, Dictionary};
use serde::Deserialize;
use serde_json::{json, Value};

/// Shared application state handed to every handler.
struct AppState {
    engine: Engine,
    auth: Box<dyn Authenticator>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Auth backend: dev stub by default. Swap for a MATA mID verifier
    // (rff-auth `mata-mid` feature) in production deployments.
    let state = Arc::new(AppState {
        engine: Engine::new(),
        auth: Box::new(DevAllowAll),
    });

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/codecs", get(list_codecs))
        .route("/v1/formats", get(list_formats))
        .route("/v1/probe", post(probe))
        .route("/v1/transcode", post(transcode))
        .with_state(state);

    let addr: SocketAddr = std::env::var("RFF_SERVER_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()?;

    tracing::info!("rff-server listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn list_codecs(State(state): State<Arc<AppState>>) -> Json<Value> {
    let codecs: Vec<Value> = state
        .engine
        .codecs
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "long_name": c.long_name,
                "media_type": c.media_type.to_string(),
                "decode": c.can_decode(),
                "encode": c.can_encode(),
            })
        })
        .collect();
    Json(json!({ "codecs": codecs }))
}

async fn list_formats(State(state): State<Arc<AppState>>) -> Json<Value> {
    let formats: Vec<Value> = state
        .engine
        .formats
        .iter()
        .map(|f| {
            json!({
                "name": f.name,
                "long_name": f.long_name,
                "extensions": f.extensions,
                "demux": f.can_demux(),
                "mux": f.can_mux(),
            })
        })
        .collect();
    Json(json!({ "formats": formats }))
}

#[derive(Deserialize)]
struct ProbeRequest {
    path: String,
}

async fn probe(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ProbeRequest>,
) -> Result<Json<Value>, ApiError> {
    let info = rff::probe::probe(&state.engine, &req.path)?;
    let streams: Vec<Value> = info
        .streams
        .iter()
        .map(|s| {
            json!({
                "index": s.index,
                "codec_type": s.media_type.to_string(),
                "codec_name": s.codec_id.name(),
                "width": s.width,
                "height": s.height,
                "sample_rate": s.sample_rate,
                "channels": s.channels,
            })
        })
        .collect();
    Ok(Json(json!({
        "format_name": info.format_name,
        "streams": streams,
    })))
}

#[derive(Deserialize)]
struct TranscodeRequest {
    input: String,
    output: String,
    #[serde(default)]
    input_format: Option<String>,
    #[serde(default)]
    output_format: Option<String>,
    #[serde(default)]
    video_codec: Option<String>,
    #[serde(default)]
    audio_codec: Option<String>,
    #[serde(default)]
    video_filters: Option<String>,
    #[serde(default)]
    overwrite: bool,
}

async fn transcode(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<TranscodeRequest>,
) -> Result<Json<Value>, ApiError> {
    // Transcoding mutates the filesystem, so it requires a verified caller.
    let identity = authenticate(&*state.auth, &headers)?;
    tracing::info!(subject = %identity.subject, "transcode requested");

    let video_codec = resolve_codec(req.video_codec.as_deref())?;
    let audio_codec = resolve_codec(req.audio_codec.as_deref())?;

    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: req.input.into(),
            format: req.input_format,
        }],
        outputs: vec![OutputSpec {
            path: req.output.into(),
            format: req.output_format,
            video_codec: video_codec.map(|codec| StreamCodec {
                codec,
                options: Dictionary::new(),
            }),
            audio_codec: audio_codec.map(|codec| StreamCodec {
                codec,
                options: Dictionary::new(),
            }),
            video_filters: req.video_filters,
            maps: Vec::new(),
            overwrite: req.overwrite,
        }],
    };

    let report = rff::transcode::run(&state.engine, &spec)?;
    Ok(Json(json!({
        "status": "completed",
        "packets_written": report.packets_written,
        "frames_decoded": report.frames_decoded,
    })))
}

/// Map an optional codec name to a [`CodecId`], rejecting unknown names.
fn resolve_codec(name: Option<&str>) -> Result<Option<CodecId>, ApiError> {
    match name {
        None => Ok(None),
        Some(n) => CodecId::from_name(n)
            .map(Some)
            .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, format!("unknown codec `{n}`"))),
    }
}

/// Pull the bearer credential from `Authorization` and verify it.
fn authenticate(
    auth: &dyn Authenticator,
    headers: &HeaderMap,
) -> Result<rff_auth::Identity, ApiError> {
    let credential = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v))
        .unwrap_or_default();
    auth.authenticate(credential)
        .map_err(|e| ApiError(StatusCode::UNAUTHORIZED, e.to_string()))
}

/// A simple (status, message) API error that serializes to a JSON body.
struct ApiError(StatusCode, String);

impl From<rff_core::Error> for ApiError {
    fn from(err: rff_core::Error) -> Self {
        // Engine "not yet implemented" maps to 501; everything else to 422.
        let status = match err {
            rff_core::Error::Unimplemented(_) => StatusCode::NOT_IMPLEMENTED,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        };
        ApiError(status, err.to_string())
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}
