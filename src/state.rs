use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{watch, Mutex, Notify};

use crate::config::{Config, Destination};
use crate::meter::ByteCounter;

#[derive(Clone)]
pub struct Shared {
    pub config_path: PathBuf,
    pub config: watch::Receiver<Config>,
    pub config_tx: watch::Sender<Config>,
    pub publishing: watch::Receiver<bool>,
    pub publishing_tx: watch::Sender<bool>,
    pub pushers: Arc<Mutex<Vec<PusherStatus>>>,
    pub asr: Arc<Mutex<AsrStatus>>,
    pub adhoc: Arc<Mutex<Vec<Destination>>>,
    pub shutdown: Arc<Notify>,
    pub pusher_stop: Arc<Notify>,
    pub asr_drops: Arc<AtomicU64>,
    pub pusher_epoch: Arc<AtomicU64>,
    pub pusher_live: Arc<AtomicU64>,
    pub pusher_pids: Arc<std::sync::Mutex<HashSet<u32>>>,
    pub publisher_session: Arc<AtomicU64>,
    /// IDR/keyframe count for the current OBS publish session (reset on publish).
    pub ingest_keyframes: Arc<AtomicU64>,
    pub standby_active: Arc<AtomicBool>,
    /// OBS has published at least once this process (for standby.after_first_publish).
    pub ever_published: Arc<AtomicBool>,
    pub ingest: Arc<ByteCounter>,
    pub out_meters: Arc<std::sync::Mutex<HashMap<String, Arc<ByteCounter>>>>,
    /// App + stream name the encoder actually published (OBS often puts
    /// `/live` on the server URL, so app is `echochamber/live` not `echochamber`).
    pub published: Arc<Mutex<Option<PublishedStream>>>,
}

#[derive(Debug, Clone)]
pub struct PublishedStream {
    pub app: String,
    pub stream_key: String,
}

impl Shared {
    pub fn new(config: Config, config_path: PathBuf) -> Self {
        let (config_tx, config_rx) = watch::channel(config);
        let (publishing_tx, publishing_rx) = watch::channel(false);
        Self {
            config_path,
            config: config_rx,
            config_tx,
            publishing: publishing_rx,
            publishing_tx,
            pushers: Arc::new(Mutex::new(Vec::new())),
            asr: Arc::new(Mutex::new(AsrStatus::default())),
            adhoc: Arc::new(Mutex::new(Vec::new())),
            shutdown: Arc::new(Notify::new()),
            pusher_stop: Arc::new(Notify::new()),
            asr_drops: Arc::new(AtomicU64::new(0)),
            publisher_session: Arc::new(AtomicU64::new(0)),
            pusher_epoch: Arc::new(AtomicU64::new(0)),
            pusher_live: Arc::new(AtomicU64::new(0)),
            pusher_pids: Arc::new(std::sync::Mutex::new(HashSet::new())),
            ingest_keyframes: Arc::new(AtomicU64::new(0)),
            standby_active: Arc::new(AtomicBool::new(false)),
            ever_published: Arc::new(AtomicBool::new(false)),
            ingest: Arc::new(ByteCounter::new()),
            out_meters: Arc::new(std::sync::Mutex::new(HashMap::new())),
            published: Arc::new(Mutex::new(None)),
        }
    }

    pub fn register_pid(&self, pid: u32) {
        self.pusher_pids.lock().expect("pusher_pids").insert(pid);
    }

    pub fn unregister_pid(&self, pid: u32) {
        self.pusher_pids.lock().expect("pusher_pids").remove(&pid);
    }

    pub fn kill_pusher_groups(&self) {
        let pids: Vec<u32> = self
            .pusher_pids
            .lock()
            .expect("pusher_pids")
            .iter()
            .copied()
            .collect();
        for pid in pids {
            crate::process::kill_group(pid, libc::SIGKILL);
        }
    }

    pub fn out_meter(&self, name: &str) -> Arc<ByteCounter> {
        let mut g = self.out_meters.lock().expect("out_meters");
        g.entry(name.to_string())
            .or_insert_with(|| Arc::new(ByteCounter::new()))
            .clone()
    }

    pub fn request_stop(&self) {
        self.shutdown.notify_waiters();
        self.pusher_stop.notify_waiters();
        let _ = self.publishing_tx.send(false);
    }

    pub async fn mark_publisher_gone(&self) {
        self.publisher_session.store(0, Ordering::SeqCst);
        {
            let mut g = self.published.lock().await;
            *g = None;
        }
        self.pusher_stop.notify_waiters();
        let _ = self.publishing_tx.send(false);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PusherStatus {
    pub name: String,
    pub url_masked: String,
    pub state: String,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub last_error: Option<String>,
    #[serde(default)]
    pub bytes_out: u64,
    /// Instantaneous outbound bits/sec (~1s window).
    #[serde(default)]
    pub bps: f64,
    #[serde(default)]
    pub bps_avg: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AsrStatus {
    pub enabled: bool,
    pub cues: u64,
    pub last_error: Option<String>,
    pub last_text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusPayload {
    pub ingest: IngestStatus,
    pub pushers: Vec<PusherStatus>,
    pub asr: AsrStatus,
    pub asr_drops: u64,
    pub ffmpeg: FfmpegStatus,
    pub destinations: usize,
    pub bandwidth: BandwidthStatus,
}

#[derive(Debug, Serialize)]
pub struct BandwidthStatus {
    pub window_ms: u64,
    pub ingest_bytes: u64,
    /// OBS → echochamber, bits/sec over ~1s.
    pub ingest_bps: f64,
    pub ingest_bps_avg: f64,
    pub push_bytes: u64,
    /// Sum of pusher output, bits/sec over ~1s.
    pub push_bps: f64,
    pub push_bps_avg: f64,
}

#[derive(Debug, Serialize)]
pub struct IngestStatus {
    pub bind: String,
    pub publishing: bool,
    /// Destinations are looping the local slate (OBS is down).
    pub standby: bool,
    pub play_url: String,
    pub stream_key: String,
}

#[derive(Debug, Serialize)]
pub struct FfmpegStatus {
    pub bin: String,
    pub burn_in_available: bool,
}

pub fn bump_drops(shared: &Shared) {
    shared.asr_drops.fetch_add(1, Ordering::Relaxed);
}

pub fn drops(shared: &Shared) -> u64 {
    shared.asr_drops.load(Ordering::Relaxed)
}

pub async fn run_sampler(shared: Shared) {
    loop {
        tokio::select! {
            _ = shared.shutdown.notified() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                shared.ingest.tick();
                let meters: Vec<Arc<ByteCounter>> = {
                    let g = shared.out_meters.lock().expect("out_meters");
                    g.values().cloned().collect()
                };
                for m in meters {
                    m.tick();
                }
            }
        }
    }
}
