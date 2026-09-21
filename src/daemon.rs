use anyhow::Result;
use tracing::{error, info};

use crate::config::Config;
use crate::ffmpeg::{ffmpeg_bin, ffmpeg_has_burnin_filter, probe_ffmpeg};
use crate::state::Shared;

pub async fn serve(cfg: Config, config_path: std::path::PathBuf) -> Result<()> {
    info!("echochamber starting");
    info!("\n{}", cfg.masked_summary());
    match probe_ffmpeg() {
        Ok(s) => info!(ffmpeg = %s, "ffmpeg probe ok"),
        Err(e) => tracing::warn!(error = %e, "ffmpeg probe failed"),
    }
    if cfg.subtitles.burn_in && !ffmpeg_has_burnin_filter() {
        tracing::warn!(
            bin = %ffmpeg_bin().display(),
            "burn_in=true but this ffmpeg has no ass/subtitles filter; pushers will fail until you point ECHOCHAMBER_FFMPEG_BIN at a libass build (or set burn_in=false)"
        );
    }

    let shared = Shared::new(cfg, config_path);

    let control = {
        let s = shared.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::control::run(s).await {
                error!(error = %e, "control server");
            }
        })
    };
    let pushers = {
        let s = shared.clone();
        tokio::spawn(async move {
            crate::pusher::run_supervisor(s).await;
        })
    };
    let asr = {
        let s = shared.clone();
        tokio::spawn(async move {
            crate::asr::run(s).await;
        })
    };
    let sampler = {
        let s = shared.clone();
        tokio::spawn(async move {
            crate::state::run_sampler(s).await;
        })
    };

    let rtmp = crate::rtmp::run_server(shared.clone());

    tokio::select! {
        r = rtmp => {
            if let Err(e) = r {
                error!(error = %e, "rtmp");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("SIGINT");
            shared.request_stop();
        }
    }

    shared.request_stop();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(6), async {
        let _ = control.await;
        let _ = pushers.await;
        let _ = asr.await;
        let _ = sampler.await;
    })
    .await;
    info!("echochamber stopped");
    Ok(())
}
