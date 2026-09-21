use std::sync::Arc;
use std::time::Duration;

use rtmp_rs::media::FlvTag;
use rtmp_rs::protocol::message::{ConnectParams, PlayParams, PublishParams};
use rtmp_rs::server::handler::AuthResult;
use rtmp_rs::session::{SessionContext, StreamContext};
use rtmp_rs::{RtmpHandler, RtmpServer, ServerConfig};
use tracing::{info, warn};

use crate::config::Config;
use crate::state::Shared;

pub struct EchoHandler {
    shared: Shared,
}

impl EchoHandler {
    pub fn new(shared: Shared) -> Self {
        Self { shared }
    }

    fn cfg(&self) -> Config {
        self.shared.config.borrow().clone()
    }

    fn accept_app(&self, app: &str) -> bool {
        let cfg = self.cfg();
        let want = cfg.ingest.app.trim_matches('/');
        let got = app.trim_matches('/');
        got == want
            || got == format!("{want}/{}", cfg.ingest.stream_key.trim_matches('/'))
            || got.starts_with(&format!("{want}/"))
    }

    fn accept_key(&self, key: &str) -> bool {
        let cfg = self.cfg();
        let want = cfg.ingest.stream_key.trim_matches('/');
        let got = key.trim_matches('/');
        got == want || got.ends_with(&format!("/{want}"))
    }

    fn reject_key() -> AuthResult {
        AuthResult::Reject("invalid stream key for echochamber relay".into())
    }
}

impl RtmpHandler for EchoHandler {
    async fn on_connect(&self, _ctx: &SessionContext, params: &ConnectParams) -> AuthResult {
        tracing::debug!(app = %params.app, tc_url = ?params.tc_url, "rtmp connect");
        if self.accept_app(&params.app) {
            AuthResult::Accept
        } else {
            warn!(app = %params.app, "rejecting connect: unknown app");
            AuthResult::Reject("unknown application".into())
        }
    }

    async fn on_fc_publish(&self, _ctx: &SessionContext, stream_key: &str) -> AuthResult {
        if self.accept_key(stream_key) {
            AuthResult::Accept
        } else {
            Self::reject_key()
        }
    }

    async fn on_publish(&self, ctx: &SessionContext, params: &PublishParams) -> AuthResult {
        if !self.accept_key(&params.stream_key) {
            warn!(key = %params.stream_key, "rejecting publish");
            return Self::reject_key();
        }
        info!(app = %ctx.app, key = %params.stream_key, "publisher connected");
        if ctx.app != self.cfg().ingest.app {
            tracing::warn!(
                obs_app = %ctx.app,
                config_app = %self.cfg().ingest.app,
                "encoder RTMP app differs from ingest.app (OBS server URL probably includes the stream key). Pushers will play back using the encoder's app name."
            );
        }
        self.shared.ingest.reset();
        self.shared
            .ingest_keyframes
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.shared.out_meters.lock().expect("out_meters").clear();
        self.shared
            .publisher_session
            .store(ctx.session_id, std::sync::atomic::Ordering::SeqCst);
        self.shared
            .ever_published
            .store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let mut g = self.shared.published.lock().await;
            *g = Some(crate::state::PublishedStream {
                app: ctx.app.clone(),
                stream_key: params.stream_key.clone(),
            });
        }
        // Always notify, even if we were already "live" (OBS stop without unpublish).
        let _ = self.shared.publishing_tx.send(false);
        let _ = self.shared.publishing_tx.send(true);
        AuthResult::Accept
    }

    async fn on_play(&self, ctx: &SessionContext, params: &PlayParams) -> AuthResult {
        tracing::debug!(
            app = %ctx.app,
            play = %params.stream_name,
            "rtmp play"
        );
        AuthResult::Accept
    }

    async fn on_unpublish(&self, ctx: &StreamContext) {
        let pub_id = self
            .shared
            .publisher_session
            .load(std::sync::atomic::Ordering::SeqCst);
        if pub_id != 0 && pub_id == ctx.session.session_id {
            info!("publisher gone");
            self.shared.mark_publisher_gone().await;
        }
    }

    async fn on_disconnect(&self, ctx: &SessionContext) {
        let pub_id = self
            .shared
            .publisher_session
            .load(std::sync::atomic::Ordering::SeqCst);
        if pub_id != 0 && pub_id == ctx.session_id {
            info!("publisher disconnected");
            self.shared.mark_publisher_gone().await;
        }
    }

    async fn on_media_tag(&self, ctx: &StreamContext, tag: &FlvTag) -> bool {
        if ctx.is_publishing {
            self.shared.ingest.add(tag.size() as u64);
            // Sequence headers report as keyframes; wait for a real IDR so the
            // GOP cache drops leftover timestamps from the previous OBS session.
            if tag.is_keyframe() && !tag.is_avc_sequence_header() {
                self.shared
                    .ingest_keyframes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        true
    }
}

pub async fn run_server(shared: Shared) -> anyhow::Result<()> {
    let bind = shared
        .config
        .borrow()
        .ingest
        .bind
        .parse()
        .expect("validated bind");
    let mut cfg = ServerConfig::with_addr(bind);
    cfg.gop_buffer_enabled = true;
    cfg.gop_buffer_max_size = 8 * 1024 * 1024;
    cfg.idle_timeout = Duration::from_secs(3600);
    cfg.connection_timeout = Duration::from_secs(15);

    let handler = EchoHandler::new(shared.clone());
    let server = RtmpServer::new(cfg, handler);
    info!(%bind, "RTMP ingest listening");
    let shutdown = {
        let n = Arc::clone(&shared.shutdown);
        async move {
            n.notified().await;
        }
    };
    server
        .run_until(shutdown)
        .await
        .map_err(|e| anyhow::anyhow!("rtmp server: {e}"))
}
