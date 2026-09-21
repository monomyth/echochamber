use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::net::TcpListener;
use tracing::info;

use crate::config::Destination;
use crate::ffmpeg::{ffmpeg_bin, ffmpeg_has_burnin_filter};
use crate::redact::redact_key;
use crate::state::{drops, BandwidthStatus, FfmpegStatus, IngestStatus, Shared, StatusPayload};

pub async fn run(shared: Shared) -> anyhow::Result<()> {
    let bind: SocketAddr = shared.config.borrow().control.bind.parse()?;
    let stop = Arc::clone(&shared.shutdown);
    let app = Router::new()
        .route("/status", get(handle_status))
        .route("/reload", post(handle_reload))
        .route("/stop", post(handle_stop))
        .route("/push", post(handle_push))
        .with_state(Arc::new(shared));

    let listener = TcpListener::bind(bind).await?;
    info!(%bind, "control HTTP listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            stop.notified().await;
        })
        .await?;
    Ok(())
}

#[axum::debug_handler]
async fn handle_status(State(shared): State<Arc<Shared>>) -> Json<StatusPayload> {
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
            standby: shared.standby_active.load(std::sync::atomic::Ordering::Relaxed),
            play_url: cfg.local_rtmp_url(),
            stream_key: redact_key(&cfg.ingest.stream_key),
        },
        pushers,
        asr,
        asr_drops: drops(&shared),
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

async fn handle_reload(State(shared): State<Arc<Shared>>) -> Result<String, (StatusCode, String)> {
    let cfg = crate::config::Config::load_from_path(&shared.config_path)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let _ = shared.config_tx.send(cfg);
    info!("config reloaded from {}", shared.config_path.display());
    Ok("reloaded\n".into())
}

async fn handle_stop(State(shared): State<Arc<Shared>>) -> String {
    info!("stop requested");
    shared.request_stop();
    "stopping\n".into()
}

#[derive(Debug, Deserialize)]
struct PushBody {
    url: String,
    name: Option<String>,
}

async fn handle_push(
    State(shared): State<Arc<Shared>>,
    Json(body): Json<PushBody>,
) -> Result<String, (StatusCode, String)> {
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
    // Kick supervisor by toggling the config watch (clone send).
    let cfg = shared.config.borrow().clone();
    let _ = shared.config_tx.send(cfg);
    Ok(format!("added destination {name}\n"))
}

fn chrono_like_id() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

pub async fn client_get_status(control: &str) -> anyhow::Result<String> {
    let url = format!("http://{control}/status");
    let body = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(3))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(body)
}

pub async fn client_post(
    control: &str,
    path: &str,
    json: Option<serde_json::Value>,
) -> anyhow::Result<String> {
    let url = format!("http://{control}{path}");
    let mut req = reqwest::Client::new()
        .post(&url)
        .timeout(Duration::from_secs(3));
    if let Some(j) = json {
        req = req.json(&j);
    }
    let body = req.send().await?.error_for_status()?.text().await?;
    Ok(body)
}

pub fn control_addr_from_shared_config(shared: &Shared) -> String {
    shared.config.borrow().control.bind.clone()
}
