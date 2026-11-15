use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::net::TcpListener;
use tracing::info;

use crate::config::Destination;
use crate::deploy::{
    CredentialFields, DeployError, DeployRequest, DeployService, DeploymentRecord, ReapReport,
    ReapRequest, RelaySeed,
};
use crate::ffmpeg::{ffmpeg_bin, ffmpeg_has_burnin_filter};
use crate::redact::redact_key;
use crate::state::{drops, BandwidthStatus, FfmpegStatus, IngestStatus, Shared, StatusPayload};

const CLOUD_PAGE: &str = include_str!("deploy/ui.html");

#[derive(Clone)]
pub struct ControlCtx {
    pub shared: Shared,
    pub deploy: Option<Arc<DeployService>>,
}

impl ControlCtx {
    pub fn from_shared(shared: Shared) -> Self {
        let enabled = std::env::var("ECHOCHAMBER_DISABLE_DEPLOY").ok().as_deref() != Some("1");
        let deploy = if enabled {
            Some(Arc::new(DeployService::from_env()))
        } else {
            None
        };
        Self { shared, deploy }
    }
}

pub fn router(ctx: ControlCtx) -> Router {
    let mut app = Router::new()
        .route("/status", get(handle_status))
        .route("/reload", post(handle_reload))
        .route("/stop", post(handle_stop))
        .route("/push", post(handle_push));
    if ctx.deploy.is_some() {
        app = app
            .route("/cloud", get(handle_cloud_page))
            .route("/deploy", post(handle_deploy))
            .route("/deployments", get(handle_deployments))
            .route("/deployments/{id}", get(handle_deployment))
            .route("/deployments/{id}/teardown", post(handle_teardown))
            .route("/reap", post(handle_reap));
    }
    app.with_state(ctx)
}

pub async fn run(shared: Shared) -> anyhow::Result<()> {
    let bind: SocketAddr = shared.config.borrow().control.bind.parse()?;
    let stop = Arc::clone(&shared.shutdown);
    let app = router(ControlCtx::from_shared(shared));

    let listener = TcpListener::bind(bind).await?;
    info!(%bind, "control HTTP listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            stop.notified().await;
        })
        .await?;
    Ok(())
}

struct Authed;

impl FromRequestParts<ControlCtx> for Authed {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ControlCtx,
    ) -> Result<Self, Self::Rejection> {
        if authorize(&state.shared, &parts.headers) {
            Ok(Authed)
        } else {
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

fn authorize(shared: &Shared, headers: &HeaderMap) -> bool {
    let token = shared.config.borrow().control.token.clone();
    if token.is_empty() {
        return true;
    }
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let rest = presented.strip_prefix("Bearer ").unwrap_or("");
    tokens_eq(rest, &token)
}

fn tokens_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut diff = a.len() ^ b.len();
    let len = a.len().max(b.len());
    for i in 0..len {
        let left = a.get(i).copied().unwrap_or(0);
        let right = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(left ^ right);
    }
    diff == 0 && !a.is_empty()
}

#[axum::debug_handler]
async fn handle_status(State(ctx): State<ControlCtx>, Authed: Authed) -> Json<StatusPayload> {
    let shared = &ctx.shared;
    let cfg = shared.config.borrow().clone();
    let publishing = *shared.publishing.borrow();
    let dest_cfg = cfg.configured_destinations().len();
    let mut pushers = shared.pushers.lock().await.clone();
    {
        let meters = shared.out_meters.lock().expect("out_meters");
        for p in pushers.iter_mut() {
            if let Some(m) = meters.get(&p.name) {
                let r = m.snapshot();
                p.bytes_out = r.bytes;
                p.bps = r.bps;
                p.bps_avg = r.bps_avg;
            }
        }
    }
    let asr = shared.asr.lock().await.clone();
    let adhoc = shared.adhoc.lock().await.len();
    let inn = shared.ingest.snapshot();
    let push_bytes: u64 = pushers.iter().map(|p| p.bytes_out).sum();
    let push_bps: f64 = pushers.iter().map(|p| p.bps).sum();
    let push_bps_avg: f64 = pushers.iter().map(|p| p.bps_avg).sum();
    Json(StatusPayload {
        ingest: IngestStatus {
            bind: cfg.ingest.bind.clone(),
            publishing,
            standby: shared
                .standby_active
                .load(std::sync::atomic::Ordering::Relaxed),
            play_url: cfg.local_rtmp_url(),
            stream_key: redact_key(&cfg.ingest.stream_key),
        },
        pushers,
        asr,
        asr_drops: drops(shared),
        ffmpeg: FfmpegStatus {
            bin: ffmpeg_bin().display().to_string(),
            burn_in_available: ffmpeg_has_burnin_filter(),
        },
        destinations: dest_cfg + adhoc,
        bandwidth: BandwidthStatus {
            window_ms: inn.window_ms.max(1),
            ingest_bytes: inn.bytes,
            ingest_bps: inn.bps,
            ingest_bps_avg: inn.bps_avg,
            push_bytes,
            push_bps,
            push_bps_avg,
        },
    })
}

async fn handle_reload(
    State(ctx): State<ControlCtx>,
    Authed: Authed,
) -> Result<String, (StatusCode, String)> {
    let shared = &ctx.shared;
    let cfg = crate::config::Config::load_from_path(&shared.config_path)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let _ = shared.config_tx.send(cfg);
    info!("config reloaded from {}", shared.config_path.display());
    Ok("reloaded\n".into())
}

async fn handle_stop(State(ctx): State<ControlCtx>, Authed: Authed) -> String {
    info!("stop requested");
    ctx.shared.request_stop();
    "stopping\n".into()
}

#[derive(Debug, Deserialize)]
struct PushBody {
    url: String,
    name: Option<String>,
}

async fn handle_push(
    State(ctx): State<ControlCtx>,
    Authed: Authed,
    Json(body): Json<PushBody>,
) -> Result<String, (StatusCode, String)> {
    let shared = &ctx.shared;
    if body.url.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "url required".into()));
    }
    let name = body
        .name
        .unwrap_or_else(|| format!("adhoc-{}", chrono_like_id()));
    shared.adhoc.lock().await.push(Destination {
        name: name.clone(),
        url: body.url,
    });
    let cfg = shared.config.borrow().clone();
    let _ = shared.config_tx.send(cfg);
    Ok(format!("added destination {name}\n"))
}

async fn handle_cloud_page() -> axum::response::Html<&'static str> {
    axum::response::Html(CLOUD_PAGE)
}

async fn handle_deploy(
    State(ctx): State<ControlCtx>,
    Authed: Authed,
    Json(body): Json<DeployRequest>,
) -> Result<Json<DeploymentRecord>, (StatusCode, Json<serde_json::Value>)> {
    let deploy = deploy_service(&ctx)?;
    let cfg = ctx.shared.config.borrow().clone();
    let seed = RelaySeed {
        platforms: cfg.platforms,
        quality: cfg.quality,
    };
    let record = deploy.deploy(body, seed).await.map_err(json_error)?;
    Ok(Json(record))
}

async fn handle_deployments(
    State(ctx): State<ControlCtx>,
    Authed: Authed,
) -> Result<Json<Vec<DeploymentRecord>>, (StatusCode, Json<serde_json::Value>)> {
    let deploy = deploy_service(&ctx)?;
    deploy.list().map(Json).map_err(json_error)
}

async fn handle_deployment(
    State(ctx): State<ControlCtx>,
    Authed: Authed,
    Path(id): Path<String>,
) -> Result<Json<DeploymentRecord>, (StatusCode, Json<serde_json::Value>)> {
    let deploy = deploy_service(&ctx)?;
    deploy.get(&id).map(Json).map_err(json_error)
}

async fn handle_teardown(
    State(ctx): State<ControlCtx>,
    Authed: Authed,
    Path(id): Path<String>,
    body: Option<Json<CredentialFields>>,
) -> Result<Json<DeploymentRecord>, (StatusCode, Json<serde_json::Value>)> {
    let deploy = deploy_service(&ctx)?;
    let fields = body.map(|json| json.0).unwrap_or_default();
    deploy
        .teardown(&id, fields)
        .await
        .map(Json)
        .map_err(json_error)
}

async fn handle_reap(
    State(ctx): State<ControlCtx>,
    Authed: Authed,
    Json(body): Json<ReapRequest>,
) -> Result<Json<ReapReport>, (StatusCode, Json<serde_json::Value>)> {
    let deploy = deploy_service(&ctx)?;
    deploy.reap(body).await.map(Json).map_err(json_error)
}

fn deploy_service(
    ctx: &ControlCtx,
) -> Result<Arc<DeployService>, (StatusCode, Json<serde_json::Value>)> {
    ctx.deploy.clone().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "deploy is disabled on this relay"})),
        )
    })
}

fn json_error(err: DeployError) -> (StatusCode, Json<serde_json::Value>) {
    (
        err.status(),
        Json(serde_json::json!({"error": err.to_string()})),
    )
}

fn chrono_like_id() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

pub async fn client_get_status(control: &str, token: Option<&str>) -> anyhow::Result<String> {
    client_send(
        control,
        reqwest::Method::GET,
        "/status",
        None,
        token,
        Duration::from_secs(3),
    )
    .await
}

pub async fn client_post(
    control: &str,
    path: &str,
    json: Option<serde_json::Value>,
    token: Option<&str>,
) -> anyhow::Result<String> {
    client_send(
        control,
        reqwest::Method::POST,
        path,
        json,
        token,
        Duration::from_secs(3),
    )
    .await
}

pub async fn client_post_timeout(
    control: &str,
    path: &str,
    json: Option<serde_json::Value>,
    token: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<String> {
    client_send(control, reqwest::Method::POST, path, json, token, timeout).await
}

async fn client_send(
    control: &str,
    method: reqwest::Method,
    path: &str,
    json: Option<serde_json::Value>,
    token: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<String> {
    let url = format!("http://{control}{path}");
    let mut req = reqwest::Client::new()
        .request(method, &url)
        .timeout(timeout);
    if let Some(token) = token {
        if !token.is_empty() {
            req = req.header(AUTHORIZATION, format!("Bearer {token}"));
        }
    }
    if let Some(body) = json {
        req = req.json(&body);
    }
    let body = req.send().await?.error_for_status()?.text().await?;
    Ok(body)
}

pub fn control_addr_from_shared_config(shared: &Shared) -> String {
    shared.config.borrow().control.bind.clone()
}
