use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{info, warn, Instrument};

use crate::config::{Config, Destination};
use crate::ffmpeg::{standby_tee_args, PusherSpec};
use crate::process::{spawn_grouped, terminate};
use crate::redact::{redact_cmd, redact_url};
use crate::state::{PusherStatus, Shared};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PushMode {
    Live,
    Standby,
}

impl PushMode {
    fn should_run(self, publishing: bool) -> bool {
        match self {
            Self::Live => publishing,
            Self::Standby => !publishing,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Live => "running",
            Self::Standby => "standby",
        }
    }
}

/// Counts in-flight `run_one` tasks so restart can wait for ffmpeg to actually die.
struct LiveGuard(Arc<AtomicU64>);

impl LiveGuard {
    fn new(n: Arc<AtomicU64>) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        Self(n)
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn sidecar_paths() -> (PathBuf, PathBuf) {
    (
        PathBuf::from("echochamber_live.srt"),
        PathBuf::from("echochamber_live.ass"),
    )
}

pub async fn run_supervisor(shared: Shared) {
    let mut publishing = shared.publishing.clone();
    let mut config = shared.config.clone();
    loop {
        apply_pushers(&shared).await;
        tokio::select! {
            _ = shared.shutdown.notified() => {
                shared.pusher_epoch.fetch_add(1, Ordering::SeqCst);
                shared.pusher_stop.notify_waiters();
                shared.standby_active.store(false, Ordering::SeqCst);
                stop_all(&shared).await;
                wait_pushers_dead(&shared).await;
                break;
            }
            _ = publishing.changed() => {}
            _ = config.changed() => {}
        }
    }
}

async fn apply_pushers(shared: &Shared) {
    if *shared.publishing.borrow() {
        restart_pushers(shared, PushMode::Live).await;
        return;
    }
    let cfg = shared.config.borrow().clone();
    let dests = destinations(shared).await;
    let video = cfg.standby.resolve_video(&shared.config_path);
    let waiting_for_first = cfg.standby.after_first_publish
        && !shared.ever_published.load(Ordering::Relaxed);
    // Stop live ffmpeg immediately so dest sockets are not held (and so
    // -c copy cannot reconnect into the next OBS session alongside a new pusher).
    shared.pusher_epoch.fetch_add(1, Ordering::SeqCst);
    shared.pusher_stop.notify_waiters();
    shared.standby_active.store(false, Ordering::SeqCst);
    stop_all(shared).await;
    if dests.is_empty() || !cfg.standby.enabled || waiting_for_first || video.is_none() {
        wait_pushers_dead(shared).await;
        if dests.is_empty() {
            info!("relay mode: no platforms configured, not starting pushers");
        } else if cfg.standby.enabled && video.is_none() {
            warn!(
                video = %cfg.standby.video,
                "standby enabled but video file not found"
            );
        }
        return;
    }
    let delay = Duration::from_secs(cfg.standby.delay_secs);
    if !debounce_standby(shared, delay).await {
        if *shared.publishing.borrow() {
            restart_pushers(shared, PushMode::Live).await;
        } else {
            wait_pushers_dead(shared).await;
        }
        return;
    }
    restart_pushers(shared, PushMode::Standby).await;
}

/// Returns false if OBS came back (or shutdown) before the slate should start.
async fn debounce_standby(shared: &Shared, delay: Duration) -> bool {
    if delay.is_zero() {
        return !*shared.publishing.borrow();
    }
    let mut rx = shared.publishing.clone();
    tokio::select! {
        _ = shared.shutdown.notified() => false,
        _ = async {
            loop {
                if *rx.borrow() {
                    break;
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        } => false,
        _ = tokio::time::sleep(delay) => !*shared.publishing.borrow(),
    }
}

async fn destinations(shared: &Shared) -> Vec<Destination> {
    let cfg = shared.config.borrow().clone();
    let mut dests = cfg.configured_destinations();
    dests.extend(shared.adhoc.lock().await.clone());
    dests
}

async fn restart_pushers(shared: &Shared, mode: PushMode) {
    let epoch = shared.pusher_epoch.fetch_add(1, Ordering::SeqCst) + 1;
    shared.pusher_stop.notify_waiters();
    stop_all(shared).await;
    let dests = destinations(shared).await;
    if dests.is_empty() {
        wait_pushers_dead(shared).await;
        shared.standby_active.store(false, Ordering::SeqCst);
        info!("relay mode: no platforms configured, not starting pushers");
        return;
    }
    match mode {
        PushMode::Live => {
            tokio::join!(wait_pushers_dead(shared), wait_for_ingest_media(shared));
        }
        PushMode::Standby => {
            wait_pushers_dead(shared).await;
        }
    }
    if !mode.should_run(*shared.publishing.borrow()) || !epoch_alive(shared, epoch) {
        return;
    }
    if epoch > 1 {
        tokio::time::sleep(Duration::from_millis(800)).await;
        if !mode.should_run(*shared.publishing.borrow()) || !epoch_alive(shared, epoch) {
            return;
        }
    }
    let cfg = shared.config.borrow().clone();
    let published = shared.published.lock().await.clone();
    let (_, ass) = sidecar_paths();
    let video = cfg.standby.resolve_video(&shared.config_path);
    let audio = cfg.standby.resolve_audio(&shared.config_path);
    if mode == PushMode::Standby && video.is_none() {
        warn!("standby video disappeared before spawn");
        return;
    }
    shared
        .standby_active
        .store(mode == PushMode::Standby, Ordering::SeqCst);
    let statuses: Vec<PusherStatus> = dests
        .iter()
        .map(|dest| PusherStatus {
            name: dest.name.clone(),
            url_masked: redact_url(&dest.url),
            state: "starting".into(),
            pid: None,
            restarts: 0,
            last_error: None,
            bytes_out: 0,
            bps: 0.0,
            bps_avg: 0.0,
        })
        .collect();
    *shared.pushers.lock().await = statuses;
    if mode == PushMode::Standby {
        let names: Vec<String> = dests.iter().map(|d| d.name.clone()).collect();
        let urls: Vec<String> = dests.iter().map(|d| d.url.clone()).collect();
        let span = tracing::info_span!("pusher", platform = "standby");
        tokio::spawn(
            run_standby(
                shared.clone(),
                cfg,
                names,
                urls,
                epoch,
                video,
                audio,
            )
            .instrument(span),
        );
        return;
    }
    for dest in dests {
        let name = dest.name.clone();
        let span = tracing::info_span!("pusher", platform = %name);
        tokio::spawn(
            run_one(
                shared.clone(),
                cfg.clone(),
                dest,
                ass.clone(),
                published.clone(),
                epoch,
            )
            .instrument(span),
        );
    }
}

async fn wait_pushers_dead(shared: &Shared) {
    for i in 0..40 {
        if shared.pusher_live.load(Ordering::SeqCst) == 0 {
            return;
        }
        if i == 8 {
            info!("force-killing previous ffmpeg pushers");
            shared.kill_pusher_groups();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shared.kill_pusher_groups();
    warn!(
        still_live = shared.pusher_live.load(Ordering::SeqCst),
        "timed out waiting for previous ffmpeg pushers to exit"
    );
}

async fn wait_for_ingest_media(shared: &Shared) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if !*shared.publishing.borrow() {
            return;
        }
        if shared.ingest_keyframes.load(Ordering::Relaxed) >= 1 {
            // A few more frames after the IDR so play doesn't start on headers only.
            tokio::time::sleep(Duration::from_millis(200)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    info!("starting pushers without waiting for ingest keyframe (timeout)");
}

async fn stop_all(shared: &Shared) {
    let mut list = shared.pushers.lock().await;
    for p in list.iter_mut() {
        if p.pid.is_some() {
            p.state = "stopping".into();
        } else {
            p.state = "stopped".into();
        }
    }
}

async fn update_status(shared: &Shared, name: &str, f: impl FnOnce(&mut PusherStatus)) {
    let mut list = shared.pushers.lock().await;
    if let Some(p) = list.iter_mut().find(|p| p.name == name) {
        f(p);
    }
}

fn epoch_alive(shared: &Shared, epoch: u64) -> bool {
    shared.pusher_epoch.load(Ordering::SeqCst) == epoch
}

async fn run_one(
    shared: Shared,
    cfg: Config,
    dest: Destination,
    ass: PathBuf,
    published: Option<crate::state::PublishedStream>,
    epoch: u64,
) {
    let mode = PushMode::Live;
    let _live = LiveGuard::new(Arc::clone(&shared.pusher_live));
    let mut backoff = Duration::from_secs(1);
    loop {
        if !epoch_alive(&shared, epoch) || !mode.should_run(*shared.publishing.borrow()) {
            break;
        }

        let spec = PusherSpec::from_published(&cfg, dest.url.clone(), &ass, published.as_ref());
        let args = spec.args();
        info!(
            cmd = %redact_cmd(
                &crate::ffmpeg::ffmpeg_bin().display().to_string(),
                &args
            ),
            "starting pusher"
        );

        match spawn_grouped(&crate::ffmpeg::ffmpeg_bin(), &args) {
            Ok(mut child) => {
                let pid = child.id();
                if let Some(p) = pid {
                    shared.register_pid(p);
                }
                update_status(&shared, &dest.name, |p| {
                    p.state = mode.label().into();
                    p.pid = pid;
                    p.last_error = None;
                })
                .await;
                shared.out_meter(&dest.name).reset();
                info!(?pid, mode = mode.label(), "pusher connected");

                if let Some(stderr) = child.stderr.take() {
                    let name = dest.name.clone();
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            tracing::debug!(platform = %name, "{line}");
                        }
                    });
                }
                if let Some(stdout) = child.stdout.take() {
                    let shared_p = shared.clone();
                    let name = dest.name.clone();
                    tokio::spawn(async move {
                        read_ffmpeg_progress(shared_p, vec![name], stdout).await;
                    });
                }

                let wait = tokio::select! {
                    _ = shared.shutdown.notified() => {
                        terminate(&mut child).await;
                        if let Some(p) = pid {
                            shared.unregister_pid(p);
                        }
                        return;
                    }
                    _ = wait_until_stale(&shared, epoch, mode) => {
                        terminate(&mut child).await;
                        if let Some(p) = pid {
                            shared.unregister_pid(p);
                        }
                        return;
                    }
                    status = child.wait() => status,
                };

                if let Some(p) = pid {
                    shared.unregister_pid(p);
                }
                match wait {
                    Ok(status) => {
                        warn!(%status, "pusher exited");
                        update_status(&shared, &dest.name, |p| {
                            p.state = "restarting".into();
                            p.pid = None;
                            p.restarts += 1;
                            p.last_error = Some(format!("exited {status}"));
                        })
                        .await;
                    }
                    Err(e) => {
                        warn!(error = %e, "pusher wait failed");
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to spawn ffmpeg");
                update_status(&shared, &dest.name, |p| {
                    p.state = "error".into();
                    p.last_error = Some(e.to_string());
                    p.restarts += 1;
                })
                .await;
            }
        }

        if !epoch_alive(&shared, epoch) || !mode.should_run(*shared.publishing.borrow()) {
            break;
        }
        tokio::select! {
            _ = shared.shutdown.notified() => break,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
    update_status(&shared, &dest.name, |p| {
        p.state = "stopped".into();
        p.pid = None;
    })
    .await;
}

async fn run_standby(
    shared: Shared,
    cfg: Config,
    names: Vec<String>,
    urls: Vec<String>,
    epoch: u64,
    video: Option<PathBuf>,
    audio: Option<PathBuf>,
) {
    let mode = PushMode::Standby;
    let _live = LiveGuard::new(Arc::clone(&shared.pusher_live));
    let mut backoff = Duration::from_secs(1);
    loop {
        if !epoch_alive(&shared, epoch) || !mode.should_run(*shared.publishing.borrow()) {
            break;
        }
        let Some(v) = video.as_deref() else {
            warn!("standby pusher missing video");
            break;
        };
        let args = standby_tee_args(&cfg, v, audio.as_deref(), &urls);
        info!(
            cmd = %redact_cmd(
                &crate::ffmpeg::ffmpeg_bin().display().to_string(),
                &args
            ),
            dests = names.len(),
            "starting standby encode"
        );
        match spawn_grouped(&crate::ffmpeg::ffmpeg_bin(), &args) {
            Ok(mut child) => {
                let pid = child.id();
                if let Some(p) = pid {
                    shared.register_pid(p);
                }
                for name in &names {
                    update_status(&shared, name, |p| {
                        p.state = mode.label().into();
                        p.pid = pid;
                        p.last_error = None;
                    })
                    .await;
                    shared.out_meter(name).reset();
                }
                info!(?pid, dests = names.len(), "standby encode connected");
                if let Some(stderr) = child.stderr.take() {
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            tracing::debug!(platform = "standby", "{line}");
                        }
                    });
                }
                if let Some(stdout) = child.stdout.take() {
                    let shared_p = shared.clone();
                    let names_p = names.clone();
                    tokio::spawn(async move {
                        read_ffmpeg_progress(shared_p, names_p, stdout).await;
                    });
                }
                let wait = tokio::select! {
                    _ = shared.shutdown.notified() => {
                        terminate(&mut child).await;
                        if let Some(p) = pid {
                            shared.unregister_pid(p);
                        }
                        return;
                    }
                    _ = wait_until_stale(&shared, epoch, mode) => {
                        terminate(&mut child).await;
                        if let Some(p) = pid {
                            shared.unregister_pid(p);
                        }
                        return;
                    }
                    status = child.wait() => status,
                };
                if let Some(p) = pid {
                    shared.unregister_pid(p);
                }
                if let Ok(status) = &wait {
                    warn!(%status, "standby encode exited");
                }
                for name in &names {
                    update_status(&shared, name, |p| {
                        p.state = "restarting".into();
                        p.pid = None;
                        p.restarts += 1;
                    })
                    .await;
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to spawn standby encode");
            }
        }
        if !epoch_alive(&shared, epoch) || !mode.should_run(*shared.publishing.borrow()) {
            break;
        }
        tokio::select! {
            _ = shared.shutdown.notified() => break,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
    for name in &names {
        update_status(&shared, name, |p| {
            p.state = "stopped".into();
            p.pid = None;
        })
        .await;
    }
}

async fn read_ffmpeg_progress(
    shared: Shared,
    names: Vec<String>,
    stdout: impl tokio::io::AsyncRead + Unpin,
) {
    let n = names.len().max(1) as u64;
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(rest) = line.strip_prefix("total_size=") {
            if let Ok(total) = rest.trim().parse::<u64>() {
                let each = total / n;
                for name in &names {
                    shared.out_meter(name).set(each);
                }
            }
        }
    }
}

async fn wait_until_stale(shared: &Shared, epoch: u64, mode: PushMode) {
    let mut rx = shared.publishing.clone();
    loop {
        if !epoch_alive(shared, epoch) || !mode.should_run(*rx.borrow()) {
            return;
        }
        tokio::select! {
            _ = shared.pusher_stop.notified() => return,
            _ = rx.changed() => {}
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }
    }
}
